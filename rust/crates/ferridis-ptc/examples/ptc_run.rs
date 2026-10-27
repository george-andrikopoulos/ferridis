//! Run one programmatic-tool-calling conversation against the live
//! Messages API, with the tools an adapters file registers.
//!
//! ```text
//! ANTHROPIC_API_KEY=... cargo run -p ferridis-ptc --example ptc_run -- \
//!     adapters.json "Read every file under the root and total their line counts."
//! ```
//!
//! A key not scoped to a workspace also needs `ANTHROPIC_WORKSPACE_ID`.
//!
//! This spends tokens: one conversation is a few Messages API requests
//! at the default model's rates, plus code-execution container time.
//! Make a tool code-callable with the adapters file's `callers` map or the
//! manifest's `callers` declaration; with none, every tool is direct and
//! no container starts.
//!
//! Uses an ephemeral wallet: OAuth-backed adapters need the MCP server's
//! keychain wallet and are out of scope for this example.

use std::path::PathBuf;
use std::process::ExitCode;

use ferridis_client::Client;
use ferridis_client::adapters::{AdaptersFile, register_adapters};
use ferridis_client::catalogue::Catalogue;
use ferridis_ptc::{
    ApiKey, CodeRun, Conversation, Executor, MaxTokens, MaxTurns, ModelId, PtcClient, ToolPlan,
    UserPrompt, WorkspaceId, run,
};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    match real_main().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let adapters = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: ptc_run <adapters.json> <prompt...>")?;
    let prompt = UserPrompt::new(args.collect::<Vec<_>>().join(" "))?;

    let file = AdaptersFile::load(&adapters)?;
    let client = Client::ephemeral();
    let (sources, failures) = register_adapters(&client, file.adapters()).await;
    for f in &failures {
        eprintln!(
            "skipped {:?} adapter {}: {}",
            f.kind(),
            f.descriptor(),
            f.error()
        );
    }
    let plan = ToolPlan::build(&Catalogue::from_sources(&sources), file.caller_overrides());
    for t in plan.tools() {
        eprintln!(
            "tool {} -> {} ({:?})",
            t.name(),
            t.policy().policy(),
            t.policy().source()
        );
    }
    let conversation = Conversation::new(
        ModelId::default_model(),
        MaxTokens::default(),
        plan.tool_set(),
    );
    let executor = Executor::new(plan, client);
    let mut ptc = PtcClient::new(ApiKey::from_env()?);
    if let Some(w) = WorkspaceId::from_env()? {
        ptc = ptc.with_workspace(w);
    }

    let outcome = run(&ptc, &executor, conversation, prompt, MaxTurns::default()).await?;
    for run in outcome.final_turn().code_runs() {
        match run {
            CodeRun::Finished {
                stdout,
                return_code,
                ..
            } => eprintln!("code run exited {return_code}; stdout:\n{stdout}"),
            CodeRun::Failed { error_code } => eprintln!("code run failed: {error_code}"),
        }
    }
    let calls = outcome.calls();
    let usage = outcome.usage();
    eprintln!(
        "turns {} | programmatic {} direct {} refused {} failed {} | \
         tokens in {} (cache write {}, cache read {}) out {}",
        outcome.turns(),
        calls.programmatic(),
        calls.direct(),
        calls.refused(),
        calls.failed(),
        usage.input_tokens(),
        usage.cache_creation_input_tokens(),
        usage.cache_read_input_tokens(),
        usage.output_tokens()
    );
    println!("{}", outcome.final_turn().text());
    Ok(())
}
