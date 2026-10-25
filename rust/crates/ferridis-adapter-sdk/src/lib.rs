//! Framework for building Ferridis adapters (the publisher side).
//!
//! An adapter is a small process that publishes a Ferridis manifest and
//! handles intent calls against an underlying service. The SDK provides:
//!
//! - [`Capability`](capability::Capability) — the trait an adapter
//!   implements. It declares the manifest, the schema source, and the
//!   per-intent dispatch logic.
//! - [`AdapterServer`](server::AdapterServer) — an axum-based HTTP server
//!   that mounts one [`Capability`] and serves the manifest, the schema,
//!   and the intent endpoints.
//! - [`DispatchError`](dispatch::DispatchError) — the structured error
//!   that adapter dispatch implementations return.
//! - [`events`] — scaffolding for event publishing
//!   (webhook delivery in v0.1; WebSocket / SSE follow in v0.2).
//!
//! # Type-driven discipline
//!
//! - [`Capability`] is a trait, not a struct, so the SDK does not assume
//!   storage shape. Adapters own their state.
//! - [`DispatchError`] is an `enum` (illegal states unrepresentable).
//! - [`AdapterServer`] is built from a value, not configured via mutation
//!   after construction. The route table is derived from the capability's
//!   declared intents — wiring routes for an intent the manifest does
//!   not list is impossible by construction.

#![deny(missing_docs)]
#![forbid(unsafe_code)]
// Adapter dispatch errors are intentionally rich (URLs, source errors,
// payload context) so they're useful in logs.
#![allow(clippy::result_large_err)]

pub mod broker;
pub mod capability;
pub mod conformance;
pub mod dispatch;
pub mod events;
pub mod server;
pub mod validation;
pub mod ws_handler;

pub use broker::{
    AdapterUrl, AdapterUrlError, BrokerConfig, BrokerError, BrokerRegistration, BrokerUrl,
    BrokerUrlError, RegistrationPersistence, ServiceKind, ServiceName, ServiceNameError,
};
pub use capability::{
    Capability, FlowIntentStream, FlowStreamItem, IntentStream, SchemaSource, StreamItem,
};
pub use conformance::{ResponseConformance, check_response};
pub use dispatch::DispatchError;
pub use events::{Event, EventPublisher, WebhookPublisher};
pub use ferridis_protocol::{BackpressureSignal, StreamChunk};
pub use server::AdapterServer;
pub use validation::{ValidationError, ValidationOutcome, validate_body};
pub use ws_handler::{WsConnId, WsHandler, WsMessage};
