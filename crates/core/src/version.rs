//! MCP protocol revision negotiation (ADR 0018).
//!
//! See [the MCP spec][1] for authoritative definitions of each revision.
//!
//! [1]: https://modelcontextprotocol.io/specification/

/// MCP protocol revisions this client can negotiate (ADR 0018).
///
/// Wire structs keep raw `String` fields (serde and the zero-copy hot path
/// of ADR 0006 are untouched); this enum is the typed form used for
/// negotiation policy. A server answering `initialize` with a revision
/// outside this set triggers a warning by default and gates the run only
/// under `[validation] strict = true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProtocolVersion {
    /// The 2025-03-26 revision — the oldest revision this client negotiates.
    V2025_03_26,
    /// The 2025-06-18 revision — added `outputSchema` / `structuredContent`.
    V2025_06_18,
    /// The 2025-11-25 revision — current stable.
    V2025_11_25,
    /// The 2026-07-28 revision — stateless core: no `initialize` handshake,
    /// `_meta` on every request, `server/discover` (ADR 0019). Selecting it
    /// switches the client's connection to the stateless mode instead of
    /// participating in handshake negotiation. Implemented against the
    /// release candidate; re-verified against the final spec (2026-07-28).
    V2026_07_28,
}

impl ProtocolVersion {
    /// Revision advertised in `initialize` when `[server] protocol_version`
    /// is `"auto"` or unset.
    ///
    /// 2025-11-25 is advertised because a gap audit of that spec release
    /// confirmed it is backwards compatible and every addition is
    /// capability-gated. Servers
    /// on older revisions negotiate down; we accept anything in
    /// [`Self::SUPPORTED`]. Pin `[server] protocol_version = "2025-03-26"`
    /// to reproduce that revision's wire bytes exactly.
    pub const DEFAULT_ADVERTISED: Self = Self::V2025_11_25;

    /// Every **handshake** revision negotiable via `initialize` (ADR 0018),
    /// oldest first. The stateless 2026-07-28 revision is deliberately not
    /// in this set — it is selected explicitly via
    /// `[server] protocol_version = "2026-07-28"`, never negotiated.
    pub const SUPPORTED: &'static [Self] =
        &[Self::V2025_03_26, Self::V2025_06_18, Self::V2025_11_25];

    /// Every revision this crate knows, handshake and stateless alike.
    pub const ALL: &'static [Self] = &[
        Self::V2025_03_26,
        Self::V2025_06_18,
        Self::V2025_11_25,
        Self::V2026_07_28,
    ];

    /// The wire string for this revision (e.g. `"2025-11-25"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V2025_03_26 => "2025-03-26",
            Self::V2025_06_18 => "2025-06-18",
            Self::V2025_11_25 => "2025-11-25",
            Self::V2026_07_28 => "2026-07-28",
        }
    }

    /// `true` for revisions using the stateless core (no `initialize`;
    /// `_meta` on every request — ADR 0019).
    pub const fn is_stateless(self) -> bool {
        matches!(self, Self::V2026_07_28)
    }

    /// Parse a wire string; `None` for revisions outside [`Self::ALL`].
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|v| v.as_str() == s)
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_round_trips_every_known_revision() {
        for v in ProtocolVersion::ALL {
            assert_eq!(ProtocolVersion::parse(v.as_str()), Some(*v));
            assert_eq!(v.to_string(), v.as_str());
        }
    }

    #[test]
    fn stateless_revision_is_not_in_the_handshake_set() {
        assert!(ProtocolVersion::V2026_07_28.is_stateless());
        assert!(
            !ProtocolVersion::SUPPORTED.contains(&ProtocolVersion::V2026_07_28),
            "2026-07-28 is selected explicitly, never negotiated via initialize"
        );
        assert!(ProtocolVersion::SUPPORTED.iter().all(|v| !v.is_stateless()));
    }

    #[test]
    fn protocol_version_unknown_string_parses_to_none() {
        assert_eq!(ProtocolVersion::parse("9999-12-31"), None);
        assert_eq!(ProtocolVersion::parse(""), None);
        assert_eq!(ProtocolVersion::parse("2025-3-26"), None);
    }

    #[test]
    fn default_advertised_is_2025_11_25() {
        // Deliberately advanced in T1.2 (spec gap audit cleared it). Changing
        // this again is a user-visible wire change: CHANGELOG + audit first.
        assert_eq!(ProtocolVersion::DEFAULT_ADVERTISED.as_str(), "2025-11-25");
    }
}
