//! In-memory registry of capabilities this client knows about.
//!
//! A [`CapabilityRecord`] is what the client remembers per registered
//! capability: the validated [`Manifest`], the base URL its intent
//! endpoints live under, and a fetched-at timestamp for TTL checks.
//!
//! v0.1 holds only the **personal tier** — capabilities the host has
//! explicitly registered. The three-tier traversal (personal → org →
//! public) sketched in `architecture.md` arrives in v0.2 when the
//! public mesh registry deliverable lands; the [`Registry`] API is
//! shaped so the org/public tiers can be added without disturbing
//! call sites.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use time::{Duration as TimeDuration, OffsetDateTime};
use url::Url;

use ferridis_core::{CapabilityRef, IntentVerb, Manifest};
use ferridis_protocol::ResponseSchemas;

use crate::mcp::McpClient;

/// Discovery scope (tier) of a registered capability.
///
/// The architecture's three-tier discovery model: callers register
/// capabilities they trust most as `Personal`; the org administrator's
/// push lands as `Org`; capabilities discovered via the public mesh
/// (M4) come in as `Public`. `candidates_for_intent` returns matches
/// in this priority order so the host's own configuration always
/// wins ties.
///
/// `Public` is wired in v0.2 but no records actually arrive through
/// it until the public-mesh registry (M4) and signed-manifest scheme
/// (M3) land. Hosts can still tag records as `Public` manually for
/// integration testing.
///
/// The variant order matters: `derive(Ord)` makes Personal < Org <
/// Public, which is exactly the comparator the candidate sort relies
/// on. Do not reorder.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
pub enum RegistryTier {
    /// The host's own registrations. Highest priority.
    #[default]
    Personal,
    /// Pushed or pre-approved by the org. Loaded from a shared config
    /// or pulled from an org-specific endpoint at startup.
    Org,
    /// Discovered via the public mesh. Lowest priority.
    Public,
}

/// A score in 0–100 indicating how much a capability should be
/// preferred when multiple candidates declare the same intent.
///
/// v0.4 derives scores from tier only; v0.5 will incorporate
/// manifest-declared quality signals (latency hints, SLA declarations).
/// Private inner value — callers use [`as_u8`](Self::as_u8) and the
/// tier constructors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConfidenceScore(u8);

impl ConfidenceScore {
    /// Score for a Personal-tier capability (highest priority).
    pub fn personal() -> Self {
        Self(100)
    }

    /// Score for an Org-tier capability.
    pub fn org() -> Self {
        Self(66)
    }

    /// Score for a Public-tier capability (lowest priority).
    pub fn public() -> Self {
        Self(33)
    }

    /// Derive the appropriate score for a [`RegistryTier`].
    pub fn for_tier(tier: RegistryTier) -> Self {
        match tier {
            RegistryTier::Personal => Self::personal(),
            RegistryTier::Org => Self::org(),
            RegistryTier::Public => Self::public(),
        }
    }

    /// The raw score value in `0..=100`.
    pub fn as_u8(self) -> u8 {
        self.0
    }
}

/// A candidate returned by [`Registry::candidates_for_intent_scored`],
/// pairing a [`CapabilityRef`] with its [`ConfidenceScore`].
#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    capability: CapabilityRef,
    score: ConfidenceScore,
}

impl ScoredCandidate {
    /// Construct a scored candidate.
    pub fn new(capability: CapabilityRef, score: ConfidenceScore) -> Self {
        Self { capability, score }
    }

    /// The capability this candidate refers to.
    pub fn capability(&self) -> &CapabilityRef {
        &self.capability
    }

    /// The confidence score for this candidate.
    pub fn score(&self) -> ConfidenceScore {
        self.score
    }
}

/// Default freshness window for cached manifests.
///
/// A record older than this is considered stale by [`Registry::is_fresh`];
/// the client uses that signal to decide whether to refetch. v0.2 will
/// make this configurable per capability.
pub const DEFAULT_MANIFEST_TTL: TimeDuration = TimeDuration::hours(1);

/// Where dispatch for a capability is routed.
///
/// - [`CapabilityBackend::Native`] — Ferridis-native adapter at the
///   given base URL. Dispatch goes over HTTP through
///   [`ferridis_protocol`].
/// - [`CapabilityBackend::Mcp`] — an MCP server consumed by the
///   in-process [`McpClient`]. Dispatch translates the Ferridis intent
///   verb back to the original MCP tool name and calls `tools/call`.
#[derive(Clone)]
pub enum CapabilityBackend {
    /// A Ferridis-native adapter.
    Native {
        /// Base URL the adapter serves its `/intents/:verb` routes under.
        base_url: Url,
    },
    /// An MCP server consumed as a `Tier::Native` capability.
    Mcp {
        /// The live MCP connection.
        client: McpClient,
        /// Ferridis intent verb → original MCP tool name.
        intent_to_mcp: Arc<HashMap<IntentVerb, String>>,
    },
}

impl std::fmt::Debug for CapabilityBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapabilityBackend::Native { base_url } => f
                .debug_struct("Native")
                .field("base_url", base_url)
                .finish(),
            CapabilityBackend::Mcp { intent_to_mcp, .. } => f
                .debug_struct("Mcp")
                .field("server", &"<McpClient>")
                .field("intents", &intent_to_mcp.len())
                .finish(),
        }
    }
}

/// What the client remembers about one registered capability.
#[derive(Debug, Clone)]
pub struct CapabilityRecord {
    capability: CapabilityRef,
    manifest: Manifest,
    backend: CapabilityBackend,
    /// Discovery scope. Defaults to `Personal`; hosts retag records
    /// via [`Registry::set_tier`] when they come from an org config
    /// or the public mesh.
    tier: RegistryTier,
    /// Per-intent `inputSchema` carried alongside the manifest.
    ///
    /// Populated when the upstream source advertises typed schemas
    /// (e.g., MCP servers via `tools/list`). Re-publishers should
    /// prefer these over the manifest's capability-level OpenAPI URL
    /// because they're per-intent and already parsed into JSON.
    /// `None` means no per-intent schemas were captured — the caller
    /// is responsible for falling back to whatever defaults apply.
    input_schemas: Option<Arc<HashMap<IntentVerb, serde_json::Value>>>,
    /// Per-intent response schemas read from the capability's OpenAPI
    /// document by [`crate::Client::load_response_schemas`]. `None` until
    /// that runs, and it is never run by registration itself: schemas
    /// stay lazy-loaded (resolved friction #1), so only callers that
    /// need them up front, such as a tool catalogue, opt in.
    response_schemas: Option<Arc<ResponseSchemas>>,
    fetched_at: OffsetDateTime,
}

impl CapabilityRecord {
    /// Build a Native-backed record (Ferridis adapter). The fetched-at
    /// timestamp is set to `now`.
    pub fn new(capability: CapabilityRef, manifest: Manifest, base_url: Url) -> Self {
        Self::with_backend(capability, manifest, CapabilityBackend::Native { base_url })
    }

    /// Build a record with an explicit backend (Native or Mcp).
    /// Defaults to [`RegistryTier::Personal`]; use [`Self::with_tier`]
    /// or [`Registry::set_tier`] for org / public records.
    pub fn with_backend(
        capability: CapabilityRef,
        manifest: Manifest,
        backend: CapabilityBackend,
    ) -> Self {
        Self {
            capability,
            manifest,
            backend,
            tier: RegistryTier::Personal,
            input_schemas: None,
            response_schemas: None,
            fetched_at: OffsetDateTime::now_utc(),
        }
    }

    /// Builder-style tier override. Returns `self` so it can chain
    /// with [`Self::with_input_schemas`] inside an
    /// [`crate::client::Client`] registration.
    #[must_use]
    pub fn with_tier(mut self, tier: RegistryTier) -> Self {
        self.tier = tier;
        self
    }

    /// Builder-style attachment of per-intent input schemas. Takes
    /// ownership of the map; wraps it in `Arc` for cheap downstream
    /// cloning. Pass an empty map to mean "no schemas" — that becomes
    /// `None` so accessors can short-circuit.
    #[must_use]
    pub fn with_input_schemas(mut self, schemas: HashMap<IntentVerb, serde_json::Value>) -> Self {
        if !schemas.is_empty() {
            self.input_schemas = Some(Arc::new(schemas));
        }
        self
    }

    /// The capability this record describes.
    pub fn capability(&self) -> &CapabilityRef {
        &self.capability
    }

    /// The cached manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Dispatch backend (Native HTTP vs MCP).
    pub fn backend(&self) -> &CapabilityBackend {
        &self.backend
    }

    /// Per-intent input schemas if the upstream source advertised them.
    /// Native-backed records return `None`.
    pub fn input_schemas(&self) -> Option<&Arc<HashMap<IntentVerb, serde_json::Value>>> {
        self.input_schemas.as_ref()
    }

    /// Per-intent response schemas, once
    /// [`crate::Client::load_response_schemas`] has found some.
    pub fn response_schemas(&self) -> Option<&Arc<ResponseSchemas>> {
        self.response_schemas.as_ref()
    }

    /// The discovery tier this record was registered under.
    pub fn tier(&self) -> RegistryTier {
        self.tier
    }

    /// The base URL the adapter's intent endpoints live under, for
    /// Native-backed records. `None` for MCP-backed records.
    pub fn base_url(&self) -> Option<&Url> {
        match &self.backend {
            CapabilityBackend::Native { base_url } => Some(base_url),
            CapabilityBackend::Mcp { .. } => None,
        }
    }

    /// When the manifest was last fetched.
    pub fn fetched_at(&self) -> OffsetDateTime {
        self.fetched_at
    }
}

/// In-memory registry. Keyed by [`CapabilityRef`].
#[derive(Debug, Default)]
pub struct Registry {
    records: HashMap<CapabilityRef, CapabilityRecord>,
}

impl Registry {
    /// Build an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a record. Returns any prior record.
    pub fn insert(&mut self, record: CapabilityRecord) -> Option<CapabilityRecord> {
        self.records.insert(record.capability().clone(), record)
    }

    /// Borrow a record by capability.
    pub fn get(&self, capability: &CapabilityRef) -> Option<&CapabilityRecord> {
        self.records.get(capability)
    }

    /// Iterate all records.
    pub fn iter(&self) -> impl Iterator<Item = &CapabilityRecord> + '_ {
        self.records.values()
    }

    /// Number of records.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// All [`CapabilityRef`]s whose cached manifest declares this intent,
    /// together with a [`ConfidenceScore`] derived from their
    /// [`RegistryTier`]. Results are sorted by score descending
    /// (Personal first, then Org, then Public); within a tier candidates
    /// are sorted lexicographically by `CapabilityRef` for stable output.
    pub fn candidates_for_intent_scored(&self, intent: &IntentVerb) -> Vec<ScoredCandidate> {
        let mut matched: Vec<ScoredCandidate> = self
            .records
            .values()
            .filter(|r| r.manifest.intents().contains(intent))
            .map(|r| {
                let score = ConfidenceScore::for_tier(r.tier);
                ScoredCandidate::new(r.capability.clone(), score) // clone: records are borrowed; ScoredCandidate owns the ref
            })
            .collect();
        matched.sort_by(|a, b| {
            b.score()
                .as_u8()
                .cmp(&a.score().as_u8())
                .then_with(|| a.capability().to_string().cmp(&b.capability().to_string()))
        });
        matched
    }

    /// All [`CapabilityRef`]s whose cached manifest declares this intent,
    /// ordered by [`RegistryTier`] priority (Personal first, then Org,
    /// then Public). Within a tier the order is currently undefined;
    /// scoring + stable tie-breaking is queued for v0.3.
    pub fn candidates_for_intent(&self, intent: &IntentVerb) -> Vec<CapabilityRef> {
        let mut matched: Vec<&CapabilityRecord> = self
            .records
            .values()
            .filter(|r| r.manifest.intents().contains(intent))
            .collect();
        matched.sort_by_key(|r| r.tier);
        matched.into_iter().map(|r| r.capability.clone()).collect()
    }

    /// Same as [`Self::candidates_for_intent`] but groups matches by
    /// tier so callers can render tier-aware UIs or apply per-tier
    /// policies (e.g., "always prompt before invoking a public-tier
    /// capability").
    pub fn candidates_for_intent_by_tier(
        &self,
        intent: &IntentVerb,
    ) -> HashMap<RegistryTier, Vec<CapabilityRef>> {
        let mut out: HashMap<RegistryTier, Vec<CapabilityRef>> = HashMap::new();
        for record in self.records.values() {
            if record.manifest.intents().contains(intent) {
                out.entry(record.tier)
                    .or_default()
                    .push(record.capability.clone());
            }
        }
        out
    }

    /// Retag an existing record's [`RegistryTier`]. Returns the prior
    /// tier, or [`crate::ClientError::CapabilityNotRegistered`] if no
    /// such record exists.
    ///
    /// The intended workflow for org-tier loading: register a batch of
    /// org-pushed adapters through the normal `Client::register*`
    /// methods (which default to Personal), then call this for each
    /// one to relabel as `Org`.
    pub fn set_tier(
        &mut self,
        capability: &CapabilityRef,
        tier: RegistryTier,
    ) -> Result<RegistryTier, crate::ClientError> {
        let record = self
            .records
            .get_mut(capability)
            .ok_or_else(|| crate::ClientError::CapabilityNotRegistered(capability.clone()))?;
        let prior = record.tier;
        record.tier = tier;
        Ok(prior)
    }

    /// Attach per-intent response schemas to an existing record,
    /// replacing any it held. Returns
    /// [`crate::ClientError::CapabilityNotRegistered`] if no such record
    /// exists.
    pub fn set_response_schemas(
        &mut self,
        capability: &CapabilityRef,
        schemas: ResponseSchemas,
    ) -> Result<(), crate::ClientError> {
        let record = self
            .records
            .get_mut(capability)
            .ok_or_else(|| crate::ClientError::CapabilityNotRegistered(capability.clone()))?; // allow:clone — the error variant owns the ref it names; only on the failure path
        record.response_schemas = Some(Arc::new(schemas));
        Ok(())
    }

    /// Whether the record for `capability` is younger than [`DEFAULT_MANIFEST_TTL`].
    pub fn is_fresh(&self, capability: &CapabilityRef) -> bool {
        let Some(record) = self.records.get(capability) else {
            return false;
        };
        let age = OffsetDateTime::now_utc() - record.fetched_at;
        age < DEFAULT_MANIFEST_TTL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST_FS: &str = r#"{
        "ferridis_version": "0.1",
        "id": "test.fs.v1",
        "name": "FS",
        "category": "files",
        "summary": "Filesystem test capability.",
        "intents": ["read-file", "write-file"],
        "schema": { "type": "openapi-3", "url": "https://x/s.yaml" },
        "tiers": ["native"],
        "auth": { "type": "none" }
    }"#;

    const MANIFEST_MSG: &str = r#"{
        "ferridis_version": "0.1",
        "id": "test.msg.v1",
        "name": "MSG",
        "category": "messaging",
        "summary": "Messaging test capability.",
        "intents": ["send-message"],
        "schema": { "type": "openapi-3", "url": "https://x/s.yaml" },
        "tiers": ["native"],
        "auth": { "type": "none" }
    }"#;

    fn rec(manifest_json: &str, base: &str) -> CapabilityRecord {
        let m = Manifest::parse(manifest_json).unwrap();
        let parts: Vec<&str> = m.id().split('.').collect();
        let cap = CapabilityRef::parse(&format!(
            "ferridis://public.ferridis.io/{}/{}@{}",
            parts[0], parts[1], parts[2]
        ))
        .unwrap();
        let base = Url::parse(base).unwrap();
        CapabilityRecord::new(cap, m, base)
    }

    #[test]
    fn candidates_returns_only_capabilities_declaring_the_intent() {
        let mut r = Registry::new();
        r.insert(rec(MANIFEST_FS, "http://fs.local"));
        r.insert(rec(MANIFEST_MSG, "http://msg.local"));

        let read = IntentVerb::parse("read-file").unwrap();
        let send = IntentVerb::parse("send-message").unwrap();
        let nope = IntentVerb::parse("delete-event").unwrap();

        assert_eq!(r.candidates_for_intent(&read).len(), 1);
        assert_eq!(r.candidates_for_intent(&send).len(), 1);
        assert_eq!(r.candidates_for_intent(&nope).len(), 0);
    }

    #[test]
    fn fresh_record_is_within_ttl() {
        let mut r = Registry::new();
        let record = rec(MANIFEST_FS, "http://fs.local");
        let cap = record.capability().clone();
        r.insert(record);
        assert!(r.is_fresh(&cap));
    }

    #[test]
    fn insert_replaces_existing_record() {
        let mut r = Registry::new();
        let record = rec(MANIFEST_FS, "http://first");
        let cap = record.capability().clone();
        r.insert(record);

        let replaced = r.insert(rec(MANIFEST_FS, "http://second"));
        assert!(replaced.is_some());
        assert_eq!(r.len(), 1);
        assert_eq!(
            r.get(&cap).unwrap().base_url().unwrap().as_str(),
            "http://second/"
        );
    }

    const MANIFEST_FS_ORG: &str = r#"{
        "ferridis_version": "0.1",
        "id": "org.fs.v1",
        "name": "FS (org-pushed)",
        "category": "files",
        "summary": "Org-pushed filesystem capability.",
        "intents": ["read-file"],
        "schema": { "type": "openapi-3", "url": "https://x/s.yaml" },
        "tiers": ["native"],
        "auth": { "type": "none" }
    }"#;

    const MANIFEST_FS_PUBLIC: &str = r#"{
        "ferridis_version": "0.1",
        "id": "public.fs.v1",
        "name": "FS (public mesh)",
        "category": "files",
        "summary": "Public-mesh filesystem capability.",
        "intents": ["read-file"],
        "schema": { "type": "openapi-3", "url": "https://x/s.yaml" },
        "tiers": ["native"],
        "auth": { "type": "none" }
    }"#;

    #[test]
    fn records_default_to_personal_tier() {
        let r = rec(MANIFEST_FS, "http://fs.local");
        assert_eq!(r.tier(), RegistryTier::Personal);
    }

    #[test]
    fn set_tier_retags_existing_record() {
        let mut r = Registry::new();
        let record = rec(MANIFEST_FS_ORG, "http://org.local");
        let cap = record.capability().clone();
        r.insert(record);
        let prior = r.set_tier(&cap, RegistryTier::Org).unwrap();
        assert_eq!(prior, RegistryTier::Personal);
        assert_eq!(r.get(&cap).unwrap().tier(), RegistryTier::Org);
    }

    #[test]
    fn set_tier_errors_on_missing_capability() {
        let mut r = Registry::new();
        let cap = CapabilityRef::parse("ferridis://public.ferridis.io/x/y@v1").unwrap();
        let err = r.set_tier(&cap, RegistryTier::Public).unwrap_err();
        assert!(matches!(
            err,
            crate::ClientError::CapabilityNotRegistered(_)
        ));
    }

    /// Three records each declaring `read-file`, registered in
    /// scrambled-tier order. `candidates_for_intent` must return them
    /// Personal → Org → Public so the host's own config always wins.
    #[test]
    fn candidates_sorted_by_tier_priority() {
        let mut r = Registry::new();
        let p = rec(MANIFEST_FS, "http://personal.local");
        let o = rec(MANIFEST_FS_ORG, "http://org.local");
        let pb = rec(MANIFEST_FS_PUBLIC, "http://public.local");
        let p_cap = p.capability().clone();
        let o_cap = o.capability().clone();
        let pb_cap = pb.capability().clone();
        // Insert in reverse priority so a no-op sort would fail.
        r.insert(pb);
        r.insert(o);
        r.insert(p);
        r.set_tier(&o_cap, RegistryTier::Org).unwrap();
        r.set_tier(&pb_cap, RegistryTier::Public).unwrap();

        let intent = IntentVerb::parse("read-file").unwrap();
        let got = r.candidates_for_intent(&intent);
        assert_eq!(got, vec![p_cap, o_cap, pb_cap]);
    }

    #[test]
    fn candidates_by_tier_groups_matches() {
        let mut r = Registry::new();
        let p = rec(MANIFEST_FS, "http://personal.local");
        let o = rec(MANIFEST_FS_ORG, "http://org.local");
        let o_cap = o.capability().clone();
        r.insert(p);
        r.insert(o);
        r.set_tier(&o_cap, RegistryTier::Org).unwrap();

        let intent = IntentVerb::parse("read-file").unwrap();
        let grouped = r.candidates_for_intent_by_tier(&intent);
        assert_eq!(grouped[&RegistryTier::Personal].len(), 1);
        assert_eq!(grouped[&RegistryTier::Org].len(), 1);
        assert!(!grouped.contains_key(&RegistryTier::Public));
    }
}
