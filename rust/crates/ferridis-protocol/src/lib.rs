//! Wire-layer transport for the Ferridis protocol.
//!
//! This crate sits between [`ferridis_core`] (pure types) and the
//! consumer / adapter crates. It owns the I/O: HTTP transport, OAuth
//! flows, manifest fetching, schema fetching, and authenticated calls.
//!
//! # What this crate does
//!
//! - [`Client`] — shared HTTP client and entry point.
//! - [`fetch_manifest`](manifest::fetch_manifest) — resolve a `ferridis://`
//!   capability reference into a validated [`ferridis_core::Manifest`].
//! - [`fetch_schema`](schema::fetch_schema) — lazily load the full
//!   OpenAPI / AsyncAPI schema from a manifest's `schema_url`.
//! - [`oauth`] — OAuth 2.1 / OAuth 2.0 + PKCE primitives (verifier,
//!   challenge, authorization URL, token exchange).
//! - [`Broker`](broker::Broker) — the connection broker. Takes a manifest,
//!   produces a [`Connection<Pending>`](ferridis_core::Connection), and
//!   completes it into [`Connection<Authorized>`](ferridis_core::Connection)
//!   when the OAuth callback arrives.
//! - [`call`](call::call) — make a bearer-token authenticated call
//!   against a service, with the Ferridis advisory headers attached.
//!
//! # What this crate does not do
//!
//! - Wallet persistence. The on-disk encrypted wallet lives in
//!   `ferridis-client`.
//! - Routing. Intent → capability matching is a `ferridis-client` concern.
//! - Adapter-side serving. That's `ferridis-adapter-sdk`.
//!
//! # Design notes
//!
//! Type-driven, with the same two disciplines as `ferridis-core`:
//!
//! - **Typestate** is honoured. [`Broker::complete`](broker::Broker::complete)
//!   consumes the broker's pending state and the matched
//!   [`Connection<Pending>`](ferridis_core::Connection), returning a
//!   [`Connection<Authorized>`](ferridis_core::Connection). Old states
//!   cannot be reused.
//! - **Illegal states unrepresentable.** [`oauth::PkceVerifier`] is a
//!   private newtype constructed only from cryptographically random bytes;
//!   downstream code can rely on it being well-formed.

#![deny(missing_docs)]
#![forbid(unsafe_code)]
// Protocol errors carry rich context (URLs, source errors) by design —
// the wire layer needs that context to be useful in logs and at call
// sites. We accept the larger `Err` variant in exchange.
#![allow(clippy::result_large_err)]

pub mod broker;
pub mod call;
pub mod client;
pub mod error;
pub mod events;
pub mod manifest;
pub mod mesh;
pub mod oauth;
pub mod openapi;
pub mod schema;
pub mod signing;
pub mod ws;

pub mod backpressure;
pub mod discovery;
pub use backpressure::{BackpressureSignal, StreamChunk};
pub use broker::Broker;
pub use call::{CallRequest, CallResponse, call, call_anonymous};
pub use client::Client;
pub use discovery::{DiscoveredService, MdnsScanner, ServiceKind};
pub use error::ProtocolError;
pub use events::{
    ReconnectCursor, ServerEvent, parse_sse_stream, subscribe, subscribe_with_cursor,
};
pub use manifest::fetch_manifest;
pub use mesh::{FederatedMesh, MeshArtifact, MeshClient, MeshIndex, MeshIndexEntry};
pub use oauth::{AuthorizationUrl, PkceChallenge, PkceVerifier, TokenResponse};
pub use openapi::{OpenApiError, ResponseSchema, ResponseSchemas};
pub use schema::{SchemaBytes, fetch_schema};
pub use signing::{
    CosignBundle, SigningError, TrustRoot, VerifiedManifest, verify_signed_manifest,
    verify_signed_manifest_with_trust_root,
};
pub use ws::{ClientMessage, WsConnection, WsReceiver, WsSender, connect_ws};
