//! The adapters file (`adapters.json`) and startup registration.
//!
//! Moved here from `ferridis-mcp-server` (decision PTC-3) so every host
//! that publishes Ferridis capabilities as model tools registers them the
//! same way: the MCP publisher and `ferridis-ptc` both call
//! [`register_adapters`].
//!
//! # File shapes
//!
//! A bare array of adapter entries (the original shape, unchanged):
//!
//! ```jsonc
//! [
//!   { "capability": "ferridis://public.ferridis.io/ferridis/fs@v1",
//!     "manifestUrl": "http://127.0.0.1:7821/manifest.json",
//!     "baseUrl": "http://127.0.0.1:7821/" },
//!   { "mcpSseUrl": "http://192.168.1.190:8765/sse" },
//!   { "mcpStdioCommand": "uvx", "mcpStdioArgs": ["semble[mcp]", "semble"] }
//! ]
//! ```
//!
//! Or an object that adds the operator's per-tool caller overrides
//! (decision PTC-1), keyed by tool name so MCP-federated tools can be
//! overridden too:
//!
//! ```jsonc
//! {
//!   "adapters": [ /* entries as above */ ],
//!   "callers": { "ferridis_ferridis_fs_v1_list_dir": "code" }
//! }
//! ```

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use ferridis_core::{CallerPolicy, CapabilityRef, FerridisError};
use serde::Deserialize;
use url::Url;

use crate::catalogue::{CatalogueSource, ToolName, ToolNameError};
use crate::client::Client;
use crate::error::ClientError;

/// One adapter configured at startup.
///
/// Variants are distinguished by their unique discriminator fields
/// (`capability` for Native, `mcpSseUrl` for SSE, `mcpStdioCommand`
/// for stdio). Untagged deserialization picks the first variant whose
/// required fields are present. Values stay raw here and are parsed at
/// registration, so one malformed entry fails only its own adapter.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AdapterConfig {
    /// A Ferridis-native HTTP adapter.
    Native {
        /// The `ferridis://` capability reference.
        capability: String,
        /// HTTP URL the adapter serves its manifest from.
        #[serde(rename = "manifestUrl")]
        manifest_url: String,
        /// Base URL the adapter's `/intents/:verb` routes live under.
        #[serde(rename = "baseUrl")]
        base_url: String,
    },
    /// An MCP server consumed over SSE and federated through Ferridis.
    McpSse {
        /// The SSE endpoint URL.
        #[serde(rename = "mcpSseUrl")]
        mcp_sse_url: String,
    },
    /// An MCP server consumed over stdio and federated through Ferridis.
    McpStdio {
        /// The executable to spawn.
        #[serde(rename = "mcpStdioCommand")]
        mcp_stdio_command: String,
        /// Arguments to pass to the executable.
        #[serde(rename = "mcpStdioArgs", default)]
        mcp_stdio_args: Vec<String>,
        /// Environment variables to set for the child process.
        #[serde(rename = "mcpStdioEnv", default)]
        mcp_stdio_env: HashMap<String, String>,
    },
}

/// The operator's per-tool caller overrides, keyed by tool name.
#[derive(Debug, Clone, Default)]
pub struct CallerOverrides(BTreeMap<ToolName, CallerPolicy>);

impl CallerOverrides {
    /// The override for `tool`, if the operator set one.
    #[must_use]
    pub fn get(&self, tool: &ToolName) -> Option<CallerPolicy> {
        self.0.get(tool).copied()
    }

    /// Every overridden tool name, so a host can warn about names that
    /// match no registered tool (a typo silently overrides nothing).
    pub fn tool_names(&self) -> impl Iterator<Item = &ToolName> {
        self.0.keys()
    }

    /// How many tools are overridden.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no tool is overridden.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A parsed adapters file.
#[derive(Debug, Clone)]
pub struct AdaptersFile {
    adapters: Vec<AdapterConfig>,
    caller_overrides: CallerOverrides,
}

impl AdaptersFile {
    /// Read and parse the adapters file at `path`.
    ///
    /// # Errors
    ///
    /// [`AdaptersFileError`] naming the path and what failed.
    pub fn load(path: &Path) -> Result<Self, AdaptersFileError> {
        let bytes = std::fs::read(path).map_err(|source| AdaptersFileError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&bytes).map_err(|e| e.at(path))
    }

    /// Parse an adapters document.
    ///
    /// # Errors
    ///
    /// [`AdaptersFileError::Parse`] for malformed JSON or an unknown
    /// top-level key; [`AdaptersFileError::InvalidToolName`] for a
    /// `callers` key that cannot be a tool name.
    pub fn parse(bytes: &[u8]) -> Result<Self, AdaptersFileError> {
        let raw: RawAdaptersFile = serde_json::from_slice(bytes)
            .map_err(|source| AdaptersFileError::Parse { path: None, source })?;
        let (adapters, raw_callers) = match raw {
            RawAdaptersFile::Bare(adapters) => (adapters, BTreeMap::new()),
            RawAdaptersFile::Structured(s) => (s.adapters, s.callers),
        };
        let mut overrides = BTreeMap::new();
        for (key, policy) in raw_callers {
            let name = ToolName::parse(&key)
                .map_err(|source| AdaptersFileError::InvalidToolName { key, source })?;
            overrides.insert(name, policy);
        }
        Ok(Self {
            adapters,
            caller_overrides: CallerOverrides(overrides),
        })
    }

    /// The configured adapters, in file order.
    #[must_use]
    pub fn adapters(&self) -> &[AdapterConfig] {
        &self.adapters
    }

    /// The operator's per-tool caller overrides (empty for a bare array).
    #[must_use]
    pub fn caller_overrides(&self) -> &CallerOverrides {
        &self.caller_overrides
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawAdaptersFile {
    Bare(Vec<AdapterConfig>),
    Structured(RawStructuredFile),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStructuredFile {
    adapters: Vec<AdapterConfig>,
    #[serde(default)]
    callers: BTreeMap<String, CallerPolicy>,
}

/// Why the adapters file could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum AdaptersFileError {
    /// The file could not be read.
    #[error("read {path}: {source}")]
    Io {
        /// The path that failed.
        path: String,
        /// The I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The document is not a valid adapters file.
    #[error("parse {}: {source}", path.as_deref().unwrap_or("adapters document"))]
    Parse {
        /// The path, when the document came from a file.
        path: Option<String>,
        /// The JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// A `callers` key cannot be a tool name.
    #[error("callers key {key:?} is not a tool name: {source}")]
    InvalidToolName {
        /// The offending key.
        key: String,
        /// Why it was rejected.
        #[source]
        source: ToolNameError,
    },
}

impl AdaptersFileError {
    fn at(self, file: &Path) -> Self {
        match self {
            Self::Parse { source, .. } => Self::Parse {
                path: Some(file.display().to_string()),
                source,
            },
            other => other,
        }
    }
}

/// Which adapter kind a registration failure belongs to. Reported in
/// structured logs so operators can grep `kind=McpStdio` to find e.g.
/// failing local MCP processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    /// A Ferridis-native HTTP adapter.
    Native,
    /// An MCP server over SSE.
    McpSse,
    /// An MCP server over stdio.
    McpStdio,
}

/// Why one adapter failed to register.
#[derive(Debug, thiserror::Error)]
pub enum AdapterRegistrationError {
    /// The `capability` value is not a `ferridis://` reference.
    #[error("capability {value}: {source}")]
    InvalidCapability {
        /// The raw value.
        value: String,
        /// The parse error.
        #[source]
        source: FerridisError,
    },
    /// A URL field could not be parsed.
    #[error("{field} {value}: {source}")]
    InvalidUrl {
        /// Which field (`manifestUrl`, `baseUrl`, `mcpSseUrl`).
        field: &'static str,
        /// The raw value.
        value: String,
        /// The parse error.
        #[source]
        source: url::ParseError,
    },
    /// The client refused or failed the registration.
    #[error("register: {0}")]
    Register(#[source] Box<ClientError>),
    /// Registration succeeded but the registry holds no record for it.
    #[error("registry holds no record for {0} after registering it")]
    RecordMissing(CapabilityRef),
    /// Registration did not finish within the bound.
    #[error("registration timed out after {0:?}")]
    TimedOut(Duration),
}

/// One adapter that failed to register. Hosts log and skip it; see
/// [`register_adapters`] for the partial-failure contract.
#[derive(Debug)]
pub struct AdapterRegistrationFailure {
    kind: AdapterKind,
    descriptor: String,
    error: AdapterRegistrationError,
}

impl AdapterRegistrationFailure {
    /// Which kind of adapter failed.
    #[must_use]
    pub fn kind(&self) -> AdapterKind {
        self.kind
    }

    /// Best-effort identifier: the capability ref for Native, the SSE URL
    /// for SSE, the command for stdio.
    #[must_use]
    pub fn descriptor(&self) -> &str {
        &self.descriptor
    }

    /// Why it failed.
    #[must_use]
    pub fn error(&self) -> &AdapterRegistrationError {
        &self.error
    }
}

/// Maximum wall-clock time to wait for any single adapter to register
/// (and, for native adapters, to load its response schemas) before
/// marking it failed.
///
/// Comfortably longer than `ferridis_protocol`'s connect timeout (5 s),
/// short enough that a host with one stalled adapter still becomes
/// responsive within a UI-acceptable window.
pub const ADAPTER_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Register every adapter in `adapters` independently, never aborting
/// on a single failure.
///
/// Each adapter is bounded by [`ADAPTER_REGISTRATION_TIMEOUT`], so one
/// unreachable host cannot stall startup past the OS's TCP connect window.
/// Native adapters also load their response schemas
/// ([`Client::load_response_schemas`]); a schema that cannot be loaded is
/// logged and never fails registration.
///
/// Returns the registered sources and a failure record for each adapter
/// that did not register. An empty source list is acceptable: hosts serve
/// no tools rather than refuse to start.
pub async fn register_adapters(
    client: &Client,
    adapters: &[AdapterConfig],
) -> (Vec<CatalogueSource>, Vec<AdapterRegistrationFailure>) {
    let mut sources = Vec::with_capacity(adapters.len());
    let mut failures = Vec::new();
    for adapter in adapters {
        let (kind, descriptor, result) = match adapter {
            AdapterConfig::Native {
                capability,
                manifest_url,
                base_url,
            } => (
                AdapterKind::Native,
                capability.as_str(),
                bounded(register_native(client, capability, manifest_url, base_url)).await,
            ),
            AdapterConfig::McpSse { mcp_sse_url } => (
                AdapterKind::McpSse,
                mcp_sse_url.as_str(),
                bounded(register_mcp_sse(client, mcp_sse_url)).await,
            ),
            AdapterConfig::McpStdio {
                mcp_stdio_command,
                mcp_stdio_args,
                mcp_stdio_env,
            } => (
                AdapterKind::McpStdio,
                mcp_stdio_command.as_str(),
                bounded(register_mcp_stdio(
                    client,
                    mcp_stdio_command,
                    mcp_stdio_args,
                    mcp_stdio_env,
                ))
                .await,
            ),
        };
        match result {
            Ok(source) => sources.push(source),
            Err(error) => failures.push(AdapterRegistrationFailure {
                kind,
                descriptor: descriptor.to_owned(),
                error,
            }),
        }
    }
    (sources, failures)
}

/// Load `capability`'s response schemas within
/// [`ADAPTER_REGISTRATION_TIMEOUT`], then read back its catalogue source.
/// A load failure or timeout is logged and only makes tool descriptions
/// less specific. `None` means the registry holds no record.
pub async fn source_with_response_schemas(
    client: &Client,
    capability: &CapabilityRef,
) -> Option<CatalogueSource> {
    match tokio::time::timeout(
        ADAPTER_REGISTRATION_TIMEOUT,
        client.load_response_schemas(capability),
    )
    .await
    {
        // The outcome's failures were already logged inside the call.
        Ok(Ok(_outcome)) => {}
        Ok(Err(e)) => {
            tracing::warn!(capability = %capability, error = %e, "response schemas not loaded");
        }
        Err(_) => tracing::warn!(
            capability = %capability,
            timeout = ?ADAPTER_REGISTRATION_TIMEOUT,
            "response-schema load timed out; descriptions will omit the schema"
        ),
    }
    registered_source(client, capability).await
}

async fn bounded<F>(fut: F) -> Result<CatalogueSource, AdapterRegistrationError>
where
    F: std::future::Future<Output = Result<CatalogueSource, AdapterRegistrationError>>,
{
    tokio::time::timeout(ADAPTER_REGISTRATION_TIMEOUT, fut)
        .await
        .unwrap_or(Err(AdapterRegistrationError::TimedOut(
            ADAPTER_REGISTRATION_TIMEOUT,
        )))
}

fn parse_url(field: &'static str, value: &str) -> Result<Url, AdapterRegistrationError> {
    Url::parse(value).map_err(|source| AdapterRegistrationError::InvalidUrl {
        field,
        value: value.to_owned(),
        source,
    })
}

async fn register_native(
    client: &Client,
    capability: &str,
    manifest_url: &str,
    base_url: &str,
) -> Result<CatalogueSource, AdapterRegistrationError> {
    let cap_ref = CapabilityRef::parse(capability).map_err(|source| {
        AdapterRegistrationError::InvalidCapability {
            value: capability.to_owned(),
            source,
        }
    })?;
    let manifest_url = parse_url("manifestUrl", manifest_url)?;
    let base_url = parse_url("baseUrl", base_url)?;
    client
        .register(cap_ref.clone(), manifest_url, base_url) // allow:clone — register takes ownership; the ref is still needed to look the record up
        .await
        .map_err(|e| AdapterRegistrationError::Register(Box::new(e)))?;
    source_with_response_schemas(client, &cap_ref)
        .await
        .ok_or(AdapterRegistrationError::RecordMissing(cap_ref))
}

async fn register_mcp_sse(
    client: &Client,
    mcp_sse_url: &str,
) -> Result<CatalogueSource, AdapterRegistrationError> {
    let sse_url = parse_url("mcpSseUrl", mcp_sse_url)?;
    let cap_ref = client
        .register_mcp_sse(sse_url)
        .await
        .map_err(|e| AdapterRegistrationError::Register(Box::new(e)))?;
    registered_source(client, &cap_ref)
        .await
        .ok_or(AdapterRegistrationError::RecordMissing(cap_ref))
}

async fn register_mcp_stdio(
    client: &Client,
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<CatalogueSource, AdapterRegistrationError> {
    let env_pairs: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone())) // allow:clone — register_mcp_stdio takes owned pairs for the child's environment; the config keeps its own
        .collect();
    let cap_ref = client
        .register_mcp_stdio(command, args, &env_pairs)
        .await
        .map_err(|e| AdapterRegistrationError::Register(Box::new(e)))?;
    registered_source(client, &cap_ref)
        .await
        .ok_or(AdapterRegistrationError::RecordMissing(cap_ref))
}

/// Everything the registry knows about `capability`, as a catalogue source.
pub async fn registered_source(
    client: &Client,
    capability: &CapabilityRef,
) -> Option<CatalogueSource> {
    let registry = client.registry().lock().await;
    registry.get(capability).map(CatalogueSource::from_record)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NATIVE: &str = r#"{
        "capability": "ferridis://public.ferridis.io/ferridis/fs@v1",
        "manifestUrl": "http://127.0.0.1:7821/manifest.json",
        "baseUrl": "http://127.0.0.1:7821/"
    }"#;

    #[test]
    fn parses_a_well_formed_native_entry() {
        let file = AdaptersFile::parse(format!("[{NATIVE}]").as_bytes()).expect("parses");
        assert_eq!(file.adapters().len(), 1);
        assert!(matches!(
            &file.adapters()[0],
            AdapterConfig::Native { capability, .. }
                if capability == "ferridis://public.ferridis.io/ferridis/fs@v1"
        ));
        assert!(file.caller_overrides().is_empty());
    }

    #[test]
    fn parses_mcp_sse_entry() {
        let file = AdaptersFile::parse(br#"[{ "mcpSseUrl": "http://192.168.1.190:8765/sse" }]"#)
            .expect("parses");
        assert!(matches!(
            &file.adapters()[0],
            AdapterConfig::McpSse { mcp_sse_url } if mcp_sse_url == "http://192.168.1.190:8765/sse"
        ));
    }

    #[test]
    fn parses_mcp_stdio_entry() {
        let file = AdaptersFile::parse(
            br#"[{ "mcpStdioCommand": "uvx", "mcpStdioArgs": ["semble[mcp]", "semble"] }]"#,
        )
        .expect("parses");
        assert!(matches!(
            &file.adapters()[0],
            AdapterConfig::McpStdio { mcp_stdio_command, mcp_stdio_args, .. }
                if mcp_stdio_command == "uvx" && mcp_stdio_args.len() == 2
        ));
    }

    #[test]
    fn parses_mixed_entries() {
        let doc = format!(r#"[{NATIVE}, {{ "mcpSseUrl": "http://192.168.1.190:8765/sse" }}]"#);
        let file = AdaptersFile::parse(doc.as_bytes()).expect("parses");
        assert!(matches!(file.adapters()[0], AdapterConfig::Native { .. }));
        assert!(matches!(file.adapters()[1], AdapterConfig::McpSse { .. }));
    }

    /// PTC-1: the object form carries per-tool caller overrides.
    #[test]
    fn parses_caller_overrides_keyed_by_tool_name() {
        let doc = format!(
            r#"{{ "adapters": [{NATIVE}],
                 "callers": {{ "ferridis_ferridis_fs_v1_list_dir": "code",
                              "ferridis_ferridis_fs_v1_move_file": "direct" }} }}"#
        );
        let file = AdaptersFile::parse(doc.as_bytes()).expect("parses");
        assert_eq!(file.adapters().len(), 1);
        let name = |s: &str| ToolName::parse(s).expect("test name is valid");
        assert_eq!(
            file.caller_overrides()
                .get(&name("ferridis_ferridis_fs_v1_list_dir")),
            Some(CallerPolicy::Code)
        );
        assert_eq!(
            file.caller_overrides()
                .get(&name("ferridis_ferridis_fs_v1_move_file")),
            Some(CallerPolicy::Direct)
        );
        assert_eq!(file.caller_overrides().get(&name("ferridis_other")), None);
    }

    #[test]
    fn rejects_an_unknown_top_level_key() {
        let doc = format!(r#"{{ "adapters": [{NATIVE}], "caller": {{}} }}"#);
        assert!(matches!(
            AdaptersFile::parse(doc.as_bytes()),
            Err(AdaptersFileError::Parse { .. })
        ));
    }

    #[test]
    fn rejects_a_callers_key_that_is_not_a_tool_name() {
        let doc = format!(r#"{{ "adapters": [{NATIVE}], "callers": {{ "list dir": "code" }} }}"#);
        assert!(matches!(
            AdaptersFile::parse(doc.as_bytes()),
            Err(AdaptersFileError::InvalidToolName { key, .. }) if key == "list dir"
        ));
    }

    #[test]
    fn rejects_an_unknown_policy_value() {
        let doc = format!(r#"{{ "adapters": [{NATIVE}], "callers": {{ "ferridis_x": "both" }} }}"#);
        assert!(AdaptersFile::parse(doc.as_bytes()).is_err());
    }

    #[test]
    fn surfaces_missing_file_as_io_error() {
        let err = AdaptersFile::load(Path::new("/no/such/path.json")).expect_err("no such file");
        assert!(matches!(err, AdaptersFileError::Io { .. }));
    }

    #[test]
    fn a_parse_error_from_a_file_names_the_file() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("adapters.json");
        std::fs::write(&path, "not json").expect("write");
        let err = AdaptersFile::load(&path).expect_err("not json");
        assert!(err.to_string().contains("adapters.json"), "{err}");
    }
}
