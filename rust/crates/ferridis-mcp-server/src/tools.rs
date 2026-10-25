//! Projection of the shared [`Catalogue`] into MCP `tools/list` descriptors.
//!
//! Tool naming, descriptions (including the response schema) and the
//! input-schema precedence all live in `ferridis_client::catalogue`, so the
//! MCP publisher and `ferridis-ptc` give every intent the same tool
//! identity (decision PTC-3 in `architecture.md`). This module only
//! reshapes catalogue entries into MCP's `Tool` wire type.

use ferridis_client::catalogue::{Catalogue, CatalogueEntry};

use crate::protocol::Tool;

/// The catalogue plus its MCP-shaped descriptors, built once at startup.
#[derive(Debug, Default)]
pub struct ToolCatalogue {
    catalogue: Catalogue,
    tools: Vec<Tool>,
}

impl ToolCatalogue {
    /// Project `catalogue` into MCP tool descriptors.
    pub fn new(catalogue: Catalogue) -> Self {
        let tools = catalogue.entries().iter().map(mcp_tool).collect();
        Self { catalogue, tools }
    }

    /// Resolve an MCP tool name to its catalogue entry.
    pub fn resolve(&self, name: &str) -> Option<&CatalogueEntry> {
        self.catalogue.resolve(name)
    }

    /// All tool descriptors for `tools/list`.
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }
}

fn mcp_tool(entry: &CatalogueEntry) -> Tool {
    Tool {
        name: entry.name().as_str().to_owned(),
        description: entry.description().as_str().to_owned(),
        input_schema: entry.input_schema().clone(), // allow:clone — the MCP descriptor is serialized independently of the catalogue that keeps its own copy for dispatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferridis_client::catalogue::CatalogueSource;
    use ferridis_core::{CapabilityRef, Manifest};

    /// The MCP projection is 1:1 with the catalogue: same names, same
    /// descriptions, same schemas, same order.
    #[test]
    fn mcp_tools_mirror_the_catalogue() {
        let manifest = Manifest::parse(
            r#"{
            "ferridis_version": "0.1",
            "id": "ferridis.fs.v1",
            "name": "FS",
            "category": "files",
            "summary": "Filesystem.",
            "intents": ["read-file", "write-file"],
            "schema": { "type": "openapi-3", "url": "https://x/s.json" },
            "tiers": ["native"],
            "auth": { "type": "none" }
        }"#,
        )
        .expect("test manifest parses");
        let cap = CapabilityRef::parse("ferridis://public.ferridis.io/ferridis/fs@v1")
            .expect("test ref parses");
        let cat = ToolCatalogue::new(Catalogue::from_sources(&[CatalogueSource::new(
            cap, manifest,
        )]));
        assert_eq!(cat.tools().len(), 2);
        for tool in cat.tools() {
            let entry = cat.resolve(&tool.name).expect("every listed tool resolves");
            assert_eq!(tool.description, entry.description().as_str());
            assert_eq!(&tool.input_schema, entry.input_schema());
        }
    }
}
