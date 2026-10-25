//! Consumer-side library for the Ferridis protocol.
//!
//! `ferridis-client` is what an embedding host (IDE, agent runtime,
//! CLI) links against to talk to Ferridis capabilities. It composes:
//!
//! - [`Wallet`] — persisted [`StoredConnection`](ferridis_core::StoredConnection)
//!   set, with disk round-trip via the trusted-reconstruction
//!   projections on `StoredConnection`.
//! - [`Registry`] — in-memory cache of capabilities that have been
//!   registered with this client (their manifest, base URL, and a
//!   fetched-at timestamp).
//! - [`Client`] — the entry point. Compose `Wallet` + `Registry` +
//!   the HTTP client from [`ferridis_protocol`] and you have the four
//!   operations a host actually performs: `register`,
//!   `candidates_for_intent`, `insert_connection`, `dispatch`.
//!
//! # v0.1 scope and explicit deferrals
//!
//! The wire-protocol and architecture documents describe a fuller
//! consumer surface; not all of it ships in v0.1. The deferrals are
//! deliberate, not accidental:
//!
//! - **Wallet encryption.** v0.1 stores the wallet as plaintext JSON
//!   under the caller-supplied path, with file permissions clamped to
//!   `0o600` on Unix. **v0.2** will integrate the OS keychain via the
//!   `keyring` crate (Linux Secret Service / macOS Keychain / Windows
//!   Credential Manager). The on-disk shape is versioned (see
//!   [`wallet`]) so the migration is straightforward.
//! - **Org and public mesh tiers.** [`Registry`] holds only the
//!   personal tier — the host's locally-registered capabilities.
//!   The three-tier traversal (personal → org → public) is sketched
//!   in [`architecture.md`] and arrives in v0.2 alongside the public
//!   mesh registry deliverable.
//! - **Schema-driven request validation.** Per the doc note on
//!   [`ferridis_adapter_sdk::Capability`], request-body validation
//!   against the declared OpenAPI / AsyncAPI schema is v0.2. v0.1
//!   sends bodies through as-is and lets the adapter validate.
//! - **Event subscription management.** WebSocket and Server-Sent
//!   Events transports live in `ferridis-protocol` v0.2; client-side
//!   subscription management follows.
//! - **Confidence scoring on intent matches.** v0.1 returns a
//!   `Vec<CapabilityRef>` of all manifests that declare the requested
//!   intent. v0.2 adds scoring + tie-breaking.
//!
//! # Type-driven discipline
//!
//! - [`Wallet`] is constructed only via [`Wallet::open`] or
//!   [`Wallet::ephemeral`]; the on-disk JSON shape is versioned so
//!   future format migrations are explicit.
//! - The `Wallet → Connection<S>` projection is the
//!   [`ferridis_core::StoredConnection::as_authorized`] family of
//!   methods. The client never builds `Connection<S>` directly.
//! - [`ClientError`] is an enum (illegal states unrepresentable);
//!   variants distinguish "capability not registered" from "intent
//!   not supported by the registered manifest" from "no authorized
//!   connection" so call sites can act on the cause.
//!
//! [`architecture.md`]: ../../../../architecture.md

#![deny(missing_docs)]
#![forbid(unsafe_code)]
// Client errors carry rich context (URLs, source errors) by design,
// same rationale as in `ferridis-protocol`.
#![allow(clippy::result_large_err)]

pub mod adapters;
pub mod catalogue;
pub mod client;
pub mod discovery;
pub mod error;
pub mod mcp;
pub mod registry;
pub mod wallet;
pub mod wallet_store;

pub use client::{CallOrigin, Client, ResponseSchemaLoad, SchemaAttempt, SchemaFetchFailure};
pub use discovery::DiscoveryHandle;
pub use error::ClientError;
pub use ferridis_protocol::WsConnection;
pub use registry::{CapabilityBackend, CapabilityRecord, Registry, RegistryTier};
pub use wallet::Wallet;
pub use wallet_store::{KeychainStore, MemoryStore, WalletStore};
