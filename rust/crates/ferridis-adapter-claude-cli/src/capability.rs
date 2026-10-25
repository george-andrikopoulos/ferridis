//! The [`ClaudeCliCapability`] — wraps a `claude` (Claude Code CLI)
//! invocation as a stream-kind Ferridis capability.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use ferridis_adapter_sdk::{Capability, DispatchError, IntentStream, SchemaSource};
use ferridis_core::{IntentVerb, Manifest};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::input::{
    AllowedCwd, AllowedRoots, InputError, Model, ModelAllowList, Prompt, SessionId,
};

/// The manifest id this capability publishes under.
pub const CAPABILITY_ID: &str = "personal.claude-cli.v1";

/// Manifest JSON the adapter serves. Two stream-kind intents.
const MANIFEST_JSON: &str = r##"{
    "ferridis_version": "0.3",
    "id": "personal.claude-cli.v1",
    "name": "Claude Code CLI",
    "category": "ai-agent",
    "summary": "Drive a non-interactive Claude Code session over Ferridis. Streams the CLI's stream-json events as ordered chunks. Stopgap for remote-controlling Claude Code while editor agent panels lack a public extension API.",
    "intents": [
        {
            "verb": "submit-prompt",
            "kind": "stream",
            "chunk_schema_url": "https://ferridis.dev/schemas/personal/claude-cli/v1/event.json"
        },
        {
            "verb": "resume-session",
            "kind": "stream",
            "chunk_schema_url": "https://ferridis.dev/schemas/personal/claude-cli/v1/event.json"
        }
    ],
    "schema": {
        "type": "openapi-3",
        "url": "https://ferridis.dev/schemas/personal/claude-cli/v1/openapi.yaml"
    },
    "tiers": ["native"],
    "auth": { "type": "none" }
}"##;

/// Embedded OpenAPI 3 document the adapter serves at `/schema`. A tiny
/// outline: the real source of truth for argument shapes is the
/// per-intent input newtypes in [`crate::input`].
///
/// JSON rather than YAML so `ferridis-protocol::ResponseSchemas` can read
/// it without a YAML dependency (decision PTC-2). Both intents are
/// stream-kind and answer with `text/event-stream`, so neither declares
/// a JSON response schema. Each chunk's shape is the manifest's
/// `chunk_schema_url`.
const SCHEMA_BODY: &str = r##"{
  "openapi": "3.0.0",
  "info": { "title": "Claude Code CLI capability", "version": "1.0.0" },
  "paths": {
    "/intents/submit-prompt": {
      "post": {
        "summary": "Start a fresh non-interactive Claude Code session and stream its stream-json events.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["prompt"], "additionalProperties": false,
          "properties": {
            "prompt": { "type": "string", "minLength": 1 },
            "cwd": { "type": "string", "description": "Absolute path; must lie within an operator-configured allowed root." },
            "model": { "type": "string", "description": "Model alias; must be in the operator-configured allow-list." },
            "session_id": { "type": "string", "format": "uuid" }
          }
        } } } },
        "responses": { "200": { "description": "SSE stream of Claude Code stream-json events.", "content": { "text/event-stream": {} } } }
      }
    },
    "/intents/resume-session": {
      "post": {
        "summary": "Resume an existing Claude Code session by id and stream its stream-json events.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["session_id", "prompt"], "additionalProperties": false,
          "properties": {
            "session_id": { "type": "string", "format": "uuid" },
            "prompt": { "type": "string", "minLength": 1 },
            "cwd": { "type": "string" },
            "model": { "type": "string" }
          }
        } } } },
        "responses": { "200": { "description": "SSE stream of Claude Code stream-json events.", "content": { "text/event-stream": {} } } }
      }
    }
  }
}"##;

/// Configuration the operator hands to [`ClaudeCliCapability::new`].
#[derive(Debug, Clone)]
pub struct ClaudeCliConfig {
    claude_binary: PathBuf,
    allowed_cwds: AllowedRoots,
    allowed_models: ModelAllowList,
    default_cwd: Option<PathBuf>,
    default_model: Option<Model>,
}

impl ClaudeCliConfig {
    /// New config with the default `claude` binary discovered on PATH.
    pub fn new() -> Self {
        Self {
            claude_binary: PathBuf::from("claude"),
            allowed_cwds: AllowedRoots::empty(),
            allowed_models: ModelAllowList::standard(),
            default_cwd: None,
            default_model: None,
        }
    }

    /// Override the `claude` binary path (useful for tests with a
    /// stub script, or pinning to a specific install).
    #[must_use]
    pub fn with_binary(mut self, path: impl Into<PathBuf>) -> Self {
        self.claude_binary = path.into();
        self
    }

    /// Allowed-cwd configuration. See [`AllowedRoots`].
    #[must_use]
    pub fn with_allowed_cwds(mut self, roots: AllowedRoots) -> Self {
        self.allowed_cwds = roots;
        self
    }

    /// Allowed-model configuration. See [`ModelAllowList`].
    #[must_use]
    pub fn with_allowed_models(mut self, allow: ModelAllowList) -> Self {
        self.allowed_models = allow;
        self
    }

    /// The cwd to use when the client doesn't supply one. Must
    /// canonicalise inside one of the allowed roots — checked at
    /// dispatch time, not now, so callers can supply a default
    /// before the allow-list is finalised.
    #[must_use]
    pub fn with_default_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.default_cwd = Some(cwd.into());
        self
    }

    /// Default model when the client doesn't supply one. Must be in
    /// the allow-list.
    #[must_use]
    pub fn with_default_model(mut self, model: Model) -> Self {
        self.default_model = Some(model);
        self
    }
}

impl Default for ClaudeCliConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Wraps the Claude Code CLI as a Ferridis capability.
pub struct ClaudeCliCapability {
    manifest: Manifest,
    config: Arc<ClaudeCliConfig>,
}

impl ClaudeCliCapability {
    /// Build the capability. The manifest is parsed once here so any
    /// shape regression fails fast at construction.
    pub fn new(config: ClaudeCliConfig) -> Self {
        let manifest = Manifest::parse(MANIFEST_JSON)
            .expect("the adapter's bundled manifest must parse — fix the constant");
        Self {
            manifest,
            config: Arc::new(config),
        }
    }
}

/// Per-intent argument struct for `submit-prompt`. Deserialized
/// at dispatch time; every field validated through the typestate
/// newtypes before any process spawns.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitPromptArgs {
    prompt: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

/// Per-intent argument struct for `resume-session`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeSessionArgs {
    session_id: String,
    prompt: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[async_trait]
impl Capability for ClaudeCliCapability {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn schema(&self) -> SchemaSource {
        SchemaSource::Embedded {
            content_type: "application/json".into(),
            body: SCHEMA_BODY.into(),
        }
    }

    async fn dispatch(
        &self,
        intent: &IntentVerb,
        _body: serde_json::Value,
    ) -> Result<serde_json::Value, DispatchError> {
        // Both declared intents are stream-kind — the SDK routes
        // them through `dispatch_stream`. A request-kind call
        // means the manifest and trait impl have drifted; treat
        // it as UnsupportedIntent rather than panicking.
        Err(DispatchError::UnsupportedIntent(intent.clone()))
    }

    async fn dispatch_stream(
        &self,
        intent: &IntentVerb,
        body: serde_json::Value,
    ) -> Result<IntentStream, DispatchError> {
        match intent.as_str() {
            "submit-prompt" => {
                let args: SubmitPromptArgs = serde_json::from_value(body).map_err(|e| {
                    DispatchError::InvalidRequest(format!("submit-prompt args: {e}"))
                })?;
                self.spawn_claude(SpawnArgs {
                    resume_id: None,
                    new_session_id: args.session_id.as_deref(),
                    prompt: &args.prompt,
                    cwd: args.cwd.as_deref(),
                    model: args.model.as_deref(),
                })
                .await
            }
            "resume-session" => {
                let args: ResumeSessionArgs = serde_json::from_value(body).map_err(|e| {
                    DispatchError::InvalidRequest(format!("resume-session args: {e}"))
                })?;
                self.spawn_claude(SpawnArgs {
                    resume_id: Some(&args.session_id),
                    new_session_id: None,
                    prompt: &args.prompt,
                    cwd: args.cwd.as_deref(),
                    model: args.model.as_deref(),
                })
                .await
            }
            _ => Err(DispatchError::UnsupportedIntent(intent.clone())),
        }
    }
}

struct SpawnArgs<'a> {
    /// `Some(id)` => `-r <id>` (resume).
    resume_id: Option<&'a str>,
    /// `Some(id)` => `--session-id <id>` (new session with caller-chosen id).
    /// Ignored when `resume_id` is set.
    new_session_id: Option<&'a str>,
    prompt: &'a str,
    cwd: Option<&'a str>,
    model: Option<&'a str>,
}

impl ClaudeCliCapability {
    /// Resolve every input through its typestate, then spawn
    /// `claude` and return a stream that yields one JSON chunk per
    /// stdout line. The child is owned by the stream — when the
    /// caller drops the stream the child's pipes close and the
    /// process winds down on its next read.
    async fn spawn_claude(&self, args: SpawnArgs<'_>) -> Result<IntentStream, DispatchError> {
        // Validate everything before the spawn.
        let prompt = Prompt::parse(args.prompt).map_err(from_input)?;

        let resume = match args.resume_id {
            Some(s) => Some(SessionId::parse(s).map_err(from_input)?),
            None => None,
        };
        let new_session = match args.new_session_id {
            Some(s) => Some(SessionId::parse(s).map_err(from_input)?),
            None => None,
        };

        let cwd = match args.cwd {
            Some(s) => Some(AllowedCwd::parse(s, &self.config.allowed_cwds).map_err(from_input)?),
            None => match &self.config.default_cwd {
                Some(p) => Some(
                    AllowedCwd::parse(
                        p.to_str().ok_or_else(|| {
                            DispatchError::Internal("default cwd is not valid UTF-8".into())
                        })?,
                        &self.config.allowed_cwds,
                    )
                    .map_err(from_input)?,
                ),
                None => None,
            },
        };

        let model = match args.model {
            Some(s) => Some(Model::parse(s, &self.config.allowed_models).map_err(from_input)?),
            None => self.config.default_model.clone(),
        };

        // Compose the argv.
        let mut cmd = Command::new(&self.config.claude_binary);
        cmd.arg("-p")
            .arg(prompt.as_str())
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose");
        if let Some(id) = &resume {
            cmd.arg("-r").arg(id.as_str());
        } else if let Some(id) = &new_session {
            cmd.arg("--session-id").arg(id.as_str());
        }
        if let Some(m) = &model {
            cmd.arg("--model").arg(m.as_arg());
        }
        if let Some(c) = &cwd {
            cmd.current_dir(c.as_path());
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The child process is independent of the adapter's
            // tokio runtime — kill the process group if the parent
            // dies, so a stuck `claude` doesn't outlive us.
            .kill_on_drop(true);

        tracing::info!(
            binary = %self.config.claude_binary.display(),
            resume = ?resume.as_ref().map(SessionId::as_str),
            new_session = ?new_session.as_ref().map(SessionId::as_str),
            model = ?model.as_ref().map(Model::as_arg),
            cwd = ?cwd.as_ref().map(|c| c.as_path().display().to_string()),
            "spawning claude"
        );

        let mut child = cmd.spawn().map_err(|e| {
            DispatchError::Internal(format!(
                "failed to spawn `{}`: {e}",
                self.config.claude_binary.display()
            ))
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| DispatchError::Internal("claude child has no stdout pipe".into()))?;
        let mut stderr_pipe = child.stderr.take();

        let stream = async_stream::try_stream! {
            let mut reader = BufReader::new(stdout).lines();
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        // Each stream-json line is a JSON object.
                        // If a line ever fails to parse, surface it
                        // as a structured chunk rather than killing
                        // the stream — Claude's CLI occasionally
                        // emits non-JSON warnings on stdout that
                        // shouldn't take the session down.
                        let parsed = serde_json::from_str::<serde_json::Value>(&line)
                            .unwrap_or_else(|_| {
                                serde_json::json!({
                                    "type": "raw-line",
                                    "line": line,
                                })
                            });
                        yield parsed;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        Err(DispatchError::Internal(format!(
                            "stdout read error: {e}"
                        )))?;
                    }
                }
            }

            // Reap the child + capture stderr for diagnostics.
            // Failure here is structural (couldn't wait on the
            // process); a non-zero exit code is surfaced as a
            // final chunk so the caller's stream still terminates
            // cleanly via the SDK's `end` event.
            let status = child.wait().await.map_err(|e| {
                DispatchError::Internal(format!("wait on claude child: {e}"))
            })?;

            if !status.success() {
                let mut stderr_text = String::new();
                if let Some(mut s) = stderr_pipe.take() {
                    use tokio::io::AsyncReadExt;
                    let _ = s.read_to_string(&mut stderr_text).await;
                }
                yield serde_json::json!({
                    "type": "adapter-event",
                    "subtype": "child-exit",
                    "exit_code": status.code(),
                    "stderr": stderr_text,
                });
            }
        };

        Ok(Box::pin(stream))
    }
}

fn from_input(e: InputError) -> DispatchError {
    DispatchError::InvalidRequest(e.to_string())
}
