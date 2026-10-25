//! Projecting the Ferridis tool catalogue into Messages API tools.

use std::collections::BTreeSet;
use std::sync::Arc;

use ferridis_client::adapters::CallerOverrides;
use ferridis_client::catalogue::{Catalogue, CatalogueEntry, ToolName};
use ferridis_core::{CallerPolicy, CapabilityRef, IntentKind, IntentVerb};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::policy::{PolicySource, ResolvedPolicy};

/// The code-execution tool version whose container runs programmatic
/// calls. Also the `allowed_callers` value that makes a tool callable
/// from that container.
pub const CODE_EXECUTION_TOOL_TYPE: &str = "code_execution_20260120";

/// One catalogue tool with its effective caller policy.
#[derive(Debug, Clone)]
pub struct PlannedTool {
    name: ToolName,
    capability: CapabilityRef,
    intent: IntentVerb,
    kind: IntentKind,
    policy: ResolvedPolicy,
}

impl PlannedTool {
    fn from_entry(entry: &CatalogueEntry, overrides: &CallerOverrides) -> Self {
        let policy = ResolvedPolicy::resolve(
            entry.declared_callers(),
            overrides.get(entry.name()),
            entry.input_schema(),
        );
        Self {
            name: entry.name().clone(), // allow:clone — the plan outlives the borrowed catalogue
            capability: entry.capability().clone(), // allow:clone — same
            intent: entry.intent().clone(), // allow:clone — same
            kind: entry.kind(),
            policy,
        }
    }

    /// The tool's name.
    #[must_use]
    pub fn name(&self) -> &ToolName {
        &self.name
    }

    /// The capability it dispatches to.
    #[must_use]
    pub fn capability(&self) -> &CapabilityRef {
        &self.capability
    }

    /// The intent it dispatches.
    #[must_use]
    pub fn intent(&self) -> &IntentVerb {
        &self.intent
    }

    /// Request or stream.
    #[must_use]
    pub fn kind(&self) -> IntentKind {
        self.kind
    }

    /// Who may call it, and why.
    #[must_use]
    pub fn policy(&self) -> &ResolvedPolicy {
        &self.policy
    }
}

/// Every catalogue tool with its policy, plus the tools array sent on
/// every turn of a conversation built from it.
#[derive(Debug)]
pub struct ToolPlan {
    tools: Vec<PlannedTool>,
    tool_set: Arc<ToolSet>,
}

impl ToolPlan {
    /// Resolve every tool's policy and build the tools array.
    ///
    /// Logs a WARN naming each tool downgraded to direct-only, each
    /// override key that matches no tool (a typo overrides nothing), and
    /// each tool shadowed by a later tool of the same name.
    #[must_use]
    pub fn build(catalogue: &Catalogue, overrides: &CallerOverrides) -> Self {
        let mut tools = Vec::new();
        let mut definitions = Vec::new();
        for entry in catalogue.entries() {
            let winner = catalogue
                .resolve(entry.name().as_str())
                .is_some_and(|w| std::ptr::eq(w, entry));
            if !winner {
                tracing::warn!(tool = %entry.name(), capability = %entry.capability(),
                    "tool shadowed by a later tool of the same name; not offered");
                continue;
            }
            let planned = PlannedTool::from_entry(entry, overrides);
            if let PolicySource::Downgraded(hazard) = planned.policy.source() {
                tracing::warn!(tool = %planned.name, %hazard,
                    "tool downgraded to direct-only: its input schema cannot be used for programmatic calls");
            }
            definitions.push(ToolDefinition {
                name: entry.name().clone(), // allow:clone — the tools array is serialized every turn, long after the catalogue borrow
                description: entry.description().as_str().to_owned(),
                input_schema: entry.input_schema().clone(), // allow:clone — same
                callers: planned.policy.policy(),
            });
            tools.push(planned);
        }
        let known: BTreeSet<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        for name in overrides
            .tool_names()
            .filter(|n| !known.contains(n.as_str()))
        {
            tracing::warn!(tool = %name, "caller override names no registered tool; ignored");
        }
        let code_execution = if definitions.iter().any(|d| d.callers == CallerPolicy::Code) {
            CodeExecution::Offered
        } else {
            CodeExecution::NotOffered
        };
        Self {
            tools,
            tool_set: Arc::new(ToolSet {
                code_execution,
                definitions,
            }),
        }
    }

    /// Every offered tool, in catalogue order.
    #[must_use]
    pub fn tools(&self) -> &[PlannedTool] {
        &self.tools
    }

    /// The tools array, shared by every turn of a conversation.
    #[must_use]
    pub fn tool_set(&self) -> Arc<ToolSet> {
        Arc::clone(&self.tool_set)
    }

    pub(crate) fn into_tools(self) -> Vec<PlannedTool> {
        self.tools
    }
}

/// Whether the code-execution tool is offered. Only offered when some
/// tool is code-callable, so a direct-only plan never starts a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeExecution {
    Offered,
    NotOffered,
}

/// The `tools` array of a Messages API request.
///
/// A [`ToolDefinition`] has no `strict` field, so the flag the API
/// rejects alongside programmatic calling cannot be sent.
#[derive(Debug, PartialEq)]
pub struct ToolSet {
    code_execution: CodeExecution,
    definitions: Vec<ToolDefinition>,
}

impl ToolSet {
    /// How many custom tools the array carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.definitions.len()
    }

    /// Whether it carries no custom tool.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }

    /// Whether the code-execution tool is offered.
    #[must_use]
    pub fn offers_code_execution(&self) -> bool {
        self.code_execution == CodeExecution::Offered
    }
}

impl Serialize for ToolSet {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(None)?;
        if self.code_execution == CodeExecution::Offered {
            seq.serialize_element(&CodeExecutionTool)?;
        }
        for d in &self.definitions {
            seq.serialize_element(d)?;
        }
        seq.end()
    }
}

struct CodeExecutionTool;

impl Serialize for CodeExecutionTool {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(2))?;
        map.serialize_entry("type", CODE_EXECUTION_TOOL_TYPE)?;
        map.serialize_entry("name", "code_execution")?;
        map.end()
    }
}

/// One custom tool.
#[derive(Debug, PartialEq)]
struct ToolDefinition {
    name: ToolName,
    description: String,
    input_schema: Value,
    callers: CallerPolicy,
}

impl Serialize for ToolDefinition {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let allowed: [&str; 1] = match self.callers {
            CallerPolicy::Direct => ["direct"],
            CallerPolicy::Code => [CODE_EXECUTION_TOOL_TYPE],
        };
        let mut map = s.serialize_map(Some(4))?;
        map.serialize_entry("name", self.name.as_str())?;
        map.serialize_entry("description", &self.description)?;
        map.serialize_entry("input_schema", &self.input_schema)?;
        map.serialize_entry("allowed_callers", &allowed)?;
        map.end()
    }
}
