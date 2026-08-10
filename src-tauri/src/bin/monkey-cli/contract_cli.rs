//! `monkey contract` — the published K19 syscall ABI, from the command line.
//!
//! Three questions, one per subcommand: what version does this build
//! implement (`version`), what exactly is in it (`emit`), and does a machine
//! I am about to talk to implement something I can use (`check --url`). The
//! last one exists because a third party integrating against this ABI cannot
//! run the repository's own test — the check they need is "does the node I
//! am pointing at still speak my version", and that is an HTTP request, not
//! a repository state.

use clap::Subcommand;
use little_monkey_lib::contract;

#[derive(Subcommand, Debug)]
pub enum ContractCmd {
    /// Print the contract version and digest this build implements.
    Version,
    /// Print the full published contract as JSON (the same bytes as
    /// `contract/agent-os-contract.json`), or write them to a file.
    Emit {
        /// Write to this path instead of stdout.
        #[arg(long, value_name = "PATH")]
        out: Option<std::path::PathBuf>,
    },
    /// Ask a running instance which contract it implements and compare it
    /// with this build's. Exit code 0 when compatible (same major, and its
    /// minor is at least ours), 1 when not.
    Check {
        /// Base URL of the instance, e.g. http://127.0.0.1:1234
        #[arg(long, value_name = "URL")]
        url: String,
    },
}

pub async fn run(command: &ContractCmd) -> Result<(), String> {
    match command {
        ContractCmd::Version => {
            println!("contract {}", contract::CONTRACT_VERSION);
            println!("digest    sha256:{}", contract::digest());
            println!(
                "support   {} days after a deprecation is announced",
                contract::SUPPORT_WINDOW_DAYS
            );
            Ok(())
        }
        ContractCmd::Emit { out } => {
            let text = contract::manifest_json_text();
            match out {
                Some(path) => {
                    std::fs::write(&path, text)
                        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;
                    println!("Wrote {}", path.display());
                }
                None => print!("{text}"),
            }
            Ok(())
        }
        ContractCmd::Check { url } => check_remote(url).await,
    }
}

async fn check_remote(base_url: &str) -> Result<(), String> {
    let endpoint = format!("{}/v1/contract", base_url.trim_end_matches('/'));
    // `egress::hardened()` rather than a bare client: the URL comes from the
    // command line and may be a LAN peer, so the connect/read budgets and the
    // same-origin redirect policy are exactly the hardening this needs.
    let response =
        little_monkey_lib::egress::hardened_with_read_budget(std::time::Duration::from_secs(10))
            .build()
            .map_err(|error| format!("Could not build an HTTP client: {error}"))?
            .get(&endpoint)
            .send()
            .await
            .map_err(|error| format!("Could not reach {endpoint}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "{endpoint} answered {} — that instance predates the contract endpoint, \
             or is not a Little Monkey listener",
            response.status()
        ));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("{endpoint} did not answer JSON: {error}"))?;
    let their_version = body["contract_version"]
        .as_str()
        .ok_or_else(|| format!("{endpoint} answered without a contract_version"))?;
    println!("remote  {their_version} ({endpoint})");
    println!("local   {}", contract::CONTRACT_VERSION);
    if let Some(digest) = body["digest"].as_str() {
        println!(
            "digest  {}",
            if digest == contract::digest() {
                "identical surface".to_string()
            } else {
                format!("differs (remote sha256:{digest})")
            }
        );
    }
    let theirs = contract::parse_version(their_version)
        .ok_or_else(|| format!("Remote contract version {their_version} is not x.y.z"))?;
    let ours = contract::parse_version(contract::CONTRACT_VERSION)
        .ok_or_else(|| "Local CONTRACT_VERSION is not x.y.z".to_string())?;
    // A major difference means a surface one side does not have. A remote
    // *older* minor means it may be missing something added since — both are
    // refusals here rather than warnings, because the caller asked whether it
    // can rely on this instance.
    if theirs.major != ours.major {
        return Err(format!(
            "Incompatible: remote implements contract {their_version}, this build speaks \
             {}.x — a major difference means a surface one side does not have.",
            ours.major
        ));
    }
    if theirs.minor < ours.minor {
        return Err(format!(
            "Remote implements contract {their_version}, older than this build's {}. \
             Anything added in {}.{} is not there.",
            contract::CONTRACT_VERSION,
            ours.major,
            ours.minor
        ));
    }
    println!("compatible");
    Ok(())
}
