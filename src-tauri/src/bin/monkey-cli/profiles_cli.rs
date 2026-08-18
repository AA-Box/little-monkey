//! `monkey profiles` — the local identity boundary from the command line (K23).
//!
//! The desktop app and this CLI resolve the *same* registry through
//! `little_monkey_lib::profiles`, so a profile created here is the one the app
//! switches to, and `--profile`/`LITTLE_MONKEY_PROFILE` runs one command
//! against another identity without changing which one the app opens.
//!
//! Deliberately thin: every rule — id validation, the default profile's legacy
//! root, quota bounds, what may be deleted — lives in the library beside the
//! resolution that enforces it, because a second copy here would be a second
//! answer to "whose data is this".

use std::path::Path;

use clap::Subcommand;
use little_monkey_lib::profiles::{self, Profile, ProfileQuota};

#[derive(Subcommand, Debug)]
pub enum ProfilesCmd {
    /// List every local profile, marking the active one.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Create a profile with its own data root, credentials and run history.
    Create {
        /// Display name; the id is derived from it.
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Make a profile the one the app and CLI open next.
    Switch {
        /// Profile id, as printed by `monkey profiles list`.
        id: String,
    },
    /// Rename a profile. The id, and therefore its data root, never changes.
    Rename { id: String, name: String },
    /// Set a profile's quota (K4) and its share of the machine (K8).
    Limits {
        id: String,
        /// Share of contended memory relative to the other profiles.
        #[arg(long)]
        weight: Option<f64>,
        /// Maximum concurrently dispatched daemon jobs.
        #[arg(long)]
        max_concurrent_runs: Option<u32>,
        /// Ceiling on system memory this profile's admitted jobs may hold.
        #[arg(long)]
        max_memory_bytes: Option<u64>,
        /// Ceiling on a single job's wall clock, in milliseconds.
        #[arg(long)]
        max_runtime_ms: Option<u64>,
        /// Clear every ceiling before applying the flags above.
        #[arg(long)]
        clear: bool,
    },
    /// Delete a profile and everything under its managed and authored roots.
    Delete {
        id: String,
        /// Required: this removes run history, artifacts and authored settings.
        #[arg(long)]
        yes: bool,
    },
    /// Print the profile this command is running as, and where its data lives.
    Current {
        #[arg(long)]
        json: bool,
    },
}

/// `base` is the *unscoped* app data directory — the registry decides which
/// profile root sits under it, so this one command may not be profile-scoped.
pub fn run(action: &ProfilesCmd, base: &Path) -> Result<(), String> {
    match action {
        ProfilesCmd::List { json } => {
            let registry = profiles::load_registry(base).map_err(|error| error.to_string())?;
            let active = profiles::active_id(base).map_err(|error| error.to_string())?;
            if *json {
                let rows: Vec<_> = registry
                    .profiles
                    .iter()
                    .map(|profile| {
                        serde_json::json!({
                            "id": profile.id,
                            "name": profile.name,
                            "active": profile.id == active,
                            "root": profiles::profile_root(base, &profile.id),
                            "fairShareWeight": profile.fair_share_weight,
                            "share": registry.share_of(&profile.id),
                            "quota": profile.quota,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&rows).map_err(|error| error.to_string())?
                );
                return Ok(());
            }
            for profile in &registry.profiles {
                let marker = if profile.id == active { "*" } else { " " };
                println!(
                    "{marker} {:<20} {:<24} share {:>5.0}%  {}",
                    profile.id,
                    profile.name,
                    registry.share_of(&profile.id) * 100.0,
                    profiles::profile_root(base, &profile.id).display(),
                );
            }
            Ok(())
        }
        ProfilesCmd::Create { name, json } => {
            let profile =
                profiles::create_profile(base, name).map_err(|error| error.to_string())?;
            report(&profile, base, *json)
        }
        ProfilesCmd::Switch { id } => {
            let profile = profiles::switch_profile(base, id).map_err(|error| error.to_string())?;
            println!(
                "Active profile is now '{}' ({}). Restart a running app or daemon to pick it up.",
                profile.id, profile.name,
            );
            Ok(())
        }
        ProfilesCmd::Rename { id, name } => {
            let profile =
                profiles::rename_profile(base, id, name).map_err(|error| error.to_string())?;
            report(&profile, base, false)
        }
        ProfilesCmd::Limits {
            id,
            weight,
            max_concurrent_runs,
            max_memory_bytes,
            max_runtime_ms,
            clear,
        } => {
            let registry = profiles::load_registry(base).map_err(|error| error.to_string())?;
            let current = registry
                .get(id)
                .ok_or_else(|| format!("no profile with id '{id}'"))?;
            let base_quota = if *clear {
                ProfileQuota::default()
            } else {
                current.quota
            };
            let quota = ProfileQuota {
                max_concurrent_runs: max_concurrent_runs.or(base_quota.max_concurrent_runs),
                max_memory_bytes: max_memory_bytes.or(base_quota.max_memory_bytes),
                max_runtime_ms: max_runtime_ms.or(base_quota.max_runtime_ms),
            };
            let profile =
                profiles::set_limits(base, id, quota, weight.unwrap_or(current.fair_share_weight))
                    .map_err(|error| error.to_string())?;
            report(&profile, base, false)
        }
        ProfilesCmd::Delete { id, yes } => {
            if !yes {
                return Err(format!(
                    "refusing to delete '{id}' and its managed and authored roots without --yes"
                ));
            }
            let agent_home = little_monkey_lib::app_paths::agent_home_dir()?;
            profiles::delete_profile(base, &agent_home, id).map_err(|error| error.to_string())?;
            println!("Deleted profile '{id}' and its managed and authored roots.");
            Ok(())
        }
        ProfilesCmd::Current { json } => {
            let profile = profiles::active_profile(base).map_err(|error| error.to_string())?;
            report(&profile, base, *json)
        }
    }
}

fn report(profile: &Profile, base: &Path, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "id": profile.id,
                "name": profile.name,
                "root": profiles::profile_root(base, &profile.id),
                "fairShareWeight": profile.fair_share_weight,
                "quota": profile.quota,
            }))
            .map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "{} ({})\n  root   {}\n  weight {}\n  quota  {}",
            profile.id,
            profile.name,
            profiles::profile_root(base, &profile.id).display(),
            profile.fair_share_weight,
            describe_quota(&profile.quota),
        );
    }
    Ok(())
}

fn describe_quota(quota: &ProfileQuota) -> String {
    let mut parts = Vec::new();
    if let Some(runs) = quota.max_concurrent_runs {
        parts.push(format!("{runs} concurrent runs"));
    }
    if let Some(bytes) = quota.max_memory_bytes {
        parts.push(format!("{} MiB memory", bytes / (1024 * 1024)));
    }
    if let Some(ms) = quota.max_runtime_ms {
        parts.push(format!("{ms} ms per run"));
    }
    if parts.is_empty() {
        "unbounded".to_string()
    } else {
        parts.join(", ")
    }
}
