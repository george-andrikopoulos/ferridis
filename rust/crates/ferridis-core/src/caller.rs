//! Who may call an intent when it is exposed as a model tool.
//!
//! Decision PTC-1 in `architecture.md`. A tool is called either directly
//! by the model or from code the model runs in a code-execution container
//! (programmatic tool calling), never both, following Anthropic's guidance
//! to pick one per tool.

use serde::{Deserialize, Serialize};

/// Which surface may call an intent exposed as a model tool.
///
/// A publisher declares it per intent in the manifest (`"callers"` on a
/// structured intent entry, protocol v0.7, optional). An undeclared intent
/// is `None` at the manifest layer; the gateway resolves it to a variant
/// before dispatch, defaulting to [`CallerPolicy::Direct`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallerPolicy {
    /// The model calls the tool itself, one call per model turn.
    Direct,
    /// Only code running in the model's code-execution container calls
    /// the tool, so results go to that code rather than into the model's
    /// context.
    Code,
}

impl std::fmt::Display for CallerPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Direct => "direct",
            Self::Code => "code",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_the_lowercase_manifest_spelling() {
        assert_eq!(
            serde_json::to_string(&CallerPolicy::Code).expect("serializes"),
            "\"code\""
        );
        assert_eq!(
            serde_json::from_str::<CallerPolicy>("\"direct\"").expect("parses"),
            CallerPolicy::Direct
        );
    }

    #[test]
    fn rejects_both_and_anything_else() {
        for raw in ["\"both\"", "\"Code\"", "\"\"", "true"] {
            assert!(serde_json::from_str::<CallerPolicy>(raw).is_err(), "{raw}");
        }
    }
}
