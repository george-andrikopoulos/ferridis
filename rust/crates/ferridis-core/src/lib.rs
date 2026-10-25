//! Core type definitions for the Ferridis protocol.
//!
//! This crate is pure types — no I/O, no async runtime. It establishes the
//! type-driven foundation for the rest of the workspace.
//!
//! # Design principles
//!
//! Two type-driven design disciplines are applied throughout:
//!
//! 1. **Typestate pattern.** Lifecycle-bearing types (most prominently
//!    [`connection::Connection`]) carry their state in a type parameter
//!    so that illegal transitions are rejected at compile time. State
//!    transitions consume `self`, so an old state can never be reused
//!    after a transition.
//!
//! 2. **Illegal states unrepresentable.** Validated values (like
//!    [`manifest::Manifest`], [`intent::IntentVerb`], [`tier::Tiers`],
//!    [`token::AccessToken`]) can only be constructed through validating
//!    parsers; downstream code can rely on every value being structurally
//!    and semantically valid.
//!
//! These disciplines are not stylistic preferences — they are how the
//! protocol earns its safety claims. The wire-format work in
//! `ferridis-protocol` builds on the guarantees this crate provides.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod caller;
pub mod capability;
pub mod connection;
pub mod error;
pub mod intent;
pub mod manifest;
pub mod storage;
pub mod tier;
pub mod token;

pub use caller::CallerPolicy;
pub use capability::{CapabilityRef, CapabilityVersion};
pub use connection::{Authorized, Connection, ConnectionId, Expired, Pending, Revoked};
pub use error::FerridisError;
pub use intent::IntentVerb;
pub use manifest::{
    AuthMethod, Category, ChannelTransport, EventChannel, IntentKind, IntentMetadata, Manifest,
    Summary,
};
pub use storage::StoredConnection;
pub use tier::{Tier, Tiers};
pub use token::{AccessToken, RefreshToken};
