//! The one live run: a real Messages API conversation whose code calls a
//! real filesystem adapter programmatically.
//!
//! Ignored by default and gated twice, so neither `cargo test` nor
//! `cargo test -- --ignored` alone spends anything:
//!
//! ```text
//! FERRIDIS_PTC_LIVE=1 ANTHROPIC_API_KEY=... \
//!     cargo test -p ferridis-ptc --test live_smoke -- --ignored --nocapture
//! ```
//!
//! A key not scoped to a workspace also needs `ANTHROPIC_WORKSPACE_ID`.
//!
//! **Cost, measured 2026-10-26:** 5 requests to `claude-opus-5-5`
//! (4 programmatic calls, each answered in its own continuation),
//! 10,835 input and 215 output tokens. At $4 / $20 per million that is
//! about $0.05 per run, against a pre-run estimate of $0.10–0.15. The run
//! took 10 s; container time is free within an organisation's 1,550
//! monthly hours and $0.05 per hour beyond. The test prints its token
//! usage, so a later run can be checked against this figure.

use std::sync::Arc;

use ferridis_adapter_fs::{FilesystemCapability, Root};
use ferridis_adapter_sdk::AdapterServer;
use ferridis_client::Client;
use ferridis_client::adapters::{AdaptersFile, register_adapters};
use ferridis_client::catalogue::Catalogue;
use ferridis_ptc::{
    ApiKey, Conversation, Executor, MaxTokens, MaxTurns, ModelId, PtcClient, StopReason, ToolPlan,
    UserPrompt, WorkspaceId, run,
};
use serde_json::json;
use tempfile::TempDir;
use tokio::net::TcpListener;

const LIVE_GATE: &str = "FERRIDIS_PTC_LIVE";

#[tokio::test]
#[ignore = "spends real tokens; set FERRIDIS_PTC_LIVE=1 and ANTHROPIC_API_KEY, run with --ignored"]
async fn live_programmatic_calls_reach_a_real_adapter() {
    if std::env::var(LIVE_GATE).as_deref() != Ok("1") {
        eprintln!("{LIVE_GATE} is not 1; skipping the live run (no tokens spent)");
        return;
    }
    let api_key = ApiKey::from_env().expect("ANTHROPIC_API_KEY must be set for the live run");

    let dir = TempDir::new().expect("tempdir");
    for (name, n) in [("n1.txt", "17"), ("n2.txt", "25"), ("n3.txt", "58")] {
        std::fs::write(dir.path().join(name), n).expect("seed file");
    }
    let cap = FilesystemCapability::new(Root::new(dir.path()).expect("absolute root"))
        .expect("default manifest parses");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let router = AdapterServer::new(cap).into_router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve adapter");
    });
    let doc = json!({
        "adapters": [{
            "capability": "ferridis://public.ferridis.io/ferridis/fs@v1",
            "manifestUrl": format!("http://{addr}/manifest.json"),
            "baseUrl": format!("http://{addr}/")
        }],
        "callers": {
            "ferridis_ferridis_fs_v1_list_dir": "code",
            "ferridis_ferridis_fs_v1_read_file": "code"
        }
    });
    let file = AdaptersFile::parse(doc.to_string().as_bytes()).expect("adapters document parses");
    let client = Client::ephemeral();
    let (sources, failures) = register_adapters(&client, file.adapters()).await;
    assert!(failures.is_empty(), "{failures:?}");
    let plan = ToolPlan::build(&Catalogue::from_sources(&sources), file.caller_overrides());
    let conversation = Conversation::new(
        ModelId::default_model(),
        MaxTokens::default(),
        plan.tool_set(),
    );
    let executor = Executor::new(plan, client);
    let prompt = UserPrompt::new(
        "Using code execution, list the root directory, read every file in it, and add up \
         the integers the files contain. Reply with the sum only.",
    )
    .expect("non-empty prompt");

    let mut ptc = PtcClient::new(api_key);
    if let Some(w) =
        WorkspaceId::from_env().expect("ANTHROPIC_WORKSPACE_ID, when set, is non-empty")
    {
        ptc = ptc.with_workspace(w);
    }
    let outcome = run(
        &ptc,
        &executor,
        conversation,
        prompt,
        MaxTurns::new(8).expect("non-zero"),
    )
    .await
    .expect("live run completes");

    let usage = outcome.usage();
    eprintln!(
        "live run: {} turns, {} programmatic calls, {} input / {} output tokens; answer: {}",
        outcome.turns(),
        outcome.calls().programmatic(),
        usage.input_tokens(),
        usage.output_tokens(),
        outcome.final_turn().text()
    );
    assert_eq!(outcome.final_turn().stop_reason(), &StopReason::EndTurn);
    assert!(
        outcome.calls().programmatic() >= 1,
        "no call came from code"
    );
    assert_eq!(outcome.calls().refused(), 0);
    assert!(
        outcome.final_turn().text().contains("100"),
        "17 + 25 + 58 = 100"
    );
    let _keep_dir_alive = Arc::new(dir);
}
