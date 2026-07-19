//! Read-only CLI projection of the declarative plugin aggregate. The state
//! and health computation live in `M4CommandState`; this module only renders
//! the exact production result as human-readable or versioned JSON output.

use std::path::Path;

use clap::Subcommand;
use little_monkey_lib::m4_commands::M4CommandState;
use little_monkey_lib::m4_services::{
    PluginComponentState, PluginRuntimeDescriptor, PluginRuntimeHealth,
};
use little_monkey_lib::package_ecosystem::{PackageKind, SemanticVersion};
use serde::Serialize;

#[derive(Subcommand, Debug)]
pub enum PluginsCmd {
    /// List installed declarative plugins and their current activation state.
    List {
        /// Print a stable machine-readable list.
        #[arg(long)]
        json: bool,
    },
    /// Inspect aggregate health, setup gaps, components, and rollback state.
    Health {
        /// Print the versioned machine-readable health report.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Serialize)]
struct PluginListEntry<'a> {
    package_id: &'a str,
    name: &'a str,
    version: Option<SemanticVersion>,
    kind: PackageKind,
    health: PluginRuntimeHealth,
    enabled: bool,
    signed: bool,
    bundle_sha256: Option<&'a str>,
    pinned_version: Option<SemanticVersion>,
    rollback_target: Option<SemanticVersion>,
    rollback_healthy: bool,
}

impl<'a> From<&'a PluginRuntimeDescriptor> for PluginListEntry<'a> {
    fn from(plugin: &'a PluginRuntimeDescriptor) -> Self {
        Self {
            package_id: &plugin.package_id,
            name: &plugin.name,
            version: plugin.version,
            kind: plugin.kind,
            health: plugin.health,
            enabled: plugin.enabled,
            signed: plugin.signed,
            bundle_sha256: plugin.bundle_sha256.as_deref(),
            pinned_version: plugin.pinned_version,
            rollback_target: plugin.rollback_target,
            rollback_healthy: plugin.rollback_healthy,
        }
    }
}

#[derive(Debug, Default, Serialize)]
struct PluginHealthSummary {
    total: usize,
    healthy: usize,
    needs_setup: usize,
    disabled: usize,
    blocked: usize,
    corrupt: usize,
}

impl PluginHealthSummary {
    fn from_plugins(plugins: &[PluginRuntimeDescriptor]) -> Self {
        let mut summary = Self {
            total: plugins.len(),
            ..Self::default()
        };
        for plugin in plugins {
            match plugin.health {
                PluginRuntimeHealth::Healthy => summary.healthy += 1,
                PluginRuntimeHealth::NeedsSetup => summary.needs_setup += 1,
                PluginRuntimeHealth::Disabled => summary.disabled += 1,
                PluginRuntimeHealth::Blocked => summary.blocked += 1,
                PluginRuntimeHealth::Corrupt => summary.corrupt += 1,
            }
        }
        summary
    }
}

#[derive(Debug, Serialize)]
struct PluginHealthReport<'a> {
    schema_version: u32,
    summary: PluginHealthSummary,
    plugins: &'a [PluginRuntimeDescriptor],
}

pub fn run(action: &PluginsCmd, data_dir: &Path) -> Result<(), String> {
    let state = M4CommandState::production(data_dir)?;
    let plugins = state.plugin_runtime()?;
    match action {
        PluginsCmd::List { json } => print_list(&plugins, *json),
        PluginsCmd::Health { json } => print_health(&plugins, *json),
    }
}

fn print_list(plugins: &[PluginRuntimeDescriptor], json: bool) -> Result<(), String> {
    if json {
        let entries = plugins
            .iter()
            .map(PluginListEntry::from)
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).map_err(|error| error.to_string())?
        );
    } else if plugins.is_empty() {
        println!("No declarative plugins are installed.");
    } else {
        for plugin in plugins {
            println!(
                "{:<36} {:<10} {:<12} {:<12} {}",
                plugin.package_id,
                plugin
                    .version
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                package_kind_label(plugin.kind),
                health_label(plugin.health),
                if plugin.signed {
                    "signed"
                } else {
                    "local-unsigned"
                },
            );
        }
    }
    Ok(())
}

fn print_health(plugins: &[PluginRuntimeDescriptor], json: bool) -> Result<(), String> {
    let summary = PluginHealthSummary::from_plugins(plugins);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&PluginHealthReport {
                schema_version: 1,
                summary,
                plugins,
            })
            .map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    println!(
        "Plugin health: {} total, {} healthy, {} need setup, {} disabled, {} blocked, {} corrupt",
        summary.total,
        summary.healthy,
        summary.needs_setup,
        summary.disabled,
        summary.blocked,
        summary.corrupt,
    );
    for plugin in plugins {
        let active_components = plugin
            .components
            .iter()
            .filter(|component| component.state == PluginComponentState::Active)
            .count();
        let setup_components = plugin
            .components
            .iter()
            .filter(|component| component.state == PluginComponentState::NeedsSetup)
            .count();
        println!(
            "[{}] {} {} ({})",
            health_label(plugin.health).to_ascii_uppercase(),
            plugin.name,
            plugin
                .version
                .map(|version| format!("v{version}"))
                .unwrap_or_else(|| "version unavailable".to_string()),
            plugin.package_id,
        );
        println!(
            "  {} · {} active components · {} need setup · rollback {}",
            if plugin.signed {
                "signature verified"
            } else {
                "reviewed local unsigned package"
            },
            active_components,
            setup_components,
            match plugin.rollback_target {
                Some(version) if plugin.rollback_healthy => format!("ready at v{version}"),
                Some(version) => format!("cache invalid at v{version}"),
                None => "unavailable".to_string(),
            },
        );
        for issue in &plugin.issues {
            println!("  issue: {issue}");
        }
    }
    Ok(())
}

fn package_kind_label(kind: PackageKind) -> &'static str {
    match kind {
        PackageKind::Skill => "skill",
        PackageKind::Assistant => "assistant",
        PackageKind::Connector => "connector",
        PackageKind::Collection => "collection",
    }
}

fn health_label(health: PluginRuntimeHealth) -> &'static str {
    match health {
        PluginRuntimeHealth::Healthy => "healthy",
        PluginRuntimeHealth::NeedsSetup => "needs-setup",
        PluginRuntimeHealth::Disabled => "disabled",
        PluginRuntimeHealth::Blocked => "blocked",
        PluginRuntimeHealth::Corrupt => "corrupt",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_command_shape_supports_list_and_health_json() {
        use clap::Parser;

        #[derive(Parser)]
        struct Harness {
            #[command(subcommand)]
            command: PluginsCmd,
        }

        assert!(matches!(
            Harness::try_parse_from(["monkey", "list", "--json"])
                .expect("list should parse")
                .command,
            PluginsCmd::List { json: true }
        ));
        assert!(matches!(
            Harness::try_parse_from(["monkey", "health", "--json"])
                .expect("health should parse")
                .command,
            PluginsCmd::Health { json: true }
        ));
    }

    #[test]
    fn health_summary_keeps_each_terminal_state_distinct() {
        let health = [
            PluginRuntimeHealth::Healthy,
            PluginRuntimeHealth::NeedsSetup,
            PluginRuntimeHealth::Disabled,
            PluginRuntimeHealth::Blocked,
            PluginRuntimeHealth::Corrupt,
        ];
        assert_eq!(
            health
                .iter()
                .map(|value| health_label(*value))
                .collect::<Vec<_>>(),
            ["healthy", "needs-setup", "disabled", "blocked", "corrupt",]
        );
    }
}
