//! Named points where inbound acceptance can be made to fail on purpose.
//!
//! A crash-consistency claim is only worth what the test that proves it can
//! reproduce, and the boundaries that matter here are *inside* one SQLite
//! transaction — between the provider event and the accepted turn, and between
//! the accepted turn and the commit. No amount of timing can land a test there
//! from the outside, and a sleep is not a crash.
//!
//! So the durable path names its boundaries, and a test arms one. The armed
//! point fires exactly once and disarms itself, which is what a crash is: the
//! next process does not inherit it. Nothing is armed in a release build —
//! [`fire`] compiles to `Ok(())` — and no production code path can arm one,
//! because [`arm`] does not exist outside tests.

/// A boundary in the inbound durable path.
///
/// The names are the invariant they exist to break: everything before the
/// point has happened, nothing after it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailPoint {
    /// Inside the acceptance transaction, after the provider event is inserted
    /// and before the accepted turn is. The window the old two-transaction
    /// design left open, and the one a rollback has to close.
    AfterEventInsert,
    /// Inside the acceptance transaction, after the accepted turn is inserted
    /// and before the commit.
    BeforeAcceptCommit,
    /// After the attachments of an accepted event are downloaded and their
    /// results stored, before the message is routed.
    AfterAttachmentHydration,
    /// After the acceptance is committed, before the run reaches the queue.
    BeforeQueueSubmit,
    /// After the queue took the run, before the turn is marked queued.
    BeforeQueuedState,
    /// After every local durable write, before the provider cursor is
    /// persisted or its delivery acknowledged.
    BeforeCursorCommit,
}

impl FailPoint {
    #[cfg(test)]
    fn as_str(self) -> &'static str {
        match self {
            FailPoint::AfterEventInsert => "after the provider event was recorded",
            FailPoint::BeforeAcceptCommit => "before the acceptance was committed",
            FailPoint::AfterAttachmentHydration => "after the attachments were stored",
            FailPoint::BeforeQueueSubmit => "before the run reached the queue",
            FailPoint::BeforeQueuedState => "before the turn was marked queued",
            FailPoint::BeforeCursorCommit => "before the provider cursor was committed",
        }
    }
}

/// Fail here if a test armed this point. Free and always `Ok` in release.
#[cfg(not(test))]
#[inline]
pub(crate) fn fire(_point: FailPoint) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
pub(crate) fn fire(point: FailPoint) -> Result<(), String> {
    ARMED.with(|armed| {
        if armed.get() == Some(point) {
            armed.set(None);
            Err(format!("injected failure {}", point.as_str()))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
thread_local! {
    static ARMED: std::cell::Cell<Option<FailPoint>> = const { std::cell::Cell::new(None) };
}

/// Arm one boundary for the next time this thread reaches it.
#[cfg(test)]
pub(crate) fn arm(point: FailPoint) {
    ARMED.with(|armed| armed.set(Some(point)));
}

/// Whether the armed point is still waiting — a test that expected its failure
/// to fire asserts on this rather than on a message it can only see through a
/// count.
#[cfg(test)]
pub(crate) fn fired() -> bool {
    ARMED.with(|armed| armed.get().is_none())
}
