//! Identifiers the Messages API hands out, each its own type so a
//! `tool_result` can only ever name a `tool_use` id.

use serde::{Serialize, Serializer};

use crate::error::ResponseError;

macro_rules! wire_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// Accept an id from a response; `field` names it in the error.
            pub(crate) fn parse(field: &'static str, raw: String) -> Result<Self, ResponseError> {
                if raw.is_empty() {
                    return Err(ResponseError::EmptyId { field });
                }
                Ok(Self(raw))
            }

            /// Borrow the id.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }
    };
}

wire_id!(
    /// A `tool_use` block's id: what a `tool_result` answers.
    ToolUseId
);

wire_id!(
    /// A `server_tool_use` block's id: the code-execution run that made a
    /// programmatic call.
    ServerToolUseId
);

wire_id!(
    /// The code-execution container a programmatic call runs in. The
    /// continuation must name it, or the waiting code is lost.
    ContainerId
);
