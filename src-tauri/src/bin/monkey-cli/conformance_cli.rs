//! `monkey-cli conformance` — run the published K21 conformance suite
//! against a live node.
//!
//! This is the half of K21 that a third party actually gets: the suite lives
//! in the library, but the only way to *run* it without building this crate is
//! a command. It takes a base URL and a token, talks to whatever is listening
//! there over a real socket, and prints a report whose verdict names the suite
//! revision that produced it.
//!
//! Exit codes are the contract for CI: `0` when the node is compatible with
//! the named revision, `1` when it is not.

use little_monkey_lib::conformance::{self, SectionId, SuiteOptions};

#[derive(clap::Args, Debug)]
pub struct ConformanceArgs {
    /// Base URL of the node under test.
    #[arg(long, default_value = "http://127.0.0.1:1234")]
    pub base_url: String,
    /// Bearer token, when the node requires one. Also read from
    /// `LITTLE_MONKEY_API_TOKEN`.
    #[arg(long)]
    pub token: Option<String>,
    /// Run only these sections (repeatable): contract, isolation, limits,
    /// ledger. The default runs all of them.
    #[arg(long = "section")]
    pub sections: Vec<String>,
    /// Exercise the inference contract with this model rather than the first
    /// one the node lists.
    #[arg(long)]
    pub model: Option<String>,
    /// Emit the machine-readable report instead of the terminal summary.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: &ConformanceArgs) -> Result<(), String> {
    let mut sections = Vec::new();
    for name in &args.sections {
        let section = SectionId::parse(name).ok_or_else(|| {
            format!(
                "Unknown section '{name}'. Known sections: {}.",
                SectionId::ALL
                    .iter()
                    .map(|section| section.code())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        sections.push(section);
    }

    let options = SuiteOptions {
        base_url: args.base_url.clone(),
        token: args
            .token
            .clone()
            .or_else(|| std::env::var("LITTLE_MONKEY_API_TOKEN").ok()),
        sections,
        model: args.model.clone(),
    };

    let client = conformance::client()?;
    let report = conformance::run_suite(&client, &options).await;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|error| format!("Failed to serialize the conformance report: {error}"))?
        );
    } else {
        print!("{}", report.to_summary());
    }

    if report.is_compatible() {
        Ok(())
    } else {
        // The summary already said why, in the caller's chosen format.
        // Repeating it through `fail` would print the reasons twice.
        std::process::exit(1);
    }
}
