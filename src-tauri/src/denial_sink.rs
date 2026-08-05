//! Where a refused outbound request is written down.
//!
//! K5's acceptance asks that "every blocked attempt is a ledger event with the
//! rule that blocked it". [`crate::egress::EgressRule`] supplied the rule; this is
//! the record. It is deliberately **not** part of the run ledger, and the three
//! reasons are the whole design.
//!
//! # Why not `run_events`
//!
//! `run_events.run_id` is `NOT NULL REFERENCES runs(run_id)`, behind a trigger
//! that demands a gapless `sequence = last_sequence + 1`, refuses any event after
//! a terminal one, and caps the total. A denial fits none of that: it can come
//! from work with no run at all — `web.rs`'s fetch guard runs inside a tool call,
//! `browser_worker.rs`'s runs at Chromium launch — and one arriving after a run
//! ended would be rejected outright by a trigger whose whole job is to keep a
//! run's event stream honest. Widening those invariants to admit denials would
//! damage the thing they protect.
//!
//! # Why its own file, and not one more table in the same database
//!
//! This is the part that decided the shape. `run_ledger.rs`'s `apply_migrations`
//! refuses to open a database whose `MAX(version)` exceeds the version the binary
//! knows:
//!
//! ```text
//! if version > MIGRATION_V7 { return Err(MigrationConflict { version }) }
//! ```
//!
//! That guard is right — an older binary must not write into a schema it does not
//! understand — but it makes a schema bump a **one-way door for the whole
//! ledger**. Adding a denials table as `MIGRATION_V8` would mean that a user who
//! rolls back to the previous build, which the in-app updater makes an ordinary
//! thing to do, gets a binary that cannot open its own run history at all. Not a
//! degraded feature: no runs, no events, no approvals.
//!
//! So the choice was never "tolerate a newer database or not" — relaxing that
//! guard to ship one observability table would trade a real invariant for a
//! convenience. A separate file removes the question: the ledger stays at V7, an
//! older binary opens it exactly as before, and it simply never looks at this
//! file. `profile-v1.sqlite3` already carries a version in its name, and the
//! daemon store is already a second database with its own schema, so neither the
//! extra file nor the convention is new.
//!
//! # Why recording is fail-soft
//!
//! A denial is written down *after* the refusal has already happened. If this
//! sink cannot be opened or cannot be written, the enforcement still stands and
//! the only loss is a log line — whereas propagating the error would turn an
//! observability failure into a new way for requests to fail. That trade only
//! works in this direction: a sink error must never be allowed to *unblock*
//! anything, and it cannot here, because nothing consults the sink to decide.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use rusqlite::{Connection, OptionalExtension};

use crate::egress::EgressDenial;

/// Filename under the app data directory.
///
/// `-v1` for the same reason `profile-v1.sqlite3` has it: if this schema ever
/// changes incompatibly, a new file is a cleaner answer than a migration whose
/// failure mode is an unopenable database.
pub const SINK_FILE: &str = "egress-denials-v1.sqlite3";

/// Rows kept. Oldest beyond this are dropped by a trigger on insert.
///
/// Bounded because the volume is **attacker-influenced**: a page under
/// `browser_worker.rs`'s guard can request as many refused subresources as it
/// likes, and each one is a denial. An unbounded audit table whose row count a
/// remote page controls is a disk-exhaustion primitive, not an audit trail. Ten
/// thousand is enough to see a pattern and small enough to be irrelevant on disk.
const MAX_ROWS: i64 = 10_000;

/// Schema version. Its own counter, unrelated to the run ledger's.
const SINK_V1: i64 = 1;

/// Checksum of the V1 statements, in the run ledger's spirit: a schema edited in
/// place rather than added as a new version is a mistake worth failing on.
const SINK_V1_CHECKSUM: &str = "egress-denials-v1";

const SINK_V1_SQL: &str = "CREATE TABLE IF NOT EXISTS egress_denials (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        recorded_at_ms INTEGER NOT NULL CHECK (recorded_at_ms > 0),
        -- `EgressRule::code`, not its variant name: the code is the identity
        -- that is meant to survive a rename, which is what makes a row written
        -- by an older build still comparable with one written by a newer.
        rule_code TEXT NOT NULL CHECK (length(rule_code) > 0),
        -- Which guard refused, so two guards disagreeing about the same address
        -- class stays visible rather than averaging out.
        guard TEXT NOT NULL CHECK (length(guard) > 0),
        -- Free text from `EgressDenial::detail`. Never a URL with userinfo: the
        -- one rule whose target is a secret carries no detail at all, which
        -- `EgressRule::redacts_target` is what enforces.
        detail TEXT,
        -- A plain nullable column and deliberately NOT a foreign key. Making it
        -- one is exactly what `run_events` does, and it is why `run_events`
        -- cannot host these rows.
        run_id TEXT
     ) STRICT;
     CREATE INDEX IF NOT EXISTS egress_denials_recent
        ON egress_denials (recorded_at_ms DESC);
     CREATE INDEX IF NOT EXISTS egress_denials_by_rule
        ON egress_denials (rule_code);";

/// One recorded refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialRecord {
    pub recorded_at_ms: i64,
    pub rule_code: String,
    pub guard: String,
    pub detail: Option<String>,
    pub run_id: Option<String>,
}

/// The append-only store.
pub struct DenialSink {
    connection: Connection,
}

impl DenialSink {
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> rusqlite::Result<Self> {
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA trusted_schema = OFF;",
        )?;
        // `NORMAL` rather than the ledger's `FULL`: losing the last few denial
        // rows to a power cut costs a log line, where losing a run transition
        // costs correctness. Different data, different durability.
        let sink = Self { connection };
        sink.apply_migrations()?;
        Ok(sink)
    }

    fn apply_migrations(&self) -> rusqlite::Result<()> {
        self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS sink_migrations (
                version INTEGER PRIMARY KEY,
                checksum TEXT NOT NULL,
                applied_at_ms INTEGER NOT NULL
             ) STRICT;",
        )?;

        // The same forward-only guard the run ledger has, and the same reasoning —
        // but note what is different about the *consequence* here. If a rolled-back
        // build meets a newer sink, it declines to record and everything else keeps
        // working, because nothing but this module reads this file. That containment
        // is the point of the separate database, not a happy accident of it.
        if let Some(version) =
            self.connection
                .query_row("SELECT MAX(version) FROM sink_migrations", [], |row| {
                    row.get::<_, Option<i64>>(0)
                })?
        {
            if version > SINK_V1 {
                return Err(rusqlite::Error::InvalidQuery);
            }
        }

        if let Some(checksum) = self
            .connection
            .query_row(
                "SELECT checksum FROM sink_migrations WHERE version = ?1",
                [SINK_V1],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            if checksum != SINK_V1_CHECKSUM {
                return Err(rusqlite::Error::InvalidQuery);
            }
            return Ok(());
        }

        self.connection.execute_batch(SINK_V1_SQL)?;
        self.connection.execute(
            "INSERT INTO sink_migrations (version, checksum, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![SINK_V1, SINK_V1_CHECKSUM, 1_i64],
        )?;
        Ok(())
    }

    /// Appends one denial and enforces [`MAX_ROWS`].
    ///
    /// The prune is in the same statement batch as the insert, not a background
    /// task, because the bound has to hold against a caller producing denials in a
    /// loop — which is precisely the case it exists for.
    pub fn record(&self, record: &DenialRecord) -> rusqlite::Result<()> {
        self.connection.execute(
            "INSERT INTO egress_denials
                (recorded_at_ms, rule_code, guard, detail, run_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                record.recorded_at_ms,
                record.rule_code,
                record.guard,
                record.detail,
                record.run_id,
            ],
        )?;
        self.connection.execute(
            "DELETE FROM egress_denials
             WHERE id <= (
                SELECT MAX(id) - ?1 FROM egress_denials
             )",
            [MAX_ROWS],
        )?;
        Ok(())
    }

    /// Most recent denials first.
    pub fn recent(&self, limit: usize) -> rusqlite::Result<Vec<DenialRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT recorded_at_ms, rule_code, guard, detail, run_id
             FROM egress_denials
             ORDER BY id DESC
             LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(DenialRecord {
                recorded_at_ms: row.get(0)?,
                rule_code: row.get(1)?,
                guard: row.get(2)?,
                detail: row.get(3)?,
                run_id: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn count(&self) -> rusqlite::Result<i64> {
        self.connection
            .query_row("SELECT COUNT(*) FROM egress_denials", [], |row| row.get(0))
    }
}

/// The process-wide sink, or `None` before one is installed.
///
/// A global rather than a field threaded through the guards, and the reason is
/// specific to what these call sites look like. `validate_fetch_url` is a pure
/// function of a `Url`; `classify_ip` is a pure function of an `IpAddr`. Neither
/// has, or should have, a handle to application state — and there are zero
/// `task_local!` declarations in this crate to carry one implicitly. Threading a
/// store into them would mean changing every signature between the command layer
/// and the predicate, which is how the identity got lost at the boundary in the
/// first place: every one of those layers currently flattens the denial to a
/// `String`.
///
/// So the recorder goes to the refusal instead of the refusal coming to the
/// recorder. What makes that acceptable rather than merely convenient is that the
/// sink is append-only and no decision reads it, so a global here cannot change
/// what any guard allows.
static SINK: OnceLock<Mutex<Option<DenialSink>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<DenialSink>> {
    SINK.get_or_init(|| Mutex::new(None))
}

/// Serializes the tests that install a sink, wherever in the crate they live.
///
/// Necessary because [`install`] replaces a **process-wide** slot: two tests
/// running concurrently, each installing its own file-backed sink and then reading
/// it back, will have one's records land in the other's sink. Filtering reads by a
/// unique marker is not enough — that guards against mixing up *contents*, not
/// against the slot being swapped between the write and the read. Found the honest
/// way: the web recording test began failing `left: 0, right: 1` the moment a third
/// installing test was added.
///
/// A poisoned lock is recovered rather than propagated, so one failing test does
/// not cascade into every other test that touches the sink.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Installs the process's sink, replacing any previous one.
pub fn install(sink: DenialSink) {
    if let Ok(mut slot) = slot().lock() {
        *slot = Some(sink);
    }
}

/// Whether a sink is installed. Used by diagnostics, and by tests that must not
/// assert on a sink another test installed.
#[must_use]
pub fn is_installed() -> bool {
    slot().lock().is_ok_and(|slot| slot.is_some())
}

/// Writes a denial down if a sink is installed, and does nothing if not.
///
/// Every failure here is swallowed on purpose — see this module's doc for why an
/// observability write must not become a request failure. `guard` is the module
/// that refused, so two guards that disagree about the same address class stay
/// distinguishable.
pub fn record(guard: &'static str, denial: &EgressDenial, run_id: Option<&str>) {
    let recorded_at_ms = match std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()))
    {
        Ok(Ok(millis)) if millis > 0 => millis,
        // A clock before the epoch, or past year 292 million. The `CHECK` would
        // refuse the row anyway; dropping it here keeps the reason local.
        _ => return,
    };

    let record = DenialRecord {
        recorded_at_ms,
        rule_code: denial.rule().code().to_string(),
        guard: guard.to_string(),
        detail: denial.detail().map(str::to_string),
        run_id: run_id.map(str::to_string),
    };

    if let Ok(slot) = slot().lock() {
        if let Some(sink) = slot.as_ref() {
            let _ = sink.record(&record);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::EgressRule;

    fn record_of(rule: EgressRule, detail: &str) -> DenialRecord {
        DenialRecord {
            recorded_at_ms: 1_700_000_000_000,
            rule_code: rule.code().to_string(),
            guard: "test".to_string(),
            detail: Some(detail.to_string()),
            run_id: None,
        }
    }

    #[test]
    fn a_denial_round_trips_with_its_rule_code_intact() {
        let sink = DenialSink::open_in_memory().expect("sink opens");
        sink.record(&record_of(EgressRule::Loopback, "127.0.0.1"))
            .expect("records");

        let rows = sink.recent(10).expect("reads");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].rule_code, "egress.loopback");
        assert_eq!(rows[0].detail.as_deref(), Some("127.0.0.1"));
        assert_eq!(rows[0].run_id, None);
    }

    /// The column that `run_events` could not offer. A denial from work with no run
    /// is the common case, not the exception.
    #[test]
    fn a_denial_with_no_run_is_storable_at_all() {
        let sink = DenialSink::open_in_memory().expect("sink opens");
        let mut with_run = record_of(EgressRule::PrivateV4, "10.0.0.1");
        with_run.run_id = Some("run-1".to_string());

        sink.record(&record_of(EgressRule::PrivateV4, "10.0.0.2"))
            .expect("a runless denial records");
        sink.record(&with_run).expect("a run-scoped denial records");

        let rows = sink.recent(10).expect("reads");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].run_id.as_deref(), Some("run-1"));
        assert_eq!(rows[1].run_id, None);
    }

    /// The bound exists because a remote page controls how many denials it can
    /// cause. Driven past the cap rather than asserted at it.
    #[test]
    fn the_row_count_is_bounded_however_many_denials_arrive() {
        let sink = DenialSink::open_in_memory().expect("sink opens");
        let overshoot = MAX_ROWS + 250;
        for index in 0..overshoot {
            let mut record = record_of(EgressRule::Multicast, "224.0.0.1");
            record.detail = Some(format!("denial {index}"));
            sink.record(&record).expect("records");
        }

        assert_eq!(
            sink.count().expect("counts"),
            MAX_ROWS,
            "the sink must be bounded, not merely large"
        );
        // The newest survived and the oldest did not — a bound that kept the wrong
        // end would pass a count assertion alone.
        let rows = sink.recent(1).expect("reads");
        assert_eq!(
            rows[0].detail.as_deref(),
            Some(format!("denial {}", overshoot - 1).as_str())
        );
    }

    /// A sink from the future is declined, and the containment is the point: this
    /// returns an error from *this module only*, where the same situation in the run
    /// ledger makes the entire run history unopenable.
    #[test]
    fn a_sink_written_by_a_newer_build_is_declined_rather_than_guessed_at() {
        let connection = Connection::open_in_memory().expect("opens");
        connection
            .execute_batch(
                "CREATE TABLE sink_migrations (
                    version INTEGER PRIMARY KEY,
                    checksum TEXT NOT NULL,
                    applied_at_ms INTEGER NOT NULL
                 ) STRICT;
                 INSERT INTO sink_migrations VALUES (99, 'from-the-future', 1);",
            )
            .expect("seeds a newer schema");

        assert!(
            DenialSink::from_connection(connection).is_err(),
            "a newer schema must not be written into by this build"
        );
    }

    /// An in-place edit of a shipped migration is a mistake, not a new version.
    #[test]
    fn a_tampered_checksum_is_refused() {
        let connection = Connection::open_in_memory().expect("opens");
        connection
            .execute_batch(
                "CREATE TABLE sink_migrations (
                    version INTEGER PRIMARY KEY,
                    checksum TEXT NOT NULL,
                    applied_at_ms INTEGER NOT NULL
                 ) STRICT;
                 INSERT INTO sink_migrations VALUES (1, 'not-the-shipped-checksum', 1);",
            )
            .expect("seeds a tampered row");

        assert!(DenialSink::from_connection(connection).is_err());
    }

    #[test]
    fn opening_the_same_sink_twice_is_idempotent() {
        let directory = std::env::temp_dir().join(format!("lm-sink-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("creates");
        let path = directory.join(SINK_FILE);

        {
            let sink = DenialSink::open(&path).expect("first open");
            sink.record(&record_of(EgressRule::Loopback, "::1"))
                .expect("records");
        }
        let reopened = DenialSink::open(&path).expect("second open must not re-migrate");
        assert_eq!(reopened.count().expect("counts"), 1);

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Recording with nothing installed must be a no-op and not a panic: every
    /// guard calls this unconditionally, including in unit tests that never install
    /// a sink.
    #[test]
    fn recording_without_an_installed_sink_does_nothing() {
        let denial = EgressDenial::about(EgressRule::Loopback, "127.0.0.1");
        record("test-guard", &denial, None);
    }
}
