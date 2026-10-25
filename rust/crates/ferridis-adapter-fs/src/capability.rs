//! [`FilesystemCapability`] — the adapter's implementation of
//! [`ferridis_adapter_sdk::Capability`].

use async_trait::async_trait;
use ferridis_adapter_sdk::{Capability, DispatchError, SchemaSource};
use ferridis_core::{IntentVerb, Manifest};
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::path::{PathError, Root};

/// The embedded manifest the adapter publishes.
///
/// Adapter operators can override the manifest by calling
/// [`FilesystemCapability::with_manifest`] but the default value is
/// sufficient for the canonical demo.
const DEFAULT_MANIFEST_JSON: &str = r#"{
    "ferridis_version": "0.1",
    "id": "ferridis.fs.v1",
    "name": "Ferridis Filesystem",
    "category": "files",
    "summary": "Read, write, list, search, and move files under a sandboxed root directory.",
    "intents": ["read-file", "write-file", "list-dir", "search-files", "move-file"],
    "schema": {
        "type": "openapi-3",
        "url": "https://ferridis.io/schemas/fs.v1.yaml"
    },
    "tiers": ["native"],
    "auth": { "type": "none" }
}"#;

/// The embedded OpenAPI 3 document for the filesystem adapter, served at
/// `GET /schema`.
///
/// JSON rather than YAML so `ferridis-protocol::ResponseSchemas` can read
/// it without a YAML dependency (decision PTC-2). Every response schema
/// is pinned against the real handlers by the conformance assertions in
/// `tests/end_to_end.rs`.
const EMBEDDED_SCHEMA: &str = r#"{
  "openapi": "3.0.3",
  "info": { "title": "Ferridis Filesystem", "version": "0.1.0" },
  "paths": {
    "/intents/read-file": {
      "post": {
        "summary": "Read a UTF-8 file under the configured root.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["path"],
          "properties": { "path": { "type": "string", "description": "Relative path under the adapter's root." } }
        } } } },
        "responses": { "200": { "description": "The file's contents.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["path", "content", "bytes"],
          "properties": {
            "path": { "type": "string", "description": "The path as requested." },
            "content": { "type": "string", "description": "The file's full UTF-8 text." },
            "bytes": { "type": "integer", "minimum": 0, "description": "Length of `content` in bytes." }
          }
        } } } } }
      }
    },
    "/intents/write-file": {
      "post": {
        "summary": "Write a file under the configured root, creating parent directories.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["path", "content"],
          "properties": { "path": { "type": "string" }, "content": { "type": "string" } }
        } } } },
        "responses": { "200": { "description": "Written.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["ok"], "properties": { "ok": { "type": "boolean", "const": true } }
        } } } } }
      }
    },
    "/intents/list-dir": {
      "post": {
        "summary": "List a directory under the configured root.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["path"],
          "properties": { "path": { "type": "string", "description": "Empty string for the root." } }
        } } } },
        "responses": { "200": { "description": "One entry per directory member.", "content": { "application/json": { "schema": {
          "type": "array",
          "items": {
            "type": "object", "required": ["name", "kind"],
            "properties": {
              "name": { "type": "string" },
              "kind": { "type": "string", "enum": ["file", "dir"] }
            }
          }
        } } } } }
      }
    },
    "/intents/search-files": {
      "post": {
        "summary": "Recursively find files whose name contains a substring.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["path", "name_contains"],
          "properties": { "path": { "type": "string" }, "name_contains": { "type": "string" } }
        } } } },
        "responses": { "200": { "description": "Matching paths, relative to the root.", "content": { "application/json": { "schema": {
          "type": "array", "items": { "type": "string" }
        } } } } }
      }
    },
    "/intents/move-file": {
      "post": {
        "summary": "Move or rename a file under the configured root.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["from", "to"],
          "properties": { "from": { "type": "string" }, "to": { "type": "string" } }
        } } } },
        "responses": { "200": { "description": "Moved.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["ok"], "properties": { "ok": { "type": "boolean", "const": true } }
        } } } } }
      }
    }
  }
}"#;

/// The filesystem capability.
///
/// Construct via [`FilesystemCapability::new`] with the root path that
/// every intent operates under.
pub struct FilesystemCapability {
    manifest: Manifest,
    root: Root,
}

impl FilesystemCapability {
    /// Build a capability rooted at `root`.
    ///
    /// `root` must be an existing absolute directory; the validation
    /// happens inside [`Root::new`].
    pub fn new(root: Root) -> Result<Self, DispatchError> {
        let manifest = Manifest::parse(DEFAULT_MANIFEST_JSON).map_err(|e| {
            DispatchError::Internal(format!("default manifest failed to parse: {e}"))
        })?;
        Ok(Self { manifest, root })
    }

    /// Override the embedded manifest.
    pub fn with_manifest(mut self, manifest: Manifest) -> Self {
        self.manifest = manifest;
        self
    }
}

#[async_trait]
impl Capability for FilesystemCapability {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn schema(&self) -> SchemaSource {
        SchemaSource::Embedded {
            content_type: "application/json".into(),
            body: EMBEDDED_SCHEMA.into(),
        }
    }

    async fn dispatch(
        &self,
        intent: &IntentVerb,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, DispatchError> {
        match intent.as_str() {
            "read-file" => read_file(&self.root, body).await,
            "write-file" => write_file(&self.root, body).await,
            "list-dir" => list_dir(&self.root, body).await,
            "search-files" => search_files(&self.root, body).await,
            "move-file" => move_file(&self.root, body).await,
            _ => Err(DispatchError::UnsupportedIntent(intent.clone())),
        }
    }
}

#[derive(Debug, Deserialize)]
struct PathArg {
    path: String,
}

#[derive(Debug, Deserialize)]
struct WriteArg {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct MoveArg {
    from: String,
    to: String,
}

#[derive(Debug, Deserialize)]
struct SearchArg {
    path: String,
    name_contains: String,
}

#[derive(Debug, Serialize)]
struct FileContent {
    path: String,
    content: String,
    bytes: usize,
}

#[derive(Debug, Serialize)]
struct ListEntry {
    name: String,
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct OkResponse {
    ok: bool,
}

fn map_path_error(e: PathError) -> DispatchError {
    DispatchError::InvalidRequest(e.to_string())
}

fn map_io_not_found(e: std::io::Error, label: &str) -> DispatchError {
    match e.kind() {
        std::io::ErrorKind::NotFound => DispatchError::NotFound(label.into()),
        std::io::ErrorKind::PermissionDenied => DispatchError::Forbidden(label.into()),
        _ => DispatchError::Internal(format!("{label}: {e}")),
    }
}

async fn read_file(
    root: &Root,
    body: serde_json::Value,
) -> Result<serde_json::Value, DispatchError> {
    let arg: PathArg = serde_json::from_value(body)
        .map_err(|e| DispatchError::InvalidRequest(format!("expected {{path}}: {e}")))?;
    let rel = root.resolve(&arg.path).map_err(map_path_error)?;
    let abs = root.join(&rel);
    let bytes = fs::read(&abs)
        .await
        .map_err(|e| map_io_not_found(e, &arg.path))?;
    let content = String::from_utf8(bytes.clone())
        .map_err(|e| DispatchError::InvalidRequest(format!("file is not UTF-8: {e}")))?;
    Ok(serde_json::to_value(FileContent {
        path: arg.path,
        bytes: content.len(),
        content,
    })?)
}

async fn write_file(
    root: &Root,
    body: serde_json::Value,
) -> Result<serde_json::Value, DispatchError> {
    let arg: WriteArg = serde_json::from_value(body)
        .map_err(|e| DispatchError::InvalidRequest(format!("expected {{path, content}}: {e}")))?;
    let rel = root.resolve(&arg.path).map_err(map_path_error)?;
    let abs = root.join(&rel);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|e| DispatchError::Internal(format!("create parent: {e}")))?;
    }
    fs::write(&abs, arg.content.as_bytes())
        .await
        .map_err(|e| map_io_not_found(e, &arg.path))?;
    Ok(serde_json::to_value(OkResponse { ok: true })?)
}

async fn list_dir(
    root: &Root,
    body: serde_json::Value,
) -> Result<serde_json::Value, DispatchError> {
    let arg: PathArg = serde_json::from_value(body)
        .map_err(|e| DispatchError::InvalidRequest(format!("expected {{path}}: {e}")))?;
    let rel = root.resolve(&arg.path).map_err(map_path_error)?;
    let abs = root.join(&rel);
    let mut entries = Vec::new();
    let mut reader = fs::read_dir(&abs)
        .await
        .map_err(|e| map_io_not_found(e, &arg.path))?;
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|e| DispatchError::Internal(format!("read_dir: {e}")))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            "dir"
        } else {
            "file"
        };
        entries.push(ListEntry { name, kind });
    }
    Ok(serde_json::to_value(entries)?)
}

async fn search_files(
    root: &Root,
    body: serde_json::Value,
) -> Result<serde_json::Value, DispatchError> {
    let arg: SearchArg = serde_json::from_value(body).map_err(|e| {
        DispatchError::InvalidRequest(format!("expected {{path, name_contains}}: {e}"))
    })?;
    let rel = root.resolve(&arg.path).map_err(map_path_error)?;
    let base = root.join(&rel);

    let needle = arg.name_contains.clone();
    let base_clone = base.clone();
    let root_clone = root.as_path().to_path_buf();
    let hits = tokio::task::spawn_blocking(move || -> Result<Vec<String>, std::io::Error> {
        let mut out = Vec::new();
        walk(&base_clone, &root_clone, &needle, &mut out)?;
        Ok(out)
    })
    .await
    .map_err(|e| DispatchError::Internal(format!("join error: {e}")))?
    .map_err(|e| map_io_not_found(e, &arg.path))?;

    Ok(serde_json::to_value(hits)?)
}

fn walk(
    dir: &std::path::Path,
    root: &std::path::Path,
    needle: &str,
    out: &mut Vec<String>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(&path, root, needle, out)?;
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.contains(needle)
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(rel.display().to_string());
        }
    }
    Ok(())
}

async fn move_file(
    root: &Root,
    body: serde_json::Value,
) -> Result<serde_json::Value, DispatchError> {
    let arg: MoveArg = serde_json::from_value(body)
        .map_err(|e| DispatchError::InvalidRequest(format!("expected {{from, to}}: {e}")))?;
    let from_rel = root.resolve(&arg.from).map_err(map_path_error)?;
    let to_rel = root.resolve(&arg.to).map_err(map_path_error)?;
    let from = root.join(&from_rel);
    let to = root.join(&to_rel);
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|e| DispatchError::Internal(format!("create parent: {e}")))?;
    }
    fs::rename(&from, &to)
        .await
        .map_err(|e| map_io_not_found(e, &arg.from))?;
    Ok(serde_json::to_value(OkResponse { ok: true })?)
}
