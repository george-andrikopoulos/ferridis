//! Answering calls: the caller policy gate, then dispatch through the
//! Ferridis client.

use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};

use ferridis_client::catalogue::ToolName;
use ferridis_client::{CallOrigin, Client, ClientError};
use ferridis_core::{CallerPolicy, CapabilityRef, IntentKind, IntentVerb};
use serde_json::Value;

use crate::client::PtcClient;
use crate::error::{ParameterError, PtcError};
use crate::request::{Conversation, UserPrompt};
use crate::response::{CallHandler, Caller, FinalTurn, PendingCall, ToolOutput, Turn, Usage};
use crate::tools::{PlannedTool, ToolPlan};

/// Where an allowed call goes. Implemented for [`Client`]; a host with its
/// own dispatch layer implements it too.
pub trait Dispatcher {
    /// Dispatch a request-kind intent.
    fn request(
        &self,
        origin: CallOrigin,
        capability: &CapabilityRef,
        intent: &IntentVerb,
        body: Value,
    ) -> impl Future<Output = Result<Value, ClientError>>;

    /// Dispatch a stream-kind intent and collect it into one value.
    fn stream_collected(
        &self,
        origin: CallOrigin,
        capability: &CapabilityRef,
        intent: &IntentVerb,
        body: Value,
    ) -> impl Future<Output = Result<Value, ClientError>>;
}

impl Dispatcher for Client {
    async fn request(
        &self,
        origin: CallOrigin,
        capability: &CapabilityRef,
        intent: &IntentVerb,
        body: Value,
    ) -> Result<Value, ClientError> {
        self.dispatch_from(origin, capability, intent.clone(), body) // allow:clone — dispatch takes the verb by value; the plan keeps its own
            .await
    }

    async fn stream_collected(
        &self,
        origin: CallOrigin,
        capability: &CapabilityRef,
        intent: &IntentVerb,
        body: Value,
    ) -> Result<Value, ClientError> {
        let stream = self
            .dispatch_streaming_from(origin, capability, intent.clone(), body) // allow:clone — same
            .await?;
        Ok(ferridis_client::catalogue::collect_stream(stream)
            .await?
            .into_json())
    }
}

/// What happened to the calls an executor answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallCounts {
    programmatic: u32,
    direct: u32,
    refused: u32,
    failed: u32,
}

impl CallCounts {
    /// Calls from code that reached their adapter.
    #[must_use]
    pub fn programmatic(&self) -> u32 {
        self.programmatic
    }

    /// Direct calls that reached their adapter.
    #[must_use]
    pub fn direct(&self) -> u32 {
        self.direct
    }

    /// Calls refused before dispatch: unknown tool, or a caller the tool's
    /// policy does not allow.
    #[must_use]
    pub fn refused(&self) -> u32 {
        self.refused
    }

    /// Dispatched calls whose adapter returned an error.
    #[must_use]
    pub fn failed(&self) -> u32 {
        self.failed
    }
}

impl std::ops::Sub for CallCounts {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self {
            programmatic: self.programmatic - rhs.programmatic,
            direct: self.direct - rhs.direct,
            refused: self.refused - rhs.refused,
            failed: self.failed - rhs.failed,
        }
    }
}

#[derive(Debug, Default)]
struct Counters {
    programmatic: AtomicU32,
    direct: AtomicU32,
    refused: AtomicU32,
    failed: AtomicU32,
}

impl Counters {
    fn snapshot(&self) -> CallCounts {
        CallCounts {
            programmatic: self.programmatic.load(Ordering::Relaxed),
            direct: self.direct.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        }
    }
}

/// Why a call was refused before dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Refusal {
    UnknownTool,
    CallerNotAllowed { policy: CallerPolicy },
}

/// Answers calls against a [`ToolPlan`]: unknown tools and callers the
/// tool's policy does not allow get an error result and never reach a
/// dispatcher; everything else is dispatched with its origin recorded.
#[derive(Debug)]
pub struct Executor<D> {
    tools: HashMap<ToolName, PlannedTool>,
    dispatcher: D,
    counters: Counters,
}

impl<D: Dispatcher> Executor<D> {
    /// An executor for `plan`'s tools.
    #[must_use]
    pub fn new(plan: ToolPlan, dispatcher: D) -> Self {
        let tools = plan
            .into_tools()
            .into_iter()
            .map(|t| (t.name().clone(), t)) // allow:clone — the index key and the tool each own the name
            .collect();
        Self {
            tools,
            dispatcher,
            counters: Counters::default(),
        }
    }

    /// Cumulative counts over every call this executor answered.
    #[must_use]
    pub fn counts(&self) -> CallCounts {
        self.counters.snapshot()
    }

    /// The tool `call` names, if its caller is allowed to call it.
    fn admit(&self, call: &PendingCall) -> Result<(&PlannedTool, CallOrigin), Refusal> {
        let tool = self.tools.get(call.name()).ok_or(Refusal::UnknownTool)?;
        let policy = tool.policy().policy();
        let origin = match (policy, call.caller()) {
            (CallerPolicy::Direct, Caller::Direct) => CallOrigin::ModelDirect,
            (CallerPolicy::Code, Caller::Code { .. }) => CallOrigin::ModelCode,
            _ => return Err(Refusal::CallerNotAllowed { policy }),
        };
        Ok((tool, origin))
    }

    async fn execute(&self, call: &PendingCall) -> ToolOutput {
        let (tool, origin) = match self.admit(call) {
            Ok(admitted) => admitted,
            Err(refusal) => return self.refuse(call, &refusal),
        };
        let body = call.input().clone(); // allow:clone — dispatch consumes the body; the call stays in the history
        let result = match tool.kind() {
            IntentKind::Request => {
                self.dispatcher
                    .request(origin, tool.capability(), tool.intent(), body)
                    .await
            }
            IntentKind::Stream => {
                self.dispatcher
                    .stream_collected(origin, tool.capability(), tool.intent(), body)
                    .await
            }
        };
        let counter = match origin {
            CallOrigin::ModelCode => &self.counters.programmatic,
            _ => &self.counters.direct,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        match result {
            Ok(value) => ToolOutput::Content(value.to_string()),
            Err(e) => {
                self.counters.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(tool = call.name(), caller = %call.caller(), error = %e, "tool call failed");
                ToolOutput::Error(format!("tool {} failed: {e}", call.name()))
            }
        }
    }

    fn refuse(&self, call: &PendingCall, refusal: &Refusal) -> ToolOutput {
        self.counters.refused.fetch_add(1, Ordering::Relaxed);
        let message = match refusal {
            Refusal::UnknownTool => format!("no tool named {} is registered", call.name()),
            Refusal::CallerNotAllowed { policy } => format!(
                "tool {} may only be called {}; refused a call from {}",
                call.name(),
                match policy {
                    CallerPolicy::Direct => "directly by the model",
                    CallerPolicy::Code => "from code in the code-execution container",
                },
                call.caller()
            ),
        };
        tracing::warn!(tool = call.name(), caller = %call.caller(), "{message}");
        ToolOutput::Error(message)
    }
}

impl<D: Dispatcher> CallHandler for Executor<D> {
    fn handle<'a>(&'a self, call: &'a PendingCall) -> impl Future<Output = ToolOutput> + 'a {
        self.execute(call)
    }
}

/// The most turns a run takes before giving up, so a model that keeps
/// calling tools cannot spend without bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxTurns(NonZeroU32);

impl MaxTurns {
    /// Accept a non-zero limit.
    ///
    /// # Errors
    ///
    /// [`ParameterError::ZeroMaxTurns`].
    pub fn new(n: u32) -> Result<Self, ParameterError> {
        NonZeroU32::new(n)
            .map(Self)
            .ok_or(ParameterError::ZeroMaxTurns)
    }

    /// The limit.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl Default for MaxTurns {
    fn default() -> Self {
        Self(NonZeroU32::new(16).expect("16 is non-zero"))
    }
}

/// A finished run.
#[derive(Debug)]
pub struct RunOutcome {
    final_turn: FinalTurn,
    turns: u32,
    usage: Usage,
    calls: CallCounts,
}

impl RunOutcome {
    /// The last turn.
    #[must_use]
    pub fn final_turn(&self) -> &FinalTurn {
        &self.final_turn
    }

    /// How many responses the run took.
    #[must_use]
    pub fn turns(&self) -> u32 {
        self.turns
    }

    /// Token usage summed over every response.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// What happened to the calls the executor answered while this run was
    /// in progress. Exact for an executor used by one run at a time; runs
    /// sharing an executor concurrently count each other's calls too.
    #[must_use]
    pub fn calls(&self) -> CallCounts {
        self.calls
    }
}

/// Drive `conversation` from `prompt` to a final turn, answering every
/// call with `executor` and resuming every pause.
///
/// # Errors
///
/// [`PtcError::TurnLimit`] if no final turn arrives within `limit`
/// responses; otherwise whatever the exchange returned.
pub async fn run<D: Dispatcher>(
    client: &PtcClient,
    executor: &Executor<D>,
    conversation: Conversation,
    prompt: UserPrompt,
    limit: MaxTurns,
) -> Result<RunOutcome, PtcError> {
    let before = executor.counts();
    let mut turn = client.start(conversation, prompt).await?;
    let mut turns = 1;
    let mut usage = turn.usage();
    loop {
        turn = match turn {
            Turn::Final(final_turn) => {
                return Ok(RunOutcome {
                    final_turn,
                    turns,
                    usage,
                    calls: executor.counts() - before,
                });
            }
            _ if turns >= limit.get() => return Err(PtcError::TurnLimit { limit: limit.get() }),
            Turn::Programmatic(p) => p.respond_with(executor).await.send(client).await?,
            Turn::Direct(p) => p.respond_with(executor).await.send(client).await?,
            Turn::Paused(p) => p.resume(client).await?,
        };
        turns += 1;
        usage = usage + turn.usage();
    }
}
