//! The `place_call` agent tool.
//!
//! Dialing a phone number is the most consequential thing in this codebase: it
//! reaches a person who did not ask to be reached, and it bills the operator.
//! Three gates stand in front of it and none of them is the model's to move.
//!
//! 1. The run's own permission snapshot must allow external mutation.
//! 2. The account's `outbound_approval` must permit it at all. `never` is a
//!    refusal no approval prompt can override — an operator who configured a
//!    number for answering has not agreed to let it dial.
//! 3. The normal approval prompt, which the tool dispatcher applies like it
//!    does for every other permission-gated tool — unless the account carries
//!    the operator's standing approval (`allow`), which is what that setting
//!    means. See [`outbound_needs_prompt`].
//!
//! The call row is written before the carrier is asked, keyed on the durable
//! identity of the tool invocation that asked for it, so the retry of a run
//! finds the call it already placed instead of dialing twice.
//!
//! That identity is deliberately not the destination. Two legitimate calls to
//! the same number in one run are two calls; a key built from the number would
//! silently collapse the second onto the first and report a call that was never
//! placed. It is the runtime's tool-call id — assigned by the loop, never seen
//! by the model — paired with the job when there is one. Where neither exists
//! there is nothing durable to be identical to, so the call gets a fresh
//! identity rather than sharing one with every other call to that number.

use super::store::{DaemonPaths, DaemonStore};
use super::telecom_store::{CallDirection, CallRecording, OutboundCallApproval, TelecomCallRecord};
use super::telephony::{callback_url, provider_for_account, CallState};

/// Environment variable naming the job a task child is running, set by the
/// daemon. Absent for every other kind of run.
const JOB_ID_ENV: &str = "LITTLE_MONKEY_DAEMON_JOB_ID";

/// E.164, loosely: a plus and 7-15 digits. Rejected here rather than at the
/// carrier so a malformed number never becomes a billable attempt.
fn valid_e164(number: &str) -> bool {
    let digits = number.strip_prefix('+').unwrap_or_default();
    (7..=15).contains(&digits.len()) && digits.chars().all(|character| character.is_ascii_digit())
}

/// The durable identity of the tool invocation asking for a call.
///
/// The same shape `channel_tool` keys a send on, and for the same reason: a
/// replayed run must resolve to the effect it already had, and nothing derived
/// from the model's arguments can be trusted to say which effect that was.
#[derive(Debug, Clone, Default)]
pub(crate) struct CallInvocation {
    pub job_id: Option<String>,
    pub tool_call_id: Option<String>,
}

impl CallInvocation {
    /// What the call row deduplicates on.
    ///
    /// A tool-call id is the whole identity when there is one — it is unique
    /// per invocation and stable across a replay of it. With only a job, the
    /// job is the identity. With neither (an interactive session that keeps no
    /// durable run), there is nothing to be identical to, so a fresh id keeps
    /// two deliberate calls from collapsing into one.
    fn idempotency_key(&self) -> String {
        match (self.job_id.as_deref(), self.tool_call_id.as_deref()) {
            (Some(job), Some(tool_call)) => format!("outbound:{job}:{tool_call}"),
            (None, Some(tool_call)) => format!("outbound:tool:{tool_call}"),
            (Some(job), None) => format!("outbound:job:{job}"),
            (None, None) => format!("outbound:once:{}", uuid::Uuid::new_v4().simple()),
        }
    }
}

/// Place one call.
pub(crate) async fn place_call(
    account_id: &str,
    to_number: &str,
    opening_line: &str,
    invocation: &CallInvocation,
) -> Result<serde_json::Value, String> {
    // A call that opens with silence is worse than no call: the person who
    // picked up hears nothing and hangs up, and it still cost money. The words
    // are required, and they are what the approval prompt showed.
    if opening_line.trim().is_empty() {
        return Err("Say what the call is about; a call cannot open with silence.".to_string());
    }
    if opening_line.chars().count() > 600 {
        return Err("That opening line is too long to say on a phone call.".to_string());
    }
    if !valid_e164(to_number) {
        return Err(
            "A phone number must be in international format, like +15551234567.".to_string(),
        );
    }
    let paths = DaemonPaths::resolve()?;
    let mut store = DaemonStore::open(&paths)?;
    let account = store
        .telecom_account(account_id)?
        .ok_or_else(|| format!("No telephony account '{account_id}' is configured."))?;
    if !account.enabled {
        return Err("That telephony account is disabled.".to_string());
    }
    match account.outbound_approval {
        OutboundCallApproval::Never => {
            return Err(
                "This number is configured for receiving calls only. An operator has to enable outbound calling before it can dial."
                    .to_string(),
            )
        }
        OutboundCallApproval::Approval | OutboundCallApproval::Allow => {}
    }
    let live = store.live_call_count(account_id)?;
    if live >= account.limits.max_concurrent_calls {
        return Err(format!(
            "This account is already on {live} call(s), which is its limit. Wait for one to end or raise the limit in Settings."
        ));
    }

    let job_id = invocation
        .job_id
        .clone()
        .or_else(|| std::env::var(JOB_ID_ENV).ok())
        .filter(|id| !id.is_empty())
        .unwrap_or_default();
    let invocation = CallInvocation {
        job_id: (!job_id.is_empty()).then(|| job_id.clone()),
        tool_call_id: invocation.tool_call_id.clone(),
    };
    let idempotency_key = invocation.idempotency_key();
    let carrier_key = idempotency_key.clone();
    let now_ms = now_ms()?;
    let call_id = format!("call-{}", uuid::Uuid::new_v4().simple());
    let recorded = store.start_call(&TelecomCallRecord {
        call_id: call_id.clone(),
        account_id: account.account_id.clone(),
        provider_call_id: None,
        direction: CallDirection::Outbound,
        peer_number: to_number.to_string(),
        state: CallState::Queued,
        session_key: Some(super::telecom_store::call_session_key(
            &account, to_number, &call_id,
        )),
        job_id: (!job_id.is_empty()).then(|| job_id.clone()),
        idempotency_key,
        opening_line: Some(opening_line.trim().to_string()),
        last_error: None,
        started_at_ms: None,
        ended_at_ms: None,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    })?;
    let call_id = match recorded {
        CallRecording::Duplicate { call_id } => {
            // This exact invocation already dialed — a replayed run, not a
            // second request. Saying so is the whole point: the alternative is
            // a second ring at somebody's phone.
            return Ok(serde_json::json!({
                "status": "already_placed",
                "call_id": call_id,
                "note": "This call was already placed by this same tool call; nothing was dialed again."
            }));
        }
        CallRecording::Recorded { call_id } => call_id,
    };

    let secret = match &account.credential_ref {
        Some(reference) => super::channel_adapter::ChannelSecrets::get(
            &super::channel_adapter::KeyringChannelSecrets,
            reference,
        )
        .unwrap_or_default(),
        None => String::new(),
    };
    let provider = provider_for_account(&account, secret)?;

    // The carrier calls us back on this account's own callback path under the
    // operator's configured public URL. Without one there is nowhere for the
    // audio to be directed, so the call is refused rather than placed blind.
    let base = account
        .public_base_url
        .clone()
        .ok_or_else(|| "This account has no public callback URL configured, so a call would have nowhere to connect.".to_string())?;
    let answer_url = callback_url(&base, &account.account_id);

    match provider
        .place_call(
            to_number,
            &answer_url,
            account.limits.recording_enabled,
            &carrier_key,
        )
        .await
    {
        Ok(handle) => {
            if !handle.provider_call_id.is_empty() {
                store.set_call_provider_id(&call_id, &handle.provider_call_id, now_ms)?;
            }
            store.advance_call(&call_id, handle.state, None, now_ms)?;
            Ok(serde_json::json!({
                "status": handle.state.as_str(),
                "call_id": call_id,
            }))
        }
        Err(error) => {
            // The row stays. A failure here may still have reached the carrier,
            // and a call that might exist is not something to forget about.
            store.advance_call(
                &call_id,
                CallState::NeedsReconciliation,
                Some(&error),
                now_ms,
            )?;
            Err(format!("The carrier did not confirm the call: {error}"))
        }
    }
}

/// Whether this account still wants an approval prompt before it dials.
///
/// `approval` — the default, and what every account starts as — prompts every
/// time. `allow` is a standing approval the operator gave in Settings, so the
/// prompt is skipped; the run's own external-mutation grant and the limits
/// still apply, and `never` is refused inside [`place_call`] whatever this
/// says.
///
/// Anything unreadable prompts. The safe answer to "I cannot tell what this
/// operator agreed to" is to ask them.
pub(crate) fn outbound_needs_prompt(account_id: &str) -> bool {
    let Ok(paths) = DaemonPaths::resolve() else {
        return true;
    };
    let Ok(store) = DaemonStore::open(&paths) else {
        return true;
    };
    match store.telecom_account(account_id) {
        Ok(Some(account)) => prompts_before_dialing(account.outbound_approval),
        _ => true,
    }
}

/// The rule itself, apart from where the account is read from.
fn prompts_before_dialing(approval: OutboundCallApproval) -> bool {
    match approval {
        // The operator gave this number a standing approval in Settings.
        // Prompting anyway would make that setting mean nothing.
        OutboundCallApproval::Allow => false,
        // `never` is refused outright inside `place_call`; asking first is
        // harmless and is what an unset account gets.
        OutboundCallApproval::Approval | OutboundCallApproval::Never => true,
    }
}

/// Whether any configured account is allowed to dial out at all.
///
/// Used to decide whether to offer the tool. An operator whose numbers are all
/// receive-only never sees it, which is better than offering a tool whose only
/// possible answer is a refusal.
pub(crate) fn any_account_may_dial() -> bool {
    let Ok(paths) = DaemonPaths::resolve() else {
        return false;
    };
    let Ok(store) = DaemonStore::open(&paths) else {
        return false;
    };
    store.telecom_accounts().is_ok_and(|accounts| {
        accounts.iter().any(|account| {
            account.enabled && !matches!(account.outbound_approval, OutboundCallApproval::Never)
        })
    })
}

fn now_ms() -> Result<i64, String> {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "System clock is before the Unix epoch".to_string())?
            .as_millis(),
    )
    .map_err(|_| "System clock is beyond the supported range".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_invocation_is_one_call_however_often_the_run_replays() {
        let invocation = CallInvocation {
            job_id: Some("job-1".into()),
            tool_call_id: Some("call-7".into()),
        };

        assert_eq!(invocation.idempotency_key(), invocation.idempotency_key());
        // A second deliberate call in the same run is a different invocation
        // and must not collapse onto the first — which a key built from the
        // destination did.
        let second = CallInvocation {
            job_id: Some("job-1".into()),
            tool_call_id: Some("call-8".into()),
        };
        assert_ne!(invocation.idempotency_key(), second.idempotency_key());
        // With nothing durable behind it there is nothing to be identical to,
        // so two calls are two calls rather than one silently dropped.
        assert_ne!(
            CallInvocation::default().idempotency_key(),
            CallInvocation::default().idempotency_key()
        );
    }

    #[test]
    fn a_standing_approval_is_the_one_thing_that_skips_the_prompt() {
        // Two separate powers, and this is the second one. `allow` is the
        // operator saying "this number may dial without asking me each time";
        // anything else asks.
        assert!(!prompts_before_dialing(OutboundCallApproval::Allow));
        assert!(prompts_before_dialing(OutboundCallApproval::Approval));
        assert!(prompts_before_dialing(OutboundCallApproval::Never));
    }

    #[test]
    fn a_number_must_look_like_a_phone_number() {
        assert!(valid_e164("+15551234567"));
        assert!(valid_e164("+4670123456"));
        assert!(!valid_e164("15551234567"), "a missing plus is not E.164");
        assert!(!valid_e164("+1555"), "too short to be a real number");
        assert!(
            !valid_e164("+1555123456789012"),
            "too long to be a real number"
        );
        assert!(
            !valid_e164("+1555; DROP"),
            "no punctuation reaches a carrier"
        );
        assert!(!valid_e164(""));
    }

    #[tokio::test]
    async fn a_call_with_nothing_to_say_is_refused() {
        let error = place_call("tel-1", "+15551234567", "   ", &CallInvocation::default())
            .await
            .expect_err("refused");
        assert!(error.contains("cannot open with silence"));
    }

    #[tokio::test]
    async fn a_malformed_number_is_refused_before_anything_is_opened() {
        let error = place_call(
            "tel-1",
            "not-a-number",
            "hello, this is a test",
            &CallInvocation::default(),
        )
        .await
        .expect_err("refused");
        assert!(error.contains("international format"));
    }
}
