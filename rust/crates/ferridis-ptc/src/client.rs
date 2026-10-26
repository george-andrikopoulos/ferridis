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
use crate::retry::RetryPolicy;

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
///
/// Retry delays run on the tokio timer, so requests need a runtime with
/// time enabled (`#[tokio::main]` enables it).
#[derive(Debug, Clone)]
pub struct PtcClient {
    http: reqwest::Client,
    api_key: ApiKey,
    workspace: Option<WorkspaceId>,
    endpoint: Url,
    clock: Arc<dyn Clock>,
    retry: RetryPolicy,
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
            retry: RetryPolicy::default(),
        }
    }

    /// Retry transient failures under `retry` instead of the default.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
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
    ///
    /// A transient failure is retried under the [`RetryPolicy`], with the
    /// expiry check repeated before each retry, so a wait never carries a
    /// continuation past its container.
    pub(crate) async fn exchange(
        &self,
        conversation: Conversation,
        container: Option<&Container>,
    ) -> Result<Turn, PtcError> {
        let mut made = 0;
        let raw = loop {
            if let Some(c) = container {
                c.ensure_live(self.clock.now())?;
            }
            let failure = match self.post_once(&conversation, container).await {
                Ok(raw) => break raw,
                Err(failure) => failure,
            };
            let delay = match failure.transience {
                Transience::Transient { retry_after } => self.retry.delay(made, retry_after),
                Transience::Permanent => None,
            };
            let Some(delay) = delay else {
                return Err(failure.error);
            };
            made += 1;
            tracing::warn!(retry = made, of = self.retry.retries(), ?delay, error = %failure.error,
                "transient Messages API failure; retrying");
            tokio::time::sleep(delay).await;
        };
        Ok(raw.into_turn(conversation)?)
    }

    /// One POST of `conversation`, classified on failure.
    async fn post_once(
        &self,
        conversation: &Conversation,
        container: Option<&Container>,
    ) -> Result<RawResponse, Failure> {
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
            .map_err(|source| self.http_failure(source))?;
        let status = response.status();
        let retry_after = retry_after(response.headers());
        let body = response
            .bytes()
            .await
            .map_err(|source| self.http_failure(source))?;
        if !status.is_success() {
            let transience = if status.as_u16() == 429 || status.is_server_error() {
                Transience::Transient { retry_after }
            } else {
                Transience::Permanent
            };
            return Err(Failure {
                error: api_error(status.as_u16(), &body),
                transience,
            });
        }
        serde_json::from_slice(&body).map_err(|source| Failure {
            error: PtcError::Decode { source },
            transience: Transience::Permanent,
        })
    }

    /// A transport error: transient only when no connection was made, so
    /// the request cannot have reached the API. A timeout or a dropped
    /// response may have been processed, and is returned.
    fn http_failure(&self, source: reqwest::Error) -> Failure {
        let transience = if source.is_connect() {
            Transience::Transient { retry_after: None }
        } else {
            Transience::Permanent
        };
        Failure {
            error: self.http_error(source),
            transience,
        }
    }

    fn http_error(&self, source: reqwest::Error) -> PtcError {
        PtcError::Http {
            endpoint: self.endpoint.clone(), // allow:clone — the error outlives the client borrow
            source,
        }
    }
}

/// A failed attempt and whether it is worth another.
struct Failure {
    error: PtcError,
    transience: Transience,
}

enum Transience {
    Transient { retry_after: Option<Duration> },
    Permanent,
}

/// `retry-after` in its delay-seconds form. The HTTP-date form, or
/// anything unparseable, reads as absent and falls back to backoff.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
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
