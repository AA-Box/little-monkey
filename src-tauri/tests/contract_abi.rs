//! The K19 gate: the published contract is the one this build implements, and
//! `CONTRACT_VERSION` has moved far enough for whatever changed since the last
//! publish.
//!
//! **Why two files.** `contract/agent-os-contract.json` is what this build
//! *would* publish — regenerated from the route tables and the tool
//! definitions, so it always tracks the code. `contract/baseline.json` is what
//! was last *actually* published, and it only changes when a human publishes.
//! One file could not gate anything: regenerating it would silently accept the
//! breaking change along with the edit that caused it.
//!
//! Both tests run in CI on all three platforms as part of `pnpm test:rust`,
//! and `pnpm contract:check` runs just this file.

use little_monkey_lib::contract;

fn repo_path(relative: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

#[test]
fn the_published_artifact_is_the_contract_this_build_implements() {
    let path = repo_path("contract/agent-os-contract.json");
    let generated = contract::manifest_json_text();
    let published = std::fs::read_to_string(&path).unwrap_or_default();
    if published == generated {
        return;
    }
    if std::env::var("UPDATE_CONTRACT").is_ok() {
        std::fs::write(&path, &generated).expect("writing the regenerated contract");
        return;
    }
    panic!(
        "contract/agent-os-contract.json is stale — the code's routes, tools or \
         versions no longer match the published artifact.\n\
         Regenerate it with `UPDATE_CONTRACT=1 pnpm contract:check` (or \
         `monkey contract emit`), review the diff, and — if the surface itself \
         changed — follow docs/contract-abi.md to bump CONTRACT_VERSION and \
         republish the baseline."
    );
}

#[test]
fn the_contract_version_covers_every_change_since_the_last_publish() {
    let baseline = std::fs::read_to_string(repo_path("contract/baseline.json"))
        .expect("contract/baseline.json — the last published contract");
    match contract::check_against_baseline(&baseline) {
        Ok(_) => {}
        Err(explanation) => panic!("{explanation}"),
    }
}

/// The gate is only as good as its ability to fail. A baseline that describes
/// a surface this build no longer has must be rejected while
/// `CONTRACT_VERSION` is still a `1.x` — otherwise the test above would pass
/// on an empty file just as happily as on a real one.
#[test]
fn a_baseline_this_build_no_longer_satisfies_is_rejected() {
    let mut baseline = contract::manifest();
    baseline.acp.methods.push("session/invented".to_string());
    let error = contract::check_against_baseline(&serde_json::to_string(&baseline).unwrap())
        .expect_err("a method the build dropped must fail the gate");
    assert!(
        error.contains("acp method removed: session/invented"),
        "{error}"
    );
}
