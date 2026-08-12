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
//!    does for every other permission-gated tool.
//!
//! The call row is written before the carrier is asked, keyed on an
//! idempotency key derived from the run and the number, so a retried run finds
//! the call it already placed instead of dialing twice.

use super::store::{DaemonPaths, DaemonStore};
use super::telecom_store::{CallDirection, CallRecording, OutboundCallApproval, TelecomCallRecord};
use super::telephony::{build_provider, CallState, TelecomConfig};

/// Environment variable naming the job a task child is running, set by the
/// daemon. Absent for every other kind of run.
const JOB_ID_ENV: &str = "LITTLE_MONKEY_DAEMON_JOB_ID";

/// E.164, loosely: a plus and 7-15 digits. Rejected here rather than at the
/// carrier so a malformed number never becomes a billable attempt.
fn valid_e164(number: &str) -> bool {
    let digits = number.strip_prefix('+').unwrap_or_default();
    (7..=15).contains(&digits.len()) && digits.chars().all(|character| character.is_ascii_digit())
}

/// Place one call.
pub(crate) async fn place_call(account_id: &str, to_number: &str) -> Result<serde_json::Value, String> {
    if !valid_e164(to_number) {
        return Err("A phone number must be in international format, like +15551234567.".to_string());
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

    let job_id = std::env::var(JOB_ID_ENV).unwrap_or_default();
    let idempotency_key = format!("outbound:{job_id}:{to_number}");
    let now_ms = now_ms()?;
    let call_id = format!("call-{}", uuid::Uuid::new_v4().simple());
    let recorded = store.start_call(&TelecomCallRecord {
        call_id: call_id.clone(),
        account_id: account.account_id.clone(),
        provider_call_id: None,
        direction: CallDirection::Outbound,
        peer_number: to_number.to_string(),
        state: CallState::Queued,
        session_key: Some(format!("call:{}:{call_id}", account.account_id)),
        job_id: (!job_id.is_empty()).then(|| job_id.clone()),
        idempotency_key,
        last_error: None,
        started_at_ms: None,
        ended_at_ms: None,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    })?;
    let call_id = match recorded {
        CallRecording::Duplicate { call_id } => {
            // This run already dialed this number. Saying so is the whole
            // point: the alternative is a second ring at somebody's phone.
            return Ok(serde_json::json!({
                "status": "already_placed",
                "call_id": call_id,
                "note": "This run already placed a call to that number; nothing was dialed again."
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
    let provider = build_provider(TelecomConfig {
        account_id: account.account_id.clone(),
        kind: account.kind,
        carrier_account_id: account.carrier_account_id.clone(),
        from_number: account.from_number.clone(),
        secret,
        public_base_url: account.public_base_url.clone(),
        webhook_public_key: account
            .non_secret_config
            .get("webhook_public_key")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    })?;

    // The carrier calls us back on this account's own callback path under the
    // operator's configured public URL. Without one there is nowhere for the
    // audio to be directed, so the call is refused rather than placed blind.
    let base = account
        .public_base_url
        .clone()
        .ok_or_else(|| "This account has no public callback URL configured, so a call would have nowhere to connect.".to_string())?;
    let answer_url = format!("{}/v1/telecom/{}", base.trim_end_matches('/'), account.account_id);

    match provider.place_call(to_number, &answer_url).await {
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
            store.advance_call(&call_id, CallState::NeedsReconciliation, Some(&error), now_ms)?;
            Err(format!("The carrier did not confirm the call: {error}"))
        }
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
    fn a_number_must_look_like_a_phone_number() {
        assert!(valid_e164("+15551234567"));
        assert!(valid_e164("+4670123456"));
        assert!(!valid_e164("15551234567"), "a missing plus is not E.164");
        assert!(!valid_e164("+1555"), "too short to be a real number");
        assert!(!valid_e164("+1555123456789012"), "too long to be a real number");
        assert!(!valid_e164("+1555; DROP"), "no punctuation reaches a carrier");
        assert!(!valid_e164(""));
    }

    #[tokio::test]
    async fn a_malformed_number_is_refused_before_anything_is_opened() {
        let error = place_call("tel-1", "not-a-number").await.expect_err("refused");
        assert!(error.contains("international format"));
    }
}
