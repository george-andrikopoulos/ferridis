//! Every way a programmatic-tool-calling exchange can fail.

use url::Url;

use crate::container::ContainerExpiry;
use crate::ids::ContainerId;

/// A request parameter that cannot be sent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParameterError {
    /// The model id is empty.
    #[error("model id is empty")]
    EmptyModel,
    /// `max_tokens` is zero.
    #[error("max_tokens must be at least 1")]
    ZeroMaxTokens,
    /// The turn limit is zero.
    #[error("the turn limit must be at least 1")]
    ZeroMaxTurns,
    /// The user prompt is empty.
    #[error("the user prompt is empty")]
    EmptyPrompt,
    /// The system prompt is empty (omit it instead).
    #[error("the system prompt is empty; omit it instead")]
    EmptySystemPrompt,
    /// The API key is empty.
    #[error("the API key is empty")]
    EmptyApiKey,
}

/// A Messages API response that breaks the programmatic-tool-calling
/// contract. Each is a reason not to continue the turn, because
/// continuing would send a request the API rejects or answer calls that
/// were never made.
#[derive(Debug, thiserror::Error)]
pub enum ResponseError {
    /// An identifier field was empty.
    #[error("{field} is empty")]
    EmptyId {
        /// Which field.
        field: &'static str,
    },
    /// `stop_reason` was `tool_use` but no `tool_use` block was present.
    #[error("stop_reason is tool_use but the response holds no tool_use block")]
    ToolUseWithoutCalls,
    /// Two `tool_use` blocks share an id, so "one result per id" has no
    /// single meaning.
    #[error("tool_use id {id} appears more than once")]
    DuplicateToolUseId {
        /// The repeated id.
        id: String,
    },
    /// A call from code arrived without the container it runs in, so
    /// the continuation could not name it.
    #[error("tool_use {tool_use_id} was called from code but the response names no container")]
    ProgrammaticCallWithoutContainer {
        /// The call's id.
        tool_use_id: String,
    },
    /// The container's `expires_at` is not an RFC 3339 timestamp.
    #[error("container expires_at {value:?} is not RFC 3339: {source}")]
    InvalidExpiry {
        /// The raw value.
        value: String,
        /// The parse error.
        #[source]
        source: time::error::Parse,
    },
    /// A content block of a known type did not have that type's shape.
    #[error("malformed {block_type} block: {source}")]
    MalformedBlock {
        /// The block's `type`.
        block_type: &'static str,
        /// The decode error.
        #[source]
        source: serde_json::Error,
    },
}

/// Why a programmatic-tool-calling exchange stopped.
#[derive(Debug, thiserror::Error)]
pub enum PtcError {
    /// The API key environment variable is unset or not Unicode.
    #[error("{var} is not set")]
    MissingApiKey {
        /// The variable read.
        var: &'static str,
    },
    /// A parameter was rejected before anything was sent.
    #[error(transparent)]
    Parameter(#[from] ParameterError),
    /// The HTTP request failed before a response arrived.
    #[error("POST {endpoint}: {source}")]
    Http {
        /// The endpoint posted to.
        endpoint: Url,
        /// The transport error.
        #[source]
        source: reqwest::Error,
    },
    /// The API answered with an error status.
    #[error("Messages API returned {status} {error_type}: {message}")]
    Api {
        /// The HTTP status.
        status: u16,
        /// The API's error `type`, or `unknown` when the body had none.
        error_type: String,
        /// The API's error message, or the raw body.
        message: String,
    },
    /// The response body is not a Messages API response.
    #[error("decode Messages API response: {source}")]
    Decode {
        /// The decode error.
        #[source]
        source: serde_json::Error,
    },
    /// The response is a Messages API response that breaks the contract.
    #[error(transparent)]
    Response(#[from] ResponseError),
    /// The container the pending calls run in has expired, so the
    /// continuation was not sent: the API would reject it, and the code
    /// that made the calls is gone.
    #[error("container {container} expired at {expired_at}; continuation not sent")]
    ContainerExpired {
        /// The expired container.
        container: ContainerId,
        /// When it expired.
        expired_at: ContainerExpiry,
    },
    /// The run reached its turn limit without a final answer.
    #[error("no final answer within {limit} turns")]
    TurnLimit {
        /// The limit reached.
        limit: u32,
    },
}
