//! `ferridis-mcp-server` — Ferridis published as an MCP server.
//!
//! This binary lets any MCP-aware client (Claude Code, Zed, Cursor,
//! MCP-supporting Copilot, future clients) see Ferridis-connected
//! capabilities as MCP tools. It is the Ferridis-→-MCP half of the
//! interoperability shim.
//!
//! # Usage
//!
//! ```text
//! ferridis-mcp-server \
//!     --adapters-config /path/to/adapters.json \
//!     [--wallet /path/to/wallet.json]
//! ```
//!
//! `adapters-config` is a JSON array with the same shape as the VS
//! Code extension's `ferridis.adapters` setting:
//!
//! ```jsonc
//! [
//!   {
//!     "capability": "ferridis://public.ferridis.io/ferridis/fs@v1",
//!     "manifestUrl": "http://127.0.0.1:7821/manifest.json",
//!     "baseUrl": "http://127.0.0.1:7821/"
//!   }
//! ]
//! ```
//!
//! On startup the server fetches each manifest, registers it with the
//! shared Ferridis wallet/registry, builds the MCP tool catalogue, and
//! enters the stdio loop.
//!
//! Logs go to stderr so the MCP protocol channel (stdout) stays clean.

#![deny(missing_docs)]
#![forbid(unsafe_code)]
// Same rationale as `ferridis-client`: ClientError carries rich
// context and we don't want to box it everywhere just to hush clippy.
#![allow(clippy::result_large_err)]

mod config;
mod http_transport;
mod oauth_server;
mod protocol;
mod server;
mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use ferridis_client::{Client, DiscoveryHandle, Wallet};
use ferridis_core::CapabilityRef;
use tracing::{error, info, warn};
use url::Url;

use crate::config::AdapterConfig;
use crate::server::Server;
use crate::tools::ToolCatalogue;
use ferridis_client::catalogue::{Catalogue, CatalogueSource};

#[tokio::main]
async fn main() {
    init_tracing();

    let args = match Args::parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("argument error: {e}");
            eprintln!();
            Args::print_help();
            std::process::exit(2);
        }
    };

    info!(
        adapters_config = ?args.adapters_config,
        keychain_namespace = %args.keychain_namespace,
        use_memory_wallet = args.use_memory_wallet,
        discovery_broker = ?args.discovery_broker,
        "ferridis-mcp-server starting"
    );

    if !args.use_memory_wallet
        && let Err(e) = check_legacy_wallet()
    {
        error!(error = %e, "legacy plaintext wallet detected");
        std::process::exit(1);
    }

    let client = if args.use_memory_wallet {
        warn!("FERRIDIS_WALLET_MEMORY=1 \u{2014} ephemeral in-memory wallet (TEST USE ONLY)");
        Arc::new(Client::ephemeral())
    } else {
        match Client::open_keychain(&args.keychain_namespace) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                error!(error = %e, "failed to open keychain-backed wallet");
                std::process::exit(1);
            }
        }
    };

    let adapters = match config::load(&args.adapters_config) {
        Ok(v) => v,
        Err(e) => {
            error!(error = %e, "failed to load adapters config");
            std::process::exit(1);
        }
    };

    let (mut entries, failures) = register_adapters(&client, &adapters).await;

    // Partial-failure policy: log every failure at ERROR with structured
    // fields, then continue with the working subset. The publisher does
    // not exit on registration errors — one unreachable adapter cannot
    // tank the publisher's other capabilities. `tools/list` advertises
    // only the successful entries (no error placeholders — a tool that
    // is guaranteed to fail on every call is a worse user experience
    // than a tool that simply isn't there).
    for f in &failures {
        error!(
            kind = ?f.kind,
            descriptor = %f.descriptor,
            error = %f.error,
            "adapter registration failed; skipping"
        );
    }
    if !failures.is_empty() {
        warn!(
            failed = failures.len(),
            succeeded = entries.len(),
            total = adapters.len(),
            "some adapters failed to register; continuing with the working subset"
        );
    }

    // Discovery broker: fetch snapshot synchronously, then keep the SSE
    // subscription alive for the server lifetime. `_broker_handle` is
    // intentionally unused beyond drop-on-exit — dropping it cancels the
    // background SSE task.
    let _broker_handle: Option<DiscoveryHandle> = if let Some(broker_url) = args.discovery_broker {
        info!(broker_url = %broker_url, "fetching discovery broker snapshot");
        let known: std::collections::HashSet<CapabilityRef> =
            entries.iter().map(|e| e.capability().clone()).collect(); // clone: build dedup set
        let handle = client.discover_broker(broker_url).await;
        // Snapshot is synchronously registered — scan for new capabilities.
        // Collected under the lock, then released: loading response
        // schemas fetches over the network and takes the lock itself.
        let discovered: Vec<CapabilityRef> = {
            let registry = client.registry().lock().await;
            registry
                .iter()
                .map(|rec| rec.capability())
                .filter(|cap| !known.contains(*cap))
                .cloned()
                .collect()
        };
        for cap in discovered {
            if let Some(source) = with_response_schemas(&client, &cap).await {
                entries.push(source);
                info!(capability = %cap, "adapter registered via discovery broker");
            }
        }
        Some(handle)
    } else {
        None
    };

    if entries.is_empty() {
        warn!("no adapters registered; tools/list will be empty");
    } else {
        for e in &entries {
            info!(
                capability = %e.capability(),
                id = %e.manifest().id(),
                intents = ?e.manifest().intents().iter().map(|i| i.as_str().to_string()).collect::<Vec<_>>(),
                "adapter registered"
            );
        }
    }

    let catalogue = ToolCatalogue::new(Catalogue::from_sources(&entries));
    let server = Arc::new(Server::new(client, catalogue));

    match args.transport {
        Transport::Stdio => {
            server.serve_stdio().await;
        }
        Transport::Http {
            bind,
            bearer,
            oauth_issuer,
        } => {
            let oauth =
                oauth_issuer.map(|issuer| Arc::new(crate::oauth_server::OAuthServer::new(issuer)));
            let cfg = http_transport::HttpConfig {
                bind,
                bearer,
                oauth,
            };
            if let Err(e) = http_transport::serve_http(server, cfg).await {
                error!(error = %e, "HTTP transport failed");
                std::process::exit(1);
            }
        }
    }
}

fn init_tracing() {
    let filter = std::env::var("FERRIDIS_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "ferridis_mcp_server=info,ferridis_client=info".to_string());
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .try_init();
}

/// Refuse to start if a legacy v0.1 plaintext wallet exists at the
/// conventional path. v0.2 has no plaintext fallback by policy.
fn check_legacy_wallet() -> Result<(), ferridis_client::ClientError> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let legacy = format!("{home}/.ferridis/wallet.json");
    Wallet::detect_legacy_plaintext(&legacy)
}

/// Which adapter kind a registration failure belongs to. Reported in
/// structured logs so operators can grep `kind=McpStdio` to find e.g.
/// failing local MCP processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdapterKind {
    Native,
    McpSse,
    McpStdio,
}

/// One adapter that failed to register. The publisher logs and skips —
/// it does not exit. See [`register_adapters`] for the partial-failure
/// contract.
#[derive(Debug)]
struct AdapterRegistrationFailure {
    kind: AdapterKind,
    /// Best-effort identifier for the failed adapter — the capability
    /// ref for Native, the SSE URL for SSE, the stdio command for stdio.
    descriptor: String,
    /// Human-readable error from the underlying library (`ClientError`,
    /// `url::ParseError`, `ferridis_core::FerridisError`, etc.).
    error: String,
}

/// Maximum wall-clock time the publisher waits for any single adapter
/// to register at startup before giving up and marking it failed.
///
/// Chosen to be comfortably longer than `ferridis_protocol`'s
/// connect-timeout (5s) but short enough that a publisher with one
/// stalled adapter still becomes responsive within a UI-acceptable
/// window. A slow-failing adapter is indistinguishable from a hung one
/// from the host's perspective.
const ADAPTER_REGISTRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Register every adapter in `adapters` independently, never aborting
/// on a single failure.
///
/// Each adapter is bounded by [`ADAPTER_REGISTRATION_TIMEOUT`] so one
/// unreachable host on a routable-but-dead subnet cannot stall startup
/// past the OS's default TCP connect window (which can be minutes on
/// Linux). Timeout is reported as a regular registration failure.
///
/// Returns the successfully-registered entries paired with structured
/// failure records for any that did not register. Callers should log
/// the failures and proceed with the entry list — even an empty list
/// is acceptable (the publisher will respond to `tools/list` with no
/// tools rather than refuse to start).
async fn register_adapters(
    client: &Client,
    adapters: &[AdapterConfig],
) -> (Vec<CatalogueSource>, Vec<AdapterRegistrationFailure>) {
    let mut entries = Vec::with_capacity(adapters.len());
    let mut failures = Vec::new();
    for a in adapters {
        let (kind, descriptor, result) = match a {
            AdapterConfig::Native {
                capability,
                manifest_url,
                base_url,
            } => (
                AdapterKind::Native,
                capability.clone(),
                with_timeout(register_native(client, capability, manifest_url, base_url)).await,
            ),
            AdapterConfig::McpSse { mcp_sse_url } => (
                AdapterKind::McpSse,
                mcp_sse_url.clone(),
                with_timeout(register_mcp_sse(client, mcp_sse_url)).await,
            ),
            AdapterConfig::McpStdio {
                mcp_stdio_command,
                mcp_stdio_args,
                mcp_stdio_env,
            } => (
                AdapterKind::McpStdio,
                mcp_stdio_command.clone(),
                with_timeout(register_mcp_stdio(
                    client,
                    mcp_stdio_command,
                    mcp_stdio_args,
                    mcp_stdio_env,
                ))
                .await,
            ),
        };
        match result {
            Ok(entry) => entries.push(entry),
            Err(error) => failures.push(AdapterRegistrationFailure {
                kind,
                descriptor,
                error,
            }),
        }
    }
    (entries, failures)
}

async fn with_timeout<F>(fut: F) -> Result<CatalogueSource, String>
where
    F: std::future::Future<Output = Result<CatalogueSource, String>>,
{
    match tokio::time::timeout(ADAPTER_REGISTRATION_TIMEOUT, fut).await {
        Ok(inner) => inner,
        Err(_) => Err(format!(
            "registration timed out after {:?}",
            ADAPTER_REGISTRATION_TIMEOUT
        )),
    }
}

async fn register_native(
    client: &Client,
    capability: &str,
    manifest_url: &str,
    base_url: &str,
) -> Result<CatalogueSource, String> {
    let cap_ref =
        CapabilityRef::parse(capability).map_err(|e| format!("capability {capability}: {e}"))?;
    let manifest_url =
        Url::parse(manifest_url).map_err(|e| format!("manifestUrl {manifest_url}: {e}"))?;
    let base_url = Url::parse(base_url).map_err(|e| format!("baseUrl {base_url}: {e}"))?;
    client
        .register(cap_ref.clone(), manifest_url, base_url) // allow:clone — register takes ownership; the ref is still needed to look the record up
        .await
        .map_err(|e| format!("register native {capability}: {e}"))?;
    with_response_schemas(client, &cap_ref)
        .await
        .ok_or_else(|| format!("registry missing record for {cap_ref} after native register"))
}

/// Load `capability`'s response schemas, then read back its catalogue
/// source. Runs inside the caller's [`ADAPTER_REGISTRATION_TIMEOUT`] for
/// configured adapters. A schema that cannot be loaded is logged inside
/// `load_response_schemas` and only makes descriptions less specific, so
/// it never fails registration. `None` means the record is missing.
async fn with_response_schemas(
    client: &Client,
    capability: &CapabilityRef,
) -> Option<CatalogueSource> {
    match tokio::time::timeout(
        ADAPTER_REGISTRATION_TIMEOUT,
        client.load_response_schemas(capability),
    )
    .await
    {
        Ok(Ok(_outcome)) => {}
        Ok(Err(e)) => warn!(capability = %capability, error = %e, "response schemas not loaded"),
        Err(_) => warn!(
            capability = %capability,
            timeout = ?ADAPTER_REGISTRATION_TIMEOUT,
            "response-schema load timed out; descriptions will omit the schema"
        ),
    }
    lookup_registered_record(client, capability).await
}

async fn register_mcp_sse(client: &Client, mcp_sse_url: &str) -> Result<CatalogueSource, String> {
    let sse_url = Url::parse(mcp_sse_url).map_err(|e| format!("mcpSseUrl {mcp_sse_url}: {e}"))?;
    let cap_ref = client
        .register_mcp_sse(sse_url.clone())
        .await
        .map_err(|e| format!("register MCP SSE {sse_url}: {e}"))?;
    lookup_registered_record(client, &cap_ref)
        .await
        .ok_or_else(|| format!("registry missing record for {cap_ref} after MCP register"))
}

async fn register_mcp_stdio(
    client: &Client,
    command: &str,
    args: &[String],
    env: &std::collections::HashMap<String, String>,
) -> Result<CatalogueSource, String> {
    let env_pairs: Vec<(String, String)> =
        env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let cap_ref = client
        .register_mcp_stdio(command, args, &env_pairs)
        .await
        .map_err(|e| format!("register MCP stdio {command}: {e}"))?;
    lookup_registered_record(client, &cap_ref)
        .await
        .ok_or_else(|| format!("registry missing record for {cap_ref} after MCP register"))
}

/// Everything the registry now knows about `capability`, as a catalogue source.
async fn lookup_registered_record(
    client: &Client,
    capability: &CapabilityRef,
) -> Option<CatalogueSource> {
    let registry = client.registry().lock().await;
    registry.get(capability).map(CatalogueSource::from_record)
}

#[derive(Debug)]
struct Args {
    adapters_config: PathBuf,
    /// OS keychain `service` field. Lets multiple parallel Ferridis
    /// instances keep their wallets isolated.
    keychain_namespace: String,
    /// Explicit opt-in for ephemeral in-memory wallet. Set via the
    /// `FERRIDIS_WALLET_MEMORY=1` env var. **Test / embedded use only.**
    use_memory_wallet: bool,
    /// Transport selection. Stdio is the default; HTTP is opted in
    /// via `--http-bind`.
    transport: Transport,
    /// Optional Ferridis discovery broker URL. When set, services
    /// registered with the broker at startup are added to the MCP tool
    /// catalogue. An SSE subscription keeps the catalogue updated for
    /// new arrivals while the server runs.
    discovery_broker: Option<Url>,
}

/// Which transport the publisher should serve on. Both speak the same
/// MCP `Server` handler core — the difference is framing.
#[derive(Debug)]
enum Transport {
    /// MCP over stdio — the default. Lines of JSON-RPC on stdin/stdout.
    /// What in-editor MCP hosts (Claude Code, Zed context servers,
    /// Cursor, etc.) speak.
    Stdio,
    /// MCP Streamable HTTP — `POST /mcp`. Auth is either a static
    /// bearer (set by `--bearer-token` / `FERRIDIS_MCP_BEARER`) or
    /// OAuth-issued access tokens (set by `--oauth-public-base-url`),
    /// or both. At least one must be configured.
    Http {
        bind: std::net::SocketAddr,
        bearer: Option<secrecy::SecretString>,
        /// External-facing base URL the OAuth server advertises in
        /// its metadata and uses to validate redirect URIs. When
        /// `Some`, the OAuth endpoints are served.
        oauth_issuer: Option<url::Url>,
    },
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut adapters_config: Option<PathBuf> = None;
        let mut keychain_namespace: Option<String> = None;
        let mut http_bind: Option<std::net::SocketAddr> = None;
        let mut bearer_cli: Option<String> = None;
        let mut oauth_public_base_url: Option<String> = None;
        let mut discovery_broker: Option<Url> = None;
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--adapters-config" => {
                    let v = args
                        .next()
                        .ok_or_else(|| "--adapters-config expects a path".to_string())?;
                    adapters_config = Some(PathBuf::from(v));
                }
                "--keychain-namespace" => {
                    let v = args
                        .next()
                        .ok_or_else(|| "--keychain-namespace expects a value".to_string())?;
                    keychain_namespace = Some(v);
                }
                "--http-bind" => {
                    let v = args
                        .next()
                        .ok_or_else(|| "--http-bind expects an addr:port".to_string())?;
                    http_bind = Some(v.parse().map_err(|e| format!("--http-bind `{v}`: {e}"))?);
                }
                "--bearer-token" => {
                    let v = args
                        .next()
                        .ok_or_else(|| "--bearer-token expects a value".to_string())?;
                    bearer_cli = Some(v);
                }
                "--oauth-public-base-url" => {
                    let v = args
                        .next()
                        .ok_or_else(|| "--oauth-public-base-url expects a URL".to_string())?;
                    oauth_public_base_url = Some(v);
                }
                "--discovery-broker" => {
                    let v = args
                        .next()
                        .ok_or_else(|| "--discovery-broker expects a URL".to_string())?;
                    let u = Url::parse(&v).map_err(|e| format!("--discovery-broker `{v}`: {e}"))?;
                    discovery_broker = Some(u);
                }
                "--help" | "-h" => {
                    Self::print_help();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        let adapters_config =
            adapters_config.ok_or_else(|| "--adapters-config is required".to_string())?;
        let keychain_namespace = keychain_namespace
            .or_else(|| std::env::var("FERRIDIS_KEYCHAIN_NAMESPACE").ok())
            .unwrap_or_else(|| "ferridis".to_string());
        let use_memory_wallet = std::env::var("FERRIDIS_WALLET_MEMORY")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        // Transport: HTTP if --http-bind was passed; stdio otherwise.
        // HTTP requires *some* auth — static bearer, OAuth, or both.
        // Refusing to start with no auth is intentional: a publicly
        // reachable MCP endpoint without auth would expose every
        // connected capability and (with the Claude-CLI adapter) cost
        // real money on every request.
        let transport = match http_bind {
            None => {
                if bearer_cli.is_some() {
                    return Err("--bearer-token is only meaningful with --http-bind".to_string());
                }
                if oauth_public_base_url.is_some() {
                    return Err(
                        "--oauth-public-base-url is only meaningful with --http-bind".to_string(),
                    );
                }
                Transport::Stdio
            }
            Some(bind) => {
                let bearer = bearer_cli.or_else(|| std::env::var("FERRIDIS_MCP_BEARER").ok());
                let oauth_issuer = oauth_public_base_url
                    .or_else(|| std::env::var("FERRIDIS_OAUTH_PUBLIC_BASE_URL").ok());

                let bearer = match bearer.as_deref() {
                    Some(s) if s.trim().is_empty() => {
                        return Err("bearer token must not be empty".to_string());
                    }
                    Some(s) => Some(secrecy::SecretString::new(s.to_string())),
                    None => None,
                };
                let oauth_issuer = match oauth_issuer {
                    Some(s) => {
                        let mut u = url::Url::parse(&s)
                            .map_err(|e| format!("--oauth-public-base-url `{s}`: {e}"))?;
                        // Trailing slash for clean joins later.
                        if !u.path().ends_with('/') {
                            u.set_path(&format!("{}/", u.path()));
                        }
                        if u.scheme() != "https" && u.scheme() != "http" {
                            return Err(format!(
                                "--oauth-public-base-url must be http(s), got {}",
                                u.scheme()
                            ));
                        }
                        Some(u)
                    }
                    None => None,
                };

                if bearer.is_none() && oauth_issuer.is_none() {
                    return Err("--http-bind requires at least one auth path: \
                         --bearer-token / FERRIDIS_MCP_BEARER (static bearer) \
                         and/or --oauth-public-base-url / FERRIDIS_OAUTH_PUBLIC_BASE_URL \
                         (OAuth 2.1 server, claude.ai-compatible). \
                         Refusing to publish unauthenticated."
                        .to_string());
                }

                Transport::Http {
                    bind,
                    bearer,
                    oauth_issuer,
                }
            }
        };

        Ok(Self {
            adapters_config,
            keychain_namespace,
            use_memory_wallet,
            transport,
            discovery_broker,
        })
    }

    fn print_help() {
        eprintln!("Usage: ferridis-mcp-server --adapters-config <path> [options]");
        eprintln!();
        eprintln!("Required:");
        eprintln!("  --adapters-config <path>    JSON file listing adapters to publish.");
        eprintln!();
        eprintln!("Wallet:");
        eprintln!("  --keychain-namespace <name> OS keychain service name. Default: \"ferridis\"");
        eprintln!("                              (also FERRIDIS_KEYCHAIN_NAMESPACE env var)");
        eprintln!("                              Set FERRIDIS_WALLET_MEMORY=1 for ephemeral");
        eprintln!("                              in-memory wallet (TEST USE ONLY).");
        eprintln!();
        eprintln!("Transport (stdio is the default):");
        eprintln!("  (none)                      MCP over stdio. What in-editor MCP hosts speak.");
        eprintln!("  --http-bind <addr:port>     MCP Streamable HTTP. Use a TLS-terminating proxy");
        eprintln!("                              or tunnel (cloudflared / Tailscale Funnel) when");
        eprintln!("                              exposing to remote callers such as claude.ai.");
        eprintln!("                              Requires at least one auth path below.");
        eprintln!();
        eprintln!("Discovery:");
        eprintln!("  --discovery-broker <url>    Ferridis discovery broker URL. Services");
        eprintln!("                              registered with the broker are added to the");
        eprintln!("                              tool catalogue at startup and kept current");
        eprintln!("                              via SSE push for new arrivals.");
        eprintln!();
        eprintln!("HTTP auth (at least one required when --http-bind is set):");
        eprintln!(
            "  --bearer-token <token>      Static bearer required on `Authorization` header."
        );
        eprintln!("                              (also FERRIDIS_MCP_BEARER env var)");
        eprintln!("  --oauth-public-base-url URL Enable OAuth 2.1 + PKCE + DCR endpoints on the");
        eprintln!("                              listener. URL is the externally-facing base URL");
        eprintln!("                              (e.g. https://<tunnel>/), advertised as `issuer`");
        eprintln!("                              in /.well-known/oauth-authorization-server.");
        eprintln!("                              Required for claude.ai Custom Connectors.");
        eprintln!("                              (also FERRIDIS_OAUTH_PUBLIC_BASE_URL env var)");
    }
}
