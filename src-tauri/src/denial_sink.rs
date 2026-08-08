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
//! ## That constraint has since been lifted, and this file has not moved
//!
//! The paragraphs above were right about the ledger as it stood, and the
//! reasoning is kept because it is what justified this file. What changed is the
//! premise: `run_ledger`'s migration V13 records, per migration, whether it
//! actually rejects an older binary's writes, and the forward-only guard now
//! refuses only up to the newest *breaking* migration rather than the newest
//! migration. An additive table no longer costs a rollback anything, which is
//! precisely the one-way door this file was built to route around.
//!
//! This file stays where it is anyway, for a reason that outlived the original
//! one: **the volume here is attacker-influenced.** See [`MAX_ROWS`] — a remote
//! page can cause denials as fast as it can request subresources, and the ring
//! buffer that bounds them would be a poor neighbour for a hash-chained,
//! strictly append-only stream that must never drop a row. Moving these rows
//! into `subsystem_events` would mean either giving that stream an eviction
//! policy or letting a remote party grow it without limit; neither is
//! acceptable, and the separate file is the honest place for a bounded log.
//!
//! What the ledger *should* gain is the other half — an **allowed** egress
//! produces no row anywhere today — and that belongs in the ledger precisely
//! because its volume is the app's own, not a remote party's.
//!
//! # Why a new refusal kind needs no migration
//!
//! Worth stating, because the instinct on being handed four new rules — K5's per-run
//! allowlist added `egress.run-host-not-allowlisted` and three siblings — is to reach
//! for the ladder at [`SINK_MIGRATIONS`]. It is the wrong instinct here. `rule_code`
//! is `TEXT` and the rule *codes* are the vocabulary, so a new rule is new data in an
//! existing column, not a new shape. The ladder is for a new **fact** about a
//! denial, which is what V2 added (`unattributed_reason` — a thing the row could not
//! previously express at all). A column per rule family, or a table per guard, would
//! buy nothing and cost the one-way-door problem this file exists to avoid.
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

/// Characters kept of a denial's detail. Longer details are truncated on insert.
///
/// [`MAX_ROWS`] bounds how many rows a remote party can cause; on its own that is
/// half a bound, because it says nothing about how large one row may be. Details are
/// remote-derived by design — a refused host, a rejected scheme, an unparseable link
/// — and the values behind them are only bounded by whatever the caller's own limit
/// is, which for a Hugging Face model card is 16 MiB. Ten thousand rows of that is
/// the same disk-exhaustion primitive `MAX_ROWS` exists to refuse.
///
/// Enforced **here** rather than at the call sites, because here is the one place all
/// four guards route through: a rule applied per call site is a rule that the next
/// guard forgets. 160 characters is what `model_sources::license_title` already keeps
/// of remote text, and it is long enough to recognise the offending value.
const MAX_DETAIL_CHARS: usize = 160;

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

const SINK_V2: i64 = 2;

/// Latest version this build understands. The forward-only guard compares against
/// this rather than against a specific version, so adding V3 needs no edit there.
const SINK_LATEST: i64 = SINK_V2;

const SINK_V2_CHECKSUM: &str = "egress-denials-v2";

/// `run_id` alone could not tell "background work" from "we lost the identity" —
/// both were `NULL`. `run_scope::Unattributed` gives the first case a name, and this
/// is where the name is kept.
///
/// Added by `ALTER` rather than by rebuilding the table, which costs the ability to
/// express "exactly one of these two is set" as a SQL `CHECK` (SQLite cannot add a
/// constraint to an existing table). No loss worth rebuilding for: the pair is
/// derived from a `RunScope`, and that type is an enum, so the invariant holds by
/// construction one layer up rather than being re-checked here.
const SINK_V2_SQL: &str = "ALTER TABLE egress_denials ADD COLUMN unattributed_reason TEXT;";

/// Every migration in order, so applying them is a loop rather than a stanza per
/// version.
///
/// Each entry keeps its own checksum, and the check is per version: editing V1's SQL
/// in place still fails, which is the property the single-version form had and the
/// reason it is preserved here rather than replaced by "compare the latest".
const SINK_MIGRATIONS: &[(i64, &str, &str)] = &[
    (SINK_V1, SINK_V1_CHECKSUM, SINK_V1_SQL),
    (SINK_V2, SINK_V2_CHECKSUM, SINK_V2_SQL),
];

/// One recorded refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialRecord {
    pub recorded_at_ms: i64,
    pub rule_code: String,
    pub guard: String,
    pub detail: Option<String>,
    pub run_id: Option<String>,
    /// `run_scope::Unattributed::code` when the work deliberately had no run.
    ///
    /// Mutually exclusive with `run_id` because both are derived from one
    /// [`crate::run_scope::RunScope`], whose two arms cannot both hold. Both `None`
    /// is the third, honest state: a site nothing has scoped yet.
    pub unattributed_reason: Option<String>,
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
            if version > SINK_LATEST {
                return Err(rusqlite::Error::InvalidQuery);
            }
        }

        for &(version, checksum, sql) in SINK_MIGRATIONS {
            if let Some(recorded) = self
                .connection
                .query_row(
                    "SELECT checksum FROM sink_migrations WHERE version = ?1",
                    [version],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            {
                // Already applied. Still checked rather than skipped: a schema
                // edited in place instead of added as a new version is the mistake
                // worth failing on, and only comparing the recorded checksum can
                // see it.
                if recorded != checksum {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                continue;
            }

            self.connection.execute_batch(sql)?;
            self.connection.execute(
                "INSERT INTO sink_migrations (version, checksum, applied_at_ms)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![version, checksum, 1_i64],
            )?;
        }
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
                (recorded_at_ms, rule_code, guard, detail, run_id, unattributed_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                record.recorded_at_ms,
                record.rule_code,
                record.guard,
                record.detail,
                record.run_id,
                record.unattributed_reason,
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
            "SELECT recorded_at_ms, rule_code, guard, detail, run_id, unattributed_reason
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
                unattributed_reason: row.get(5)?,
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

/// Serializes the tests that install a sink — or `egress`'s per-run policy source,
/// which is a process-wide slot with the same hazard — wherever in the crate they
/// live.
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
///
/// # Where the identity comes from
///
/// `run_id` is the explicit answer, and it wins whenever it is `Some`: a caller
/// holding the id knows better than an ambient value, and keeping that precedence
/// makes this change a no-op at the sites that already pass one.
///
/// When it is `None`, [`crate::run_scope::current`] is consulted. That is the whole
/// point of the task-local — this module's own doc used to say "there are zero
/// `task_local!` declarations in this crate to carry one implicitly", and the
/// refusals it describes are raised by pure functions of a `Url` or an `IpAddr` that
/// will never hold a run id. Now they do not have to: the scope set at a command
/// boundary reaches them without a single intervening signature changing.
///
/// Three outcomes, and they are deliberately three rather than two:
///
/// - a run id, explicit or ambient;
/// - no run id and a *reason*, when the scope says the work is unattributed;
/// - neither, at a site nothing has scoped yet.
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

    let (run_id, unattributed_reason) = match (run_id, crate::run_scope::current()) {
        (Some(explicit), _) => (Some(explicit.to_string()), None),
        (None, Some(crate::run_scope::RunScope::Run(id))) => (Some(id), None),
        (None, Some(crate::run_scope::RunScope::Unattributed(reason))) => {
            (None, Some(reason.code().to_string()))
        }
        (None, None) => (None, None),
    };

    let record = DenialRecord {
        recorded_at_ms,
        rule_code: denial.rule().code().to_string(),
        guard: guard.to_string(),
        // Truncated by `chars` and not by bytes, so a multi-byte detail cannot be cut
        // mid-character into something that is no longer a `str`.
        detail: denial
            .detail()
            .map(|detail| detail.chars().take(MAX_DETAIL_CHARS).collect()),
        run_id,
        unattributed_reason,
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
    use crate::run_scope::{self, RunScope, Unattributed};

    fn record_of(rule: EgressRule, detail: &str) -> DenialRecord {
        DenialRecord {
            recorded_at_ms: 1_700_000_000_000,
            rule_code: rule.code().to_string(),
            guard: "test".to_string(),
            detail: Some(detail.to_string()),
            run_id: None,
            unattributed_reason: None,
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

    /// A database left at V1 by an older build must upgrade **and keep its rows**.
    ///
    /// The interesting failure is not "the column is missing" — that shows up
    /// immediately — it is a migration that recreates the table and silently drops
    /// what was already recorded. So a row is written before the upgrade and read
    /// back after it.
    #[test]
    fn a_v1_database_upgrades_in_place_without_losing_rows() {
        let directory = std::env::temp_dir().join(format!("lm-sink-v1-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("creates");
        let path = directory.join(SINK_FILE);

        // Stand up exactly what a V1-era build would have left behind: the V1
        // schema, its migration row, and one denial.
        {
            let connection = Connection::open(&path).expect("opens");
            connection
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS sink_migrations (
                        version INTEGER PRIMARY KEY,
                        checksum TEXT NOT NULL,
                        applied_at_ms INTEGER NOT NULL
                     ) STRICT;",
                )
                .expect("migration table");
            connection.execute_batch(SINK_V1_SQL).expect("v1 schema");
            connection
                .execute(
                    "INSERT INTO sink_migrations (version, checksum, applied_at_ms)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![SINK_V1, SINK_V1_CHECKSUM, 1_i64],
                )
                .expect("v1 migration row");
            connection
                .execute(
                    "INSERT INTO egress_denials
                        (recorded_at_ms, rule_code, guard, detail, run_id)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        1_700_000_000_000_i64,
                        "egress.loopback",
                        "old",
                        "::1",
                        "run-old"
                    ],
                )
                .expect("pre-existing row");
        }

        let upgraded = DenialSink::open(&path).expect("a V1 database must upgrade");
        let rows = upgraded.recent(10).expect("reads");
        assert_eq!(rows.len(), 1, "the pre-existing row must survive");
        assert_eq!(rows[0].run_id.as_deref(), Some("run-old"));
        assert_eq!(
            rows[0].unattributed_reason, None,
            "a row written before the column existed reads as neither attributed \
             nor deliberately unattributed, which is exactly what it was"
        );

        // And the new column is usable afterwards.
        let mut scheduled = record_of(EgressRule::Loopback, "127.0.0.1");
        scheduled.unattributed_reason = Some(Unattributed::Scheduled.code().to_string());
        upgraded.record(&scheduled).expect("records post-upgrade");
        let rows = upgraded.recent(10).expect("reads");
        assert_eq!(
            rows[0].unattributed_reason.as_deref(),
            Some("unattributed.scheduled")
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// [`MAX_ROWS`] alone is half a bound: it says how many rows a remote party can
    /// cause and nothing about how large one row may be. Details are remote-derived by
    /// design — a refused host, a rejected scheme, an unparseable link — and bounded
    /// only by the caller's own limit, which for a Hugging Face model card is 16 MiB.
    ///
    /// Asserted at [`record`] rather than at a call site on purpose, because that is
    /// the claim: **every** guard is bounded, including the ones that predate this and
    /// the ones not written yet. `classify_public_https_url` reaching the sink with a
    /// megabyte-long scheme is the concrete case — no call site truncates that, and
    /// none has to.
    #[test]
    fn a_remote_detail_cannot_decide_how_large_an_audit_row_is() {
        let _guard = test_lock();
        install(DenialSink::open_in_memory().expect("sink opens"));

        let huge = "z".repeat(MAX_DETAIL_CHARS * 50);
        record(
            "bound.test",
            &EgressDenial::about(EgressRule::SchemeNotAllowed, huge.clone()),
            None,
        );
        let rows = slot()
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|sink| sink.recent(8).expect("reads")))
            .expect("a sink is installed");
        let stored = rows
            .iter()
            .find(|row| row.guard == "bound.test")
            .and_then(|row| row.detail.as_deref())
            .expect("the row was written");
        assert_eq!(stored.chars().count(), MAX_DETAIL_CHARS);
        assert!(
            huge.starts_with(stored),
            "truncation must keep the front of the detail, not rewrite it"
        );

        // Counter-test, so that truncating everything to nothing — or padding every
        // detail to the cap — cannot pass the assertions above. A detail shorter than
        // the cap has to arrive whole.
        record(
            "bound.test.short",
            &EgressDenial::about(EgressRule::Loopback, "127.0.0.1"),
            None,
        );
        let rows = slot()
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|sink| sink.recent(8).expect("reads")))
            .expect("a sink is installed");
        assert_eq!(
            rows.iter()
                .find(|row| row.guard == "bound.test.short")
                .and_then(|row| row.detail.as_deref()),
            Some("127.0.0.1")
        );
    }

    /// The D3 acceptance clause at the integration level: a scope set at a command
    /// boundary reaches a refusal several frames down, with **no** run id passed to
    /// `record` and no signature in between carrying one.
    ///
    /// Both arms are asserted, because the point of the mechanism is that they are
    /// different answers rather than one blank. The third state — nothing scoped —
    /// is asserted too, since a fallback that quietly invented an identity would
    /// pass the first two.
    #[test]
    fn a_scope_at_the_boundary_reaches_a_refusal_that_was_never_handed_one() {
        let _guard = test_lock();
        let sink = DenialSink::open_in_memory().expect("sink opens");
        install(sink);

        // Stands in for the frames between a command and an SSRF predicate: it
        // takes no run id, exactly like `validate_fetch_url` and `classify_ip`.
        fn refuse_somewhere_deep(marker: &str) {
            record(
                "d3.test",
                &EgressDenial::about(EgressRule::Loopback, marker.to_string()),
                None,
            );
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime builds");
        runtime.block_on(async {
            run_scope::scoped(RunScope::run("run:d3"), async {
                refuse_somewhere_deep("attributed");
            })
            .await;

            run_scope::scoped(RunScope::Unattributed(Unattributed::Scheduled), async {
                refuse_somewhere_deep("background");
            })
            .await;
        });
        // Outside every scope, and outside the runtime entirely.
        refuse_somewhere_deep("uninstrumented");

        let rows = if let Ok(slot) = slot().lock() {
            slot.as_ref()
                .expect("sink installed")
                .recent(10)
                .expect("reads")
        } else {
            panic!("sink lock poisoned");
        };
        let find = |marker: &str| {
            rows.iter()
                .find(|row| row.detail.as_deref() == Some(marker))
                .unwrap_or_else(|| panic!("no row for {marker}"))
        };

        let attributed = find("attributed");
        assert_eq!(
            attributed.run_id.as_deref(),
            Some("run:d3"),
            "the run id must arrive without being threaded"
        );
        assert_eq!(attributed.unattributed_reason, None);

        let background = find("background");
        assert_eq!(background.run_id, None);
        assert_eq!(
            background.unattributed_reason.as_deref(),
            Some("unattributed.scheduled"),
            "deliberately runless work must record why, not a blank"
        );

        let uninstrumented = find("uninstrumented");
        assert_eq!(uninstrumented.run_id, None);
        assert_eq!(
            uninstrumented.unattributed_reason, None,
            "an unscoped site must stay distinguishable from a deliberate one"
        );
    }

    /// An explicitly passed run id wins over the ambient scope.
    ///
    /// Pinned because it is what makes this change a no-op at the sites that already
    /// pass one, and because the opposite precedence would let an outer scope
    /// silently relabel a refusal whose owner the caller already knew.
    #[test]
    fn an_explicit_run_id_beats_the_ambient_scope() {
        let _guard = test_lock();
        let sink = DenialSink::open_in_memory().expect("sink opens");
        install(sink);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime builds");
        runtime.block_on(async {
            run_scope::scoped(RunScope::run("run:ambient"), async {
                record(
                    "d3.test",
                    &EgressDenial::about(EgressRule::Loopback, "explicit-wins".to_string()),
                    Some("run:explicit"),
                );
            })
            .await;
        });

        let rows = if let Ok(slot) = slot().lock() {
            slot.as_ref()
                .expect("sink installed")
                .recent(10)
                .expect("reads")
        } else {
            panic!("sink lock poisoned");
        };
        let row = rows
            .iter()
            .find(|row| row.detail.as_deref() == Some("explicit-wins"))
            .expect("row recorded");
        assert_eq!(row.run_id.as_deref(), Some("run:explicit"));
    }

    /// K5's fourth clause for its newest guard: a per-run allowlist refusal is a
    /// row, with the rule that blocked it and the run it belonged to.
    ///
    /// Driven through `egress::check_run_allowlist` rather than by handing [`record`]
    /// a row, because the claim is about the *guard* reaching the sink — the rule code
    /// and the run id are both things nothing on that path is handed explicitly.
    #[test]
    fn a_per_run_allowlist_refusal_is_recorded_with_its_rule_and_its_run() {
        let _guard = test_lock();
        install(DenialSink::open_in_memory().expect("sink opens"));
        crate::egress::install_run_policy_source(|_| {
            crate::egress::RunEgressPolicy::Declared(std::sync::Arc::new(
                crate::run_protocol::EgressAllowlist::default(),
            ))
        });

        let url = url::Url::parse("https://api.example.com/v1").expect("parses");
        let denial = run_scope::scoped_sync(RunScope::run("run:sink"), || {
            crate::egress::check_run_allowlist(&url).expect_err("deny-all refuses this")
        });
        assert_eq!(denial.rule(), EgressRule::RunHostNotAllowlisted);

        let rows = slot()
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|sink| sink.recent(8).expect("reads")))
            .expect("a sink is installed");
        let row = rows
            .iter()
            .find(|row| row.rule_code == "egress.run-host-not-allowlisted")
            .expect("the refusal was written down");
        assert_eq!(row.guard, "egress.run-allowlist");
        assert_eq!(row.run_id.as_deref(), Some("run:sink"));
        assert_eq!(row.unattributed_reason, None);

        crate::egress::clear_run_policy_source();
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
