//! The Messages API over the workspace's `reqwest`.

use std::sync::Arc;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use url::Url;

use crate::container::{Clock, Container, SystemClock};
use crate::error::{ParameterError, PtcError};
use crate::request::{Conversation, Message, UserPrompt};
use crate::response::{RawResponse, Turn};

/// The `anthropic-version` header sent on every request.
pub const API_VERSION: &str = "2026-10-21";

/// The Messages endpoint.
pub const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// The environment variable [`ApiKey::from_env`] reads.
pub const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";

/// The environment variable [`WorkspaceId::from_env`] reads, as the
/// Anthropic CLI does.
pub const WORKSPACE_ID_VAR: &str = "ANTHROPIC_WORKSPACE_ID";

/// Upper bound on one request. A pending programmatic call times out
/// server-side after about four minutes, so a response that takes longer
/// than this is not coming.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// An Anthropic API key. Never printed.
#[derive(Clone)]
pub struct ApiKey(SecretString);

impl ApiKey {
    /// Accept a non-empty key.
    ///
    /// # Errors
    ///
    /// [`ParameterError::EmptyApiKey`].
    pub fn new(raw: String) -> Result<Self, ParameterError> {
        if raw.trim().is_empty() {
            return Err(ParameterError::EmptyApiKey);
        }
        Ok(Self(SecretString::new(raw)))
    }

    /// Read [`API_KEY_VAR`].
    ///
    /// # Errors
    ///
    /// [`PtcError::MissingApiKey`] when unset; [`ParameterError::EmptyApiKey`]
    /// when empty.
    pub fn from_env() -> Result<Self, PtcError> {
        let raw =
            std::env::var(API_KEY_VAR).map_err(|_| PtcError::MissingApiKey { var: API_KEY_VAR })?;
        Ok(Self::new(raw)?)
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// The workspace a request runs in, sent as `anthropic-workspace-id`.
/// Required with a personal or service-account key that is not scoped
/// to a workspace; a workspace-scoped key needs none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// Accept a non-empty ID.
    ///
    /// # Errors
    ///
    /// [`ParameterError::EmptyWorkspaceId`].
    pub fn new(raw: impl Into<String>) -> Result<Self, ParameterError> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(ParameterError::EmptyWorkspaceId);
        }
        Ok(Self(raw))
    }

    /// Read [`WORKSPACE_ID_VAR`]: `None` when unset, as for a
    /// workspace-scoped key.
    ///
    /// # Errors
    ///
    /// [`ParameterError::EmptyWorkspaceId`] when set but empty.
    pub fn from_env() -> Result<Option<Self>, ParameterError> {
        std::env::var(WORKSPACE_ID_VAR)
            .ok()
            .map(Self::new)
            .transpose()
    }

    /// The ID.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A Messages API client for programmatic-tool-calling conversations.
#[derive(Debug, Clone)]
pub struct PtcClient {
    http: reqwest::Client,
    api_key: ApiKey,
    workspace: Option<WorkspaceId>,
    endpoint: Url,
    clock: Arc<dyn Clock>,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    error: ApiErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ApiErrorDetail {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}

impl PtcClient {
    /// A client posting to [`DEFAULT_ENDPOINT`] on the system clock.
    ///
    /// # Panics
    ///
    /// If the TLS backend cannot initialise, as `reqwest::Client::new` does.
    #[must_use]
    pub fn new(api_key: ApiKey) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("reqwest client with a timeout and the workspace TLS backend builds"),
            api_key,
            workspace: None,
            endpoint: Url::parse(DEFAULT_ENDPOINT)
                .expect("DEFAULT_ENDPOINT is a valid URL literal"),
            clock: Arc::new(SystemClock),
        }
    }

    /// Post somewhere else: a proxy, or a replay server in tests.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: Url) -> Self {
        self.endpoint = endpoint;
        self
    }

    /// Run every request in `workspace`.
    #[must_use]
    pub fn with_workspace(mut self, workspace: WorkspaceId) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Check container expiry against another clock.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Open `conversation` with `prompt`.
    ///
    /// # Errors
    ///
    /// [`PtcError::Http`], [`PtcError::Api`], [`PtcError::Decode`] or
    /// [`PtcError::Response`] naming what failed.
    pub async fn start(
        &self,
        mut conversation: Conversation,
        prompt: UserPrompt,
    ) -> Result<Turn, PtcError> {
        conversation.push(Message::User(prompt));
        self.exchange(conversation, None).await
    }

    /// Post `conversation`, naming `container` when there is one, and
    /// append the response. Refuses before sending if the container has
    /// expired.
    pub(crate) async fn exchange(
        &self,
        conversation: Conversation,
        container: Option<&Container>,
    ) -> Result<Turn, PtcError> {
        if let Some(c) = container {
            c.ensure_live(self.clock.now())?;
        }
        let mut request = self
            .http
            .post(self.endpoint.as_str())
            .header("x-api-key", self.api_key.0.expose_secret())
            .header("anthropic-version", API_VERSION);
        if let Some(w) = &self.workspace {
            request = request.header("anthropic-workspace-id", w.as_str());
        }
        let response = request
            .json(&conversation.body(container.map(Container::id)))
            .send()
            .await
            .map_err(|source| self.http_error(source))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|source| self.http_error(source))?;
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &body));
        }
        let raw: RawResponse =
            serde_json::from_slice(&body).map_err(|source| PtcError::Decode { source })?;
        Ok(raw.into_turn(conversation)?)
    }

    fn http_error(&self, source: reqwest::Error) -> PtcError {
        PtcError::Http {
            endpoint: self.endpoint.clone(), // allow:clone — the error outlives the client borrow
            source,
        }
    }
}

fn api_error(status: u16, body: &[u8]) -> PtcError {
    match serde_json::from_slice::<ApiErrorBody>(body) {
        Ok(b) => PtcError::Api {
            status,
            error_type: b.error.error_type,
            message: b.error.message,
        },
        Err(_) => PtcError::Api {
            status,
            error_type: "unknown".to_owned(),
            message: String::from_utf8_lossy(body).into_owned(),
        },
    }
}
