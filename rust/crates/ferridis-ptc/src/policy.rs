//! Who may call each tool: the effective caller policy.
//!
//! Precedence, strongest first (decision PTC-1):
//!
//! 1. **Computed.** A tool whose input schema the API cannot accept for
//!    programmatic calls (a recursive or unresolvable `$ref`) is
//!    direct-only, whatever was asked for.
//! 2. **Operator override** from the adapters file's `callers` map.
//! 3. **Publisher declaration** in the manifest.
//! 4. **Default:** direct.

use std::collections::HashMap;

use ferridis_core::CallerPolicy;
use serde_json::Value;

/// Why a tool's input schema cannot be used for programmatic calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaHazard {
    /// A `$ref` reaches itself; the API rejects it with
    /// "Circular $ref detected".
    RecursiveRef {
        /// The reference on the cycle.
        reference: String,
    },
    /// A `$ref` points outside the schema or at nothing, so recursion
    /// cannot be ruled out.
    UnresolvableRef {
        /// The reference.
        reference: String,
    },
}

impl std::fmt::Display for SchemaHazard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RecursiveRef { reference } => write!(f, "recursive $ref {reference}"),
            Self::UnresolvableRef { reference } => write!(f, "unresolvable $ref {reference}"),
        }
    }
}

/// Where a tool's effective policy came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicySource {
    /// Nothing was declared or overridden.
    Default,
    /// The publisher's manifest declaration.
    Manifest,
    /// The operator's adapters-file override.
    Override,
    /// Code was asked for, but the schema forces direct.
    Downgraded(SchemaHazard),
}

/// A tool's effective caller policy and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPolicy {
    policy: CallerPolicy,
    source: PolicySource,
}

impl ResolvedPolicy {
    /// Resolve by the precedence in the module docs.
    #[must_use]
    pub fn resolve(
        declared: Option<CallerPolicy>,
        overridden: Option<CallerPolicy>,
        input_schema: &Value,
    ) -> Self {
        let (wanted, source) = match (overridden, declared) {
            (Some(p), _) => (p, PolicySource::Override),
            (None, Some(p)) => (p, PolicySource::Manifest),
            (None, None) => (CallerPolicy::Direct, PolicySource::Default),
        };
        if wanted == CallerPolicy::Code
            && let Some(hazard) = schema_hazard(input_schema)
        {
            return Self {
                policy: CallerPolicy::Direct,
                source: PolicySource::Downgraded(hazard),
            };
        }
        Self {
            policy: wanted,
            source,
        }
    }

    /// The effective policy.
    #[must_use]
    pub fn policy(&self) -> CallerPolicy {
        self.policy
    }

    /// Where it came from.
    #[must_use]
    pub fn source(&self) -> &PolicySource {
        &self.source
    }
}

#[derive(Clone, Copy)]
enum Visit {
    InProgress,
    Done,
}

/// The first reason `root` cannot be sent as a programmatic tool's
/// input schema, if any.
fn schema_hazard(root: &Value) -> Option<SchemaHazard> {
    let mut visits = HashMap::new();
    refs_in(root)
        .into_iter()
        .find_map(|r| visit(root, r, &mut visits))
}

/// Depth-first walk of the `$ref` graph from `reference`: reaching a
/// reference still in progress is a cycle.
fn visit<'s>(
    root: &'s Value,
    reference: &'s str,
    visits: &mut HashMap<&'s str, Visit>,
) -> Option<SchemaHazard> {
    match visits.get(reference) {
        Some(Visit::InProgress) => {
            return Some(SchemaHazard::RecursiveRef {
                reference: reference.to_owned(),
            });
        }
        Some(Visit::Done) => return None,
        None => {}
    }
    let Some(target) = resolve_local(root, reference) else {
        return Some(SchemaHazard::UnresolvableRef {
            reference: reference.to_owned(),
        });
    };
    visits.insert(reference, Visit::InProgress);
    let hazard = refs_in(target)
        .into_iter()
        .find_map(|r| visit(root, r, visits));
    visits.insert(reference, Visit::Done);
    hazard
}

/// A same-document reference (`#` or `#/json/pointer`), resolved.
fn resolve_local<'s>(root: &'s Value, reference: &str) -> Option<&'s Value> {
    let fragment = reference.strip_prefix('#')?;
    if fragment.is_empty() {
        return Some(root);
    }
    fragment
        .starts_with('/')
        .then(|| root.pointer(fragment))
        .flatten()
}

/// Every `$ref` string anywhere under `value`.
fn refs_in(value: &Value) -> Vec<&str> {
    let mut out = Vec::new();
    collect_refs(value, &mut out);
    out
}

fn collect_refs<'s>(value: &'s Value, out: &mut Vec<&'s str>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                match (key.as_str(), child) {
                    ("$ref", Value::String(r)) => out.push(r),
                    _ => collect_refs(child, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|i| collect_refs(i, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn recursive_schema() -> Value {
        json!({
            "type": "object",
            "properties": { "root": { "$ref": "#/$defs/node" } },
            "$defs": { "node": {
                "type": "object",
                "properties": { "child": { "$ref": "#/$defs/node" } }
            } }
        })
    }

    #[test]
    fn precedence_is_override_then_manifest_then_direct() {
        let flat = json!({ "type": "object" });
        let r =
            ResolvedPolicy::resolve(Some(CallerPolicy::Code), Some(CallerPolicy::Direct), &flat);
        assert_eq!(
            (r.policy(), r.source()),
            (CallerPolicy::Direct, &PolicySource::Override)
        );
        let r = ResolvedPolicy::resolve(Some(CallerPolicy::Code), None, &flat);
        assert_eq!(
            (r.policy(), r.source()),
            (CallerPolicy::Code, &PolicySource::Manifest)
        );
        let r = ResolvedPolicy::resolve(None, None, &flat);
        assert_eq!(
            (r.policy(), r.source()),
            (CallerPolicy::Direct, &PolicySource::Default)
        );
    }

    /// The API rejects a recursive `$ref` on a programmatic tool, so the
    /// computed policy beats even an operator override.
    #[test]
    fn a_recursive_ref_downgrades_a_code_tool_to_direct() {
        let r = ResolvedPolicy::resolve(None, Some(CallerPolicy::Code), &recursive_schema());
        assert_eq!(r.policy(), CallerPolicy::Direct);
        assert_eq!(
            r.source(),
            &PolicySource::Downgraded(SchemaHazard::RecursiveRef {
                reference: "#/$defs/node".to_owned()
            })
        );
    }

    #[test]
    fn a_recursive_ref_leaves_a_direct_tool_alone() {
        let r = ResolvedPolicy::resolve(None, None, &recursive_schema());
        assert_eq!(
            (r.policy(), r.source()),
            (CallerPolicy::Direct, &PolicySource::Default)
        );
    }

    #[test]
    fn detects_self_reference_through_the_root_and_mutual_recursion() {
        let to_root = json!({ "properties": { "next": { "$ref": "#" } } });
        assert!(matches!(
            schema_hazard(&to_root),
            Some(SchemaHazard::RecursiveRef { .. })
        ));
        let mutual = json!({
            "properties": { "a": { "$ref": "#/$defs/a" } },
            "$defs": {
                "a": { "properties": { "b": { "$ref": "#/$defs/b" } } },
                "b": { "properties": { "a": { "$ref": "#/$defs/a" } } }
            }
        });
        assert!(matches!(
            schema_hazard(&mutual),
            Some(SchemaHazard::RecursiveRef { .. })
        ));
    }

    #[test]
    fn shared_non_recursive_refs_are_fine() {
        let shared = json!({
            "properties": {
                "from": { "$ref": "#/$defs/point" },
                "to": { "$ref": "#/$defs/point" }
            },
            "$defs": { "point": { "type": "object", "properties": { "x": { "type": "number" } } } }
        });
        assert_eq!(schema_hazard(&shared), None);
    }

    #[test]
    fn external_and_dangling_refs_are_unresolvable() {
        for r in ["https://example.com/s.json", "#/$defs/missing", "#anchor"] {
            let schema = json!({ "properties": { "x": { "$ref": r } } });
            assert_eq!(
                schema_hazard(&schema),
                Some(SchemaHazard::UnresolvableRef {
                    reference: r.to_owned()
                }),
                "{r}"
            );
        }
    }
}
