//! The code-execution container, and the clock its expiry is read against.

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::{PtcError, ResponseError};
use crate::ids::ContainerId;

/// When a container stops accepting continuations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContainerExpiry(OffsetDateTime);

impl ContainerExpiry {
    /// Parse the response's `expires_at`.
    pub(crate) fn parse(raw: &str) -> Result<Self, ResponseError> {
        OffsetDateTime::parse(raw, &Rfc3339)
            .map(Self)
            .map_err(|source| ResponseError::InvalidExpiry {
                value: raw.to_owned(),
                source,
            })
    }

    /// The instant.
    #[must_use]
    pub fn at(&self) -> OffsetDateTime {
        self.0
    }
}

impl std::fmt::Display for ContainerExpiry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A code-execution container, as the latest response described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    id: ContainerId,
    expires_at: ContainerExpiry,
}

impl Container {
    pub(crate) fn new(id: ContainerId, expires_at: ContainerExpiry) -> Self {
        Self { id, expires_at }
    }

    /// The container's id.
    #[must_use]
    pub fn id(&self) -> &ContainerId {
        &self.id
    }

    /// When it expires.
    #[must_use]
    pub fn expires_at(&self) -> ContainerExpiry {
        self.expires_at
    }

    /// Refuse once `now` has reached the expiry.
    pub(crate) fn ensure_live(&self, now: OffsetDateTime) -> Result<(), PtcError> {
        if now >= self.expires_at.0 {
            return Err(PtcError::ContainerExpired {
                container: self.id.clone(), // allow:clone — the error outlives the container, which is dropped with the refused turn
                expired_at: self.expires_at,
            });
        }
        Ok(())
    }
}

/// The clock container expiry is checked against. Injected so expiry is
/// testable without waiting.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// The current instant.
    fn now(&self) -> OffsetDateTime;
}

/// The system clock, in UTC.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(expires: &str) -> Container {
        Container::new(
            ContainerId::parse("container.id", "container_xyz789".to_owned()).expect("valid id"),
            ContainerExpiry::parse(expires).expect("valid expiry"),
        )
    }

    #[test]
    fn a_container_is_live_until_its_expiry_and_refused_from_it() {
        let c = container("2026-01-20T14:30:00Z");
        let at = |s: &str| OffsetDateTime::parse(s, &Rfc3339).expect("valid instant");
        assert!(c.ensure_live(at("2026-01-20T14:29:59Z")).is_ok());
        assert!(matches!(
            c.ensure_live(at("2026-01-20T14:30:00Z")),
            Err(PtcError::ContainerExpired { container, .. }) if container.as_str() == "container_xyz789"
        ));
    }

    #[test]
    fn rejects_a_non_rfc3339_expiry() {
        assert!(matches!(
            ContainerExpiry::parse("tomorrow"),
            Err(ResponseError::InvalidExpiry { value, .. }) if value == "tomorrow"
        ));
    }
}
