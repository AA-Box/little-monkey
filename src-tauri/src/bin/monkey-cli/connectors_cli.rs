//! `monkey connectors` — inspect, reverify and remove connector accounts.
//!
//! Deliberately small, and deliberately without a `connect`: the OAuth
//! providers open a system browser for consent and stream progress to the
//! desktop window (`connector-oauth://status`), so connecting a new OAuth
//! account is desktop-only. A half-built terminal version would be a worse
//! claim than saying so, which `docs/limitations.md` does.
//!
//! Secrets are never printed. The human listing shows the account id,
//! provider, label, recorded identity and verification time. `--json`
//! serializes the stored `ConnectorAccount` as-is, which additionally includes
//! `credential_ref` (a keychain account name, not a credential) and
//! `connection` (non-secret provider metadata — a GitLab host, an S3 endpoint
//! and access key id). That is a wider shape than `export_audit_impl`'s
//! redacted one, and is meant for a local operator reading their own catalog.

use little_monkey_lib::connector_oauth::is_reconnect_error;
use little_monkey_lib::connectors::{self, ConnectorAccount};
use little_monkey_lib::AppState;

/// `monkey connectors <action>`.
#[derive(clap::Subcommand, Debug)]
pub enum ConnectorsCmd {
    /// List connected accounts with their recorded verification state.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Re-run an account's live verification call and record the outcome.
    /// For an OAuth account this refreshes the access token first.
    Reverify {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove an account: revokes it at the provider where the provider
    /// publishes an endpoint, then deletes the catalog row and its keychain
    /// entry.
    Remove { id: String },
}

pub async fn dispatch(action: &ConnectorsCmd) -> Result<(), String> {
    match action {
        ConnectorsCmd::List { json } => list(*json),
        ConnectorsCmd::Reverify { id, json } => reverify(id, *json).await,
        ConnectorsCmd::Remove { id } => remove(id).await,
    }
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|e| format!("Failed to render JSON: {e}"))?
    );
    Ok(())
}

fn describe(account: &ConnectorAccount) -> String {
    let identity = account.identity.as_deref().unwrap_or("—");
    let verified = match account.last_verified_at {
        Some(at) => format!("verified at {at}"),
        None => "never verified".to_string(),
    };
    format!(
        "{}  {}  {}  ({identity})  {verified}",
        account.id,
        format_args!("{:<16}", account.provider.as_str()),
        account.label
    )
}

fn list(json: bool) -> Result<(), String> {
    let accounts = connectors::load_config_impl(&connectors::config_file_path()?)?.accounts;
    if json {
        return print_json(&accounts);
    }
    if accounts.is_empty() {
        println!("No connector accounts. Connect one in Settings → Connectors.");
        return Ok(());
    }
    for account in &accounts {
        println!("{}", describe(account));
        if let Some(error) = &account.last_error {
            println!("    error: {error}");
            if is_reconnect_error(error) {
                println!("    → reconnect this account in Settings → Connectors (browser consent is desktop-only)");
            }
        }
    }
    Ok(())
}

async fn reverify(id: &str, json: bool) -> Result<(), String> {
    let state = AppState::default();
    let account = connectors::reverify_impl(&state, &connectors::config_file_path()?, id).await?;
    if json {
        return print_json(&account);
    }
    println!("{}", describe(&account));
    Ok(())
}

async fn remove(id: &str) -> Result<(), String> {
    let state = AppState::default();
    connectors::remove_account(&state, &connectors::config_file_path()?, id).await?;
    println!("Removed {id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::connector_oauth::reconnect_error;
    use little_monkey_lib::connectors::ConnectorProvider;

    fn account(provider: ConnectorProvider) -> ConnectorAccount {
        ConnectorAccount {
            id: "acct-1".to_string(),
            provider,
            label: "Work".to_string(),
            scopes: vec!["read".to_string()],
            credential_ref: Some("connector-oauth:acct-1".to_string()),
            identity: Some("ada@example.com".to_string()),
            created_at: 1,
            last_verified_at: None,
            last_error: None,
            connection: None,
        }
    }

    #[test]
    fn describe_renders_the_identity_and_the_verification_state() {
        let mut row = account(ConnectorProvider::GoogleDrive);
        let never = describe(&row);
        assert!(never.contains("acct-1"), "{never}");
        assert!(never.contains("google_drive"), "{never}");
        assert!(never.contains("Work"), "{never}");
        assert!(never.contains("(ada@example.com)"), "{never}");
        assert!(never.contains("never verified"), "{never}");

        row.last_verified_at = Some(1_700_000_000_000);
        assert!(describe(&row).contains("verified at 1700000000000"));

        row.identity = None;
        assert!(describe(&row).contains("(—)"));
    }

    #[test]
    fn describe_prints_no_keychain_reference_for_an_oauth_row() {
        // `credential_ref` is a keychain account name, not a credential, but
        // the human listing has no reason to show it — only `--json` does.
        let line = describe(&account(ConnectorProvider::Linear));
        assert!(!line.contains("connector-oauth:"), "{line}");
    }

    #[test]
    fn a_reconnect_shaped_error_is_recognised_and_an_ordinary_one_is_not() {
        let reconnect = reconnect_error(ConnectorProvider::Dropbox, "invalid_grant");
        assert!(is_reconnect_error(&reconnect), "{reconnect}");
        assert!(!is_reconnect_error("Dropbox returned HTTP 500"));
    }
}
