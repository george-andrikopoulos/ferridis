//! What is sent: the conversation so far and the request around it.

use std::num::NonZeroU32;
use std::sync::Arc;

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::error::ParameterError;
use crate::ids::ContainerId;
use crate::response::ToolResultsMessage;
use crate::tools::ToolSet;

/// The model PTC conversations use unless told otherwise.
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";

/// `max_tokens` unless told otherwise. Low enough for a non-streaming
/// request to finish well inside HTTP timeouts.
pub const DEFAULT_MAX_TOKENS: u32 = 16_000;

/// A Messages API model id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelId(String);

impl ModelId {
    /// Accept a non-empty model id.
    ///
    /// # Errors
    ///
    /// [`ParameterError::EmptyModel`].
    pub fn parse(raw: impl Into<String>) -> Result<Self, ParameterError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(ParameterError::EmptyModel);
        }
        Ok(Self(raw))
    }

    /// [`DEFAULT_MODEL`].
    #[must_use]
    pub fn default_model() -> Self {
        Self(DEFAULT_MODEL.to_owned())
    }

    /// Borrow the id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The per-response output cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxTokens(NonZeroU32);

impl MaxTokens {
    /// Accept a non-zero cap.
    ///
    /// # Errors
    ///
    /// [`ParameterError::ZeroMaxTokens`].
    pub fn new(n: u32) -> Result<Self, ParameterError> {
        NonZeroU32::new(n)
            .map(Self)
            .ok_or(ParameterError::ZeroMaxTokens)
    }

    /// The cap.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl Default for MaxTokens {
    fn default() -> Self {
        Self(NonZeroU32::new(DEFAULT_MAX_TOKENS).expect("DEFAULT_MAX_TOKENS is a non-zero literal"))
    }
}

macro_rules! prompt_text {
    ($(#[$doc:meta])* $name:ident, $empty:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name(String);

        impl $name {
            /// Accept non-empty text.
            ///
            /// # Errors
            ///
            #[doc = concat!("[`ParameterError::", stringify!($empty), "`].")]
            pub fn new(text: impl Into<String>) -> Result<Self, ParameterError> {
                let text = text.into();
                if text.trim().is_empty() {
                    return Err(ParameterError::$empty);
                }
                Ok(Self(text))
            }

            /// Borrow the text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

prompt_text!(
    /// The user message that opens a conversation.
    UserPrompt,
    EmptyPrompt
);

prompt_text!(
    /// A system prompt.
    SystemPrompt,
    EmptySystemPrompt
);

/// One message of the history.
#[derive(Debug)]
pub(crate) enum Message {
    /// The opening user prompt.
    User(UserPrompt),
    /// An assistant response's content, kept verbatim: the API requires
    /// `server_tool_use` and `tool_use` blocks back exactly as sent.
    Assistant(Vec<Value>),
    /// Answers to the previous assistant message's calls, and nothing else.
    ToolResults(ToolResultsMessage),
}

impl Serialize for Message {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(2))?;
        match self {
            Self::User(prompt) => {
                map.serialize_entry("role", "user")?;
                map.serialize_entry("content", prompt.as_str())?;
            }
            Self::Assistant(content) => {
                map.serialize_entry("role", "assistant")?;
                map.serialize_entry("content", content)?;
            }
            Self::ToolResults(results) => {
                map.serialize_entry("role", "user")?;
                map.serialize_entry("content", results)?;
            }
        }
        map.end()
    }
}

/// A conversation not yet started: model, limits, system prompt and the
/// tools array every turn will carry. Started with
/// [`PtcClient::start`](crate::PtcClient::start); from then on its history
/// only grows inside the turn types, so it can never be continued out of
/// order.
#[derive(Debug)]
pub struct Conversation {
    model: ModelId,
    max_tokens: MaxTokens,
    system: Option<SystemPrompt>,
    tools: Arc<ToolSet>,
    messages: Vec<Message>,
}

impl Conversation {
    /// A conversation offering `tools`.
    #[must_use]
    pub fn new(model: ModelId, max_tokens: MaxTokens, tools: Arc<ToolSet>) -> Self {
        Self {
            model,
            max_tokens,
            system: None,
            tools,
            messages: Vec::new(),
        }
    }

    /// Add a system prompt.
    #[must_use]
    pub fn with_system(mut self, system: SystemPrompt) -> Self {
        self.system = Some(system);
        self
    }

    pub(crate) fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub(crate) fn body<'c>(&'c self, container: Option<&'c ContainerId>) -> RequestBody<'c> {
        RequestBody {
            model: self.model.as_str(),
            max_tokens: self.max_tokens.get(),
            system: self.system.as_ref().map(SystemPrompt::as_str),
            messages: &self.messages,
            tools: &self.tools,
            container,
            cache_control: CacheControl,
        }
    }
}

/// The request body. Has no `tool_choice`, `disable_parallel_tool_use`
/// or sampling fields: forced tool choice and disabled parallel use are
/// rejected with programmatic calling, and the current API version
/// rejects sampling parameters.
///
/// Every request asks for automatic prompt caching (the top-level
/// `cache_control`): the API moves that breakpoint to the end of the
/// growing conversation, so each turn reads the previous turn's prefix.
/// The tools array carries its own breakpoint (see
/// [`ToolSet`](crate::ToolSet)), so separate conversations offering the
/// same tools share it too.
#[derive(Debug, Serialize)]
pub(crate) struct RequestBody<'c> {
    model: &'c str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'c str>,
    messages: &'c [Message],
    tools: &'c ToolSet,
    #[serde(skip_serializing_if = "Option::is_none")]
    container: Option<&'c ContainerId>,
    cache_control: CacheControl,
}

/// A prompt-caching breakpoint with the default five-minute lifetime.
///
/// Five minutes, not an hour: a cache read refreshes the entry, turns of
/// one conversation start seconds apart, and the one-hour write costs
/// twice the base input price against 1.25 times for five minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CacheControl;

impl Serialize for CacheControl {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(1))?;
        map.serialize_entry("type", "ephemeral")?;
        map.end()
    }
}
