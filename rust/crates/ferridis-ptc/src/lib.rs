//! Programmatic tool calling (PTC) for Ferridis.
//!
//! Offers Ferridis-registered capabilities to Claude as Messages API
//! custom tools, some of which the model calls from code running in its
//! code-execution container instead of one model turn per call. Results
//! of those calls go to the code, not into the model's context.
//!
//! The pieces, in the order a host uses them:
//!
//! 1. [`ToolPlan::build`] projects the client's
//!    [`Catalogue`](ferridis_client::catalogue::Catalogue) into tools, each
//!    with an effective caller policy (see [`ResolvedPolicy`]).
//! 2. [`Executor`] answers calls: a call the tool's policy does not allow
//!    gets an `is_error` result and never reaches the adapter; an allowed
//!    one goes through [`Client::dispatch_from`](ferridis_client::Client::dispatch_from),
//!    which records the caller on the tier-used telemetry.
//! 3. [`PtcClient`] talks to the Messages API. Its responses come back as
//!    a [`Turn`]: a typestate whose pending-calls states can only be
//!    continued with exactly one `tool_result` per pending call.
//! 4. [`run`] drives a conversation to its final turn.
//!
//! Server-side refusal fallback is not enabled: a refusal ends the run
//! with [`StopReason::Refusal`].

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod client;
mod container;
mod error;
mod executor;
mod ids;
mod policy;
mod request;
mod response;
mod tools;

#[cfg(test)]
mod properties;

pub use client::{API_KEY_VAR, API_VERSION, ApiKey, DEFAULT_ENDPOINT, PtcClient};
pub use container::{Clock, Container, ContainerExpiry, SystemClock};
pub use error::{ParameterError, PtcError, ResponseError};
pub use executor::{CallCounts, Dispatcher, Executor, MaxTurns, RunOutcome, run};
pub use ids::{ContainerId, ServerToolUseId, ToolUseId};
pub use policy::{PolicySource, ResolvedPolicy, SchemaHazard};
pub use request::{
    Conversation, DEFAULT_MAX_TOKENS, DEFAULT_MODEL, MaxTokens, ModelId, SystemPrompt, UserPrompt,
};
pub use response::{
    AnsweredCalls, CallHandler, Caller, CodeRun, ContainerState, FinalTurn, NoContainer,
    PausedTurn, PendingCall, PendingCalls, PendingDirectCalls, PendingProgrammaticCalls,
    StopReason, ToolOutput, ToolResultsMessage, Turn, Usage, WithContainer,
};
pub use tools::{CODE_EXECUTION_TOOL_TYPE, PlannedTool, ToolPlan, ToolSet};
