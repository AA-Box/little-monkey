//! `monkey connectors` — inspect, reverify and remove connector accounts.
//!
//! Deliberately small, and deliberately without a `connect`: the OAuth
//! providers open a system browser for consent and stream progress to the
//! desktop window (`connector-oauth://status`), so connecting a new OAuth
//! account is desktop-only. A half-built terminal version would be a worse
//! claim than saying so, which `docs/limitations.md` does.
//!
//! Secrets are never printed. The listing shows `credential_ref` — a keychain
//! account name, not a credential — and whatever the last real verification
//! call recorded.

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
