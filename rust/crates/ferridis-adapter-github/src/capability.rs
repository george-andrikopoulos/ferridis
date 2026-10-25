//! [`GitHubCapability`] — the adapter's implementation of
//! [`ferridis_adapter_sdk::Capability`].

use async_trait::async_trait;
use ferridis_adapter_sdk::{Capability, DispatchError, SchemaSource};
use ferridis_core::{IntentVerb, Manifest};
use serde::Deserialize;

use crate::client::GitHubClient;
use crate::types::{GitHubError, GitHubToken};

// ---------------------------------------------------------------------------
// Manifest + schema
// ---------------------------------------------------------------------------

const DEFAULT_MANIFEST_JSON: &str = r#"{
    "ferridis_version": "0.1",
    "id": "ferridis.github.v1",
    "name": "Ferridis GitHub",
    "category": "developer-tools",
    "summary": "Browse repositories, read files, manage issues and pull requests, and search code on GitHub.",
    "intents": [
        "list-repos",
        "get-file",
        "list-issues",
        "create-issue",
        "list-pull-requests",
        "search-code"
    ],
    "schema": {
        "type": "openapi-3",
        "url": "https://ferridis.io/schemas/github.v1.yaml"
    },
    "tiers": ["native"],
    "auth": { "type": "none" }
}"#;

/// The embedded OpenAPI 3 document, served at `GET /schema`.
///
/// JSON rather than YAML so `ferridis-protocol::ResponseSchemas` can read
/// it without a YAML dependency (decision PTC-2). Most intents pass the
/// GitHub REST response through, so their response schemas document the
/// stable fields and require only what the adapter can rely on. Each is
/// pinned by a conformance assertion in `tests/github_adapter.rs`.
const EMBEDDED_SCHEMA: &str = r#"{
  "openapi": "3.0.3",
  "info": { "title": "Ferridis GitHub", "version": "0.1.0" },
  "paths": {
    "/intents/list-repos": {
      "post": {
        "summary": "List repositories for a user or organisation.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["owner"],
          "properties": {
            "owner": { "type": "string" },
            "type": { "type": "string", "enum": ["all", "public", "private", "forks", "sources", "member"] },
            "per_page": { "type": "integer", "minimum": 1, "maximum": 100 }
          }
        } } } },
        "responses": { "200": { "description": "GitHub repository objects, passed through.", "content": { "application/json": { "schema": {
          "type": "array",
          "items": {
            "type": "object", "required": ["name"],
            "properties": {
              "id": { "type": "integer" },
              "name": { "type": "string" },
              "full_name": { "type": "string", "description": "owner/name" },
              "private": { "type": "boolean" },
              "fork": { "type": "boolean" },
              "description": { "type": ["string", "null"] },
              "html_url": { "type": "string" },
              "default_branch": { "type": "string" },
              "updated_at": { "type": "string", "description": "RFC 3339 timestamp." }
            }
          }
        } } } } }
      }
    },
    "/intents/get-file": {
      "post": {
        "summary": "Read a UTF-8 file from a repository.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["owner", "repo", "path"],
          "properties": {
            "owner": { "type": "string" },
            "repo": { "type": "string" },
            "path": { "type": "string" },
            "ref": { "type": "string", "description": "Branch, tag or commit; defaults to the default branch." }
          }
        } } } },
        "responses": { "200": { "description": "The decoded file.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["path", "sha", "size", "content"],
          "properties": {
            "path": { "type": "string" },
            "sha": { "type": "string" },
            "size": { "type": "integer", "minimum": 0 },
            "content": { "type": "string", "description": "Decoded UTF-8 text." }
          }
        } } } } }
      }
    },
    "/intents/list-issues": {
      "post": {
        "summary": "List issues for a repository (GitHub includes pull requests here).",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["owner", "repo"],
          "properties": {
            "owner": { "type": "string" },
            "repo": { "type": "string" },
            "state": { "type": "string", "enum": ["open", "closed", "all"] }
          }
        } } } },
        "responses": { "200": { "description": "GitHub issue objects, passed through (up to 30).", "content": { "application/json": { "schema": {
          "type": "array",
          "items": {
            "type": "object", "required": ["number", "title"],
            "properties": {
              "number": { "type": "integer" },
              "title": { "type": "string" },
              "state": { "type": "string", "enum": ["open", "closed"] },
              "html_url": { "type": "string" },
              "body": { "type": ["string", "null"] },
              "user": { "type": "object", "properties": { "login": { "type": "string" } } },
              "labels": { "type": "array", "items": { "type": "object", "properties": { "name": { "type": "string" } } } },
              "pull_request": { "type": "object", "description": "Present only when the item is a pull request." }
            }
          }
        } } } } }
      }
    },
    "/intents/create-issue": {
      "post": {
        "summary": "Open a new issue in a repository.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["owner", "repo", "title"],
          "properties": {
            "owner": { "type": "string" },
            "repo": { "type": "string" },
            "title": { "type": "string" },
            "body": { "type": "string" },
            "labels": { "type": "array", "items": { "type": "string" } }
          }
        } } } },
        "responses": { "200": { "description": "The created GitHub issue object, passed through.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["number", "title"],
          "properties": {
            "number": { "type": "integer" },
            "title": { "type": "string" },
            "state": { "type": "string" },
            "html_url": { "type": "string" }
          }
        } } } } }
      }
    },
    "/intents/list-pull-requests": {
      "post": {
        "summary": "List pull requests for a repository.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["owner", "repo"],
          "properties": {
            "owner": { "type": "string" },
            "repo": { "type": "string" },
            "state": { "type": "string", "enum": ["open", "closed", "all"] }
          }
        } } } },
        "responses": { "200": { "description": "GitHub pull-request objects, passed through (up to 30).", "content": { "application/json": { "schema": {
          "type": "array",
          "items": {
            "type": "object", "required": ["number", "title"],
            "properties": {
              "number": { "type": "integer" },
              "title": { "type": "string" },
              "state": { "type": "string", "enum": ["open", "closed"] },
              "draft": { "type": "boolean" },
              "html_url": { "type": "string" },
              "user": { "type": "object", "properties": { "login": { "type": "string" } } },
              "head": { "type": "object", "properties": { "ref": { "type": "string" } } },
              "base": { "type": "object", "properties": { "ref": { "type": "string" } } }
            }
          }
        } } } } }
      }
    },
    "/intents/search-code": {
      "post": {
        "summary": "Search code across GitHub.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["query"],
          "properties": {
            "query": { "type": "string", "description": "GitHub code-search syntax." },
            "per_page": { "type": "integer", "minimum": 1, "maximum": 100 }
          }
        } } } },
        "responses": { "200": { "description": "GitHub code-search result, passed through.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["total_count", "items"],
          "properties": {
            "total_count": { "type": "integer" },
            "incomplete_results": { "type": "boolean" },
            "items": {
              "type": "array",
              "items": {
                "type": "object", "required": ["name", "path"],
                "properties": {
                  "name": { "type": "string" },
                  "path": { "type": "string" },
                  "sha": { "type": "string" },
                  "html_url": { "type": "string" },
                  "repository": { "type": "object", "properties": { "full_name": { "type": "string" } } }
                }
              }
            }
          }
        } } } } }
      }
    }
  }
}"#;

// ---------------------------------------------------------------------------
// Input argument types (Parse, Don't Validate — TDP Pattern 2)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListReposArg {
    owner: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    per_page: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct GetFileArg {
    owner: String,
    repo: String,
    path: String,
    #[serde(rename = "ref")]
    git_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OwnerRepoStateArg {
    owner: String,
    repo: String,
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateIssueArg {
    owner: String,
    repo: String,
    title: String,
    body: Option<String>,
    labels: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct SearchCodeArg {
    query: String,
    per_page: Option<u8>,
}

// ---------------------------------------------------------------------------
// GitHubCapability
// ---------------------------------------------------------------------------

/// The GitHub capability.
///
/// Holds a [`GitHubClient`] configured with the operator-supplied PAT.
/// The client is the only path to the GitHub API; all six intent handlers
/// delegate to it after parsing and validating their arguments.
pub struct GitHubCapability {
    manifest: Manifest,
    client: GitHubClient,
}

impl GitHubCapability {
    /// Build a capability using the provided GitHub token.
    pub fn new(token: GitHubToken) -> Result<Self, DispatchError> {
        let manifest = Manifest::parse(DEFAULT_MANIFEST_JSON).map_err(|e| {
            DispatchError::Internal(format!("default manifest failed to parse: {e}"))
        })?;
        Ok(Self {
            manifest,
            client: GitHubClient::new(token),
        })
    }

    /// Override the GitHub API base URL (for GitHub Enterprise or tests).
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.client = self.client.with_api_base_url(url);
        self
    }

    /// Override the embedded manifest.
    pub fn with_manifest(mut self, manifest: Manifest) -> Self {
        self.manifest = manifest;
        self
    }
}

#[async_trait]
impl Capability for GitHubCapability {
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
            "list-repos" => self.dispatch_list_repos(body).await,
            "get-file" => self.dispatch_get_file(body).await,
            "list-issues" => self.dispatch_list_issues(body).await,
            "create-issue" => self.dispatch_create_issue(body).await,
            "list-pull-requests" => self.dispatch_list_pull_requests(body).await,
            "search-code" => self.dispatch_search_code(body).await,
            _ => Err(DispatchError::UnsupportedIntent(intent.clone())), // allow:clone — SDK variant takes owned IntentVerb; small string
        }
    }
}

// ---------------------------------------------------------------------------
// Intent handlers
// ---------------------------------------------------------------------------

impl GitHubCapability {
    async fn dispatch_list_repos(
        &self,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, DispatchError> {
        let arg: ListReposArg = parse_body(body, "expected {owner}")?;
        let owner = parse_owner(&arg.owner)?;
        self.client
            .list_repos(&owner, arg.kind.as_deref(), arg.per_page.unwrap_or(30))
            .await
            .map_err(map_github_error)
    }

    async fn dispatch_get_file(
        &self,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, DispatchError> {
        let arg: GetFileArg = parse_body(body, "expected {owner, repo, path}")?;
        let owner = parse_owner(&arg.owner)?;
        let repo = parse_repo(&arg.repo)?;
        self.client
            .get_file(&owner, &repo, &arg.path, arg.git_ref.as_deref())
            .await
            .map_err(map_github_error)
    }

    async fn dispatch_list_issues(
        &self,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, DispatchError> {
        let arg: OwnerRepoStateArg = parse_body(body, "expected {owner, repo}")?;
        let owner = parse_owner(&arg.owner)?;
        let repo = parse_repo(&arg.repo)?;
        self.client
            .list_issues(&owner, &repo, arg.state.as_deref())
            .await
            .map_err(map_github_error)
    }

    async fn dispatch_create_issue(
        &self,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, DispatchError> {
        let arg: CreateIssueArg = parse_body(body, "expected {owner, repo, title}")?;
        let owner = parse_owner(&arg.owner)?;
        let repo = parse_repo(&arg.repo)?;
        let labels = arg.labels.unwrap_or_default();
        self.client
            .create_issue(&owner, &repo, &arg.title, arg.body.as_deref(), &labels)
            .await
            .map_err(map_github_error)
    }

    async fn dispatch_list_pull_requests(
        &self,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, DispatchError> {
        let arg: OwnerRepoStateArg = parse_body(body, "expected {owner, repo}")?;
        let owner = parse_owner(&arg.owner)?;
        let repo = parse_repo(&arg.repo)?;
        self.client
            .list_pull_requests(&owner, &repo, arg.state.as_deref())
            .await
            .map_err(map_github_error)
    }

    async fn dispatch_search_code(
        &self,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, DispatchError> {
        let arg: SearchCodeArg = parse_body(body, "expected {query}")?;
        self.client
            .search_code(&arg.query, arg.per_page.unwrap_or(10))
            .await
            .map_err(map_github_error)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_body<T: serde::de::DeserializeOwned>(
    body: serde_json::Value,
    hint: &'static str,
) -> Result<T, DispatchError> {
    serde_json::from_value(body).map_err(|e| DispatchError::InvalidRequest(format!("{hint}: {e}")))
}

fn parse_owner(s: &str) -> Result<crate::types::RepoOwner, DispatchError> {
    crate::types::RepoOwner::parse(s).map_err(|e| DispatchError::InvalidRequest(e.to_string()))
}

fn parse_repo(s: &str) -> Result<crate::types::RepoName, DispatchError> {
    crate::types::RepoName::parse(s).map_err(|e| DispatchError::InvalidRequest(e.to_string()))
}

fn map_github_error(e: GitHubError) -> DispatchError {
    match e {
        GitHubError::ApiError {
            status: 404,
            message,
        } => DispatchError::NotFound(message),
        GitHubError::ApiError {
            status: 403,
            message,
        } => DispatchError::Forbidden(message),
        GitHubError::ApiError {
            status: 401,
            message,
        } => DispatchError::Forbidden(message),
        GitHubError::RateLimited { .. } => {
            DispatchError::Internal(format!("GitHub rate limit: {e}"))
        }
        GitHubError::InvalidBase64(_) | GitHubError::NotUtf8(_) => {
            DispatchError::Internal(e.to_string())
        }
        _ => DispatchError::Internal(e.to_string()),
    }
}
