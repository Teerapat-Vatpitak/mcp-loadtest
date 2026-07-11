//! SSRF host-allowlist + always-on private/loopback/link-local IP-literal
//! block for the outbound network transports (http / sse / ws).
//!
//! See ADR 0012. The guard runs *before* any socket is opened: every transport
//! parses its URL, then runs it through `super::resolve::resolve_and_check`
//! (which calls [`HostGuard::check_url`] first) before building the reqwest
//! client / dialing the WebSocket.
//!
//! ## Threat model
//!
//! - **IP-literal URLs** pointing at private / loopback / link-local / ULA /
//!   reserved space are *always blocked* unless the literal is explicitly
//!   listed in `[server].allowed_hosts` (an operator escape hatch for local
//!   testing, e.g. `127.0.0.1`).
//! - **Hostname URLs** are constrained by `allowed_hosts` (exact,
//!   case-insensitive, no wildcard) at this layer. A hostname that *resolves*
//!   to a private IP is caught one layer up: `super::resolve` resolves the
//!   name once, rejects if any resolved address is private, and pins the
//!   vetted addresses for the actual connection (DNS-rebinding defense,
//!   ADR 0016 — closes the gap ADR 0012 accepted).
//! - An empty / unset `allowed_hosts` means "allow any *public* host" — the
//!   IP-literal block still applies. Security here is opt-in for backward
//!   compatibility with existing configs.

use std::net::IpAddr;

use mcp_loadtest_core::config::ServerConfig;

use super::TransportError;

/// Host-allowlist + private-IP guard. Construct via [`HostGuard::from_config`].
///
/// This is part of the public API: the outbound transports
/// ([`super::http::HttpTransport`], [`super::sse::SseTransport`],
/// [`super::ws::WsTransport`]) take `&HostGuard` in their `connect`
/// constructors, so callers that build transports directly need to construct
/// one. See ADR 0012 for the threat model.
#[derive(Debug, Clone)]
pub struct HostGuard {
    /// Allowed hostnames / IP literals, stored ASCII-lowercased. Empty = no
    /// allowlist constraint (public hosts pass; private IP literals still
    /// blocked unless listed here).
    allowed: Vec<String>,
}

impl HostGuard {
    /// Build a guard from a server config. `allowed_hosts` entries are
    /// ASCII-lowercased so matching is case-insensitive.
    pub fn from_config(cfg: &ServerConfig) -> Self {
        let allowed = cfg
            .allowed_hosts
            .iter()
            .map(|h| h.to_ascii_lowercase())
            .collect();
        Self { allowed }
    }

    /// Reject `url` if its host is a blocked IP literal or (when an allowlist
    /// is configured) not in the allowlist. The literal substring
    /// `blocked host` appears in every rejection message so the CLI hint
    /// layer can detect SSRF rejections — keep these messages stable.
    ///
    /// IP detection goes through the *typed* [`url::Host`] the parser already
    /// produced, **not** `host_str().parse::<IpAddr>()`: the `url` crate
    /// renders IPv6 hosts bracketed (`[::1]`), and `[::1]` does not parse as
    /// an `IpAddr`, so the string path silently skipped the private-IP block
    /// for *every* IPv6 literal (loopback / ULA / link-local / IPv4-mapped).
    /// The typed enum classifies the host once, at parse time, with no
    /// re-parse ambiguity.
    pub fn check_url(&self, url: &url::Url) -> Result<(), TransportError> {
        let host = url.host().ok_or_else(|| {
            TransportError::Other("blocked host: URL has no host (SSRF guard, ADR 0012)".into())
        })?;

        let ip: Option<IpAddr> = match host {
            url::Host::Ipv4(v4) => Some(IpAddr::V4(v4)),
            url::Host::Ipv6(v6) => Some(IpAddr::V6(v6)),
            url::Host::Domain(_) => None,
        };

        // Canonical, bracket-less host string used for both allowlist
        // membership and rejection messages. For IP literals this is the
        // `Ipv4Addr` / `Ipv6Addr` `Display` form (e.g. `127.0.0.1`, `::1`) —
        // the same form operators write in `allowed_hosts` and that
        // `config.rs` validated.
        let host_str: String = match host {
            url::Host::Domain(d) => d.to_string(),
            url::Host::Ipv4(v4) => v4.to_string(),
            url::Host::Ipv6(v6) => v6.to_string(),
        };

        if let Some(ip) = ip {
            // IP-literal URL. The escape hatch wins: if the operator listed
            // this exact literal, allow it even if it's loopback/private
            // (e.g. `127.0.0.1` for local testing).
            if self.host_allowed(&host_str) {
                return Ok(());
            }
            if is_blocked_ip(ip) {
                return Err(TransportError::Other(format!(
                    "blocked host `{host_str}`: resolves to a \
                     private/loopback/link-local/reserved address and is not in allowed_hosts \
                     (SSRF guard, ADR 0012)"
                )));
            }
            // Public IP literal — fall through to the allowlist check.
        }

        // Hostname (or public IP literal): only the allowlist constrains it.
        // Empty allowlist => allow (opt-in security, backward-compat).
        if self.allowed.is_empty() || self.host_allowed(&host_str) {
            return Ok(());
        }
        Err(TransportError::Other(format!(
            "blocked host `{host_str}`: not in allowed_hosts (SSRF guard, ADR 0012)"
        )))
    }

    /// Exact, ASCII-case-insensitive membership. No wildcard / suffix match —
    /// `api.example.com.attacker.com` must NOT satisfy an entry of
    /// `api.example.com`.
    ///
    /// `pub(crate)` so `super::resolve` can apply the same escape-hatch
    /// semantics to hostname *resolutions* (ADR 0016): a listed hostname may
    /// resolve to a private address, exactly like a listed literal.
    pub(crate) fn host_allowed(&self, host: &str) -> bool {
        let host_lc = host.to_ascii_lowercase();
        self.allowed.iter().any(|a| a == &host_lc)
    }
}

/// Is `ip` in private / loopback / link-local / ULA / reserved space we never
/// want an operator-supplied URL to reach by IP literal?
///
/// `pub(crate)`: `super::resolve` runs *resolved* addresses through the
/// same predicate (ADR 0016), so the literal layer and the resolver layer
/// can never disagree about what "private" means.
///
/// Deliberately does **not** use `Ipv4Addr::is_global` (unstable on the
/// MSRV 1.88 toolchain). The explicit predicate set below covers the
/// SSRF-relevant ranges: 127/8, 10/8, 172.16/12, 192.168/16, 169.254/16,
/// 0.0.0.0, 255.255.255.255, and the IPv6 equivalents (::1, ::, fc00::/7,
/// fe80::/10, plus IPv4-mapped addresses re-checked through the v4 path).
pub(crate) fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            // ULA fc00::/7 — high 7 bits are 1111110.
            if (v6.octets()[0] & 0xfe) == 0xfc {
                return true;
            }
            // Link-local fe80::/10 — high 10 bits are 1111111010.
            if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                return true;
            }
            // IPv4-mapped (::ffff:a.b.c.d) — re-check via the v4 ruleset so a
            // mapped private/loopback address is caught too.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_ip(IpAddr::V4(v4));
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Build a `ServerConfig` whose `allowed_hosts` is exactly `hosts`.
    fn cfg_with_hosts(hosts: &[&str]) -> ServerConfig {
        let mut c = ServerConfig::stdio("python".into(), vec![]);
        c.allowed_hosts = hosts.iter().map(|s| s.to_string()).collect();
        // Touch a field so an accidental BTreeMap import removal still
        // compiles meaningfully; keeps env explicitly empty.
        c.env = BTreeMap::new();
        c
    }

    fn guard(hosts: &[&str]) -> HostGuard {
        HostGuard::from_config(&cfg_with_hosts(hosts))
    }

    fn url(s: &str) -> url::Url {
        url::Url::parse(s).expect("test url must parse")
    }

    fn is_blocked(guard: &HostGuard, u: &str) -> bool {
        match guard.check_url(&url(u)) {
            Ok(()) => false,
            Err(TransportError::Other(m)) => {
                assert!(
                    m.contains("blocked host"),
                    "rejection message must contain the stable `blocked host` \
                     substring, got: {m}"
                );
                true
            }
            Err(other) => panic!("expected TransportError::Other, got {other:?}"),
        }
    }

    #[test]
    fn blocks_ipv4_loopback() {
        assert!(is_blocked(&guard(&[]), "http://127.0.0.1:9/"));
    }

    #[test]
    fn blocks_ipv4_private_ranges() {
        let g = guard(&[]);
        assert!(is_blocked(&g, "http://10.0.0.1/"));
        assert!(is_blocked(&g, "http://172.16.5.4/"));
        assert!(is_blocked(&g, "http://192.168.1.1/"));
    }

    #[test]
    fn blocks_ipv4_link_local_metadata_endpoint() {
        // The classic cloud metadata SSRF target.
        assert!(is_blocked(
            &guard(&[]),
            "http://169.254.169.254/latest/meta-data/"
        ));
    }

    #[test]
    fn blocks_ipv4_unspecified() {
        assert!(is_blocked(&guard(&[]), "http://0.0.0.0/"));
    }

    #[test]
    fn blocks_ipv6_loopback_ula_and_link_local() {
        let g = guard(&[]);
        assert!(is_blocked(&g, "http://[::1]/"));
        assert!(is_blocked(&g, "http://[fd00::1]/"));
        assert!(is_blocked(&g, "http://[fe80::1]/"));
    }

    #[test]
    fn blocks_ipv4_mapped_private_v6() {
        assert!(is_blocked(&guard(&[]), "http://[::ffff:10.0.0.1]/"));
    }

    #[test]
    fn allows_public_ipv4_when_no_allowlist() {
        assert!(!is_blocked(&guard(&[]), "http://8.8.8.8/"));
    }

    #[test]
    fn empty_allowlist_allows_arbitrary_public_host() {
        assert!(!is_blocked(&guard(&[]), "https://api.example.com/v1"));
    }

    #[test]
    fn allowlist_is_exact_not_suffix() {
        let g = guard(&["api.example.com"]);
        assert!(!is_blocked(&g, "https://api.example.com/v1"));
        // Suffix-confusion attack must be rejected.
        assert!(is_blocked(&g, "https://api.example.com.attacker.com/v1"));
        // A different public host not on the list is rejected too.
        assert!(is_blocked(&g, "https://other.example.org/"));
    }

    #[test]
    fn allowlist_match_is_case_insensitive() {
        let g = guard(&["API.Example.COM"]);
        assert!(!is_blocked(&g, "https://api.example.com/"));
    }

    #[test]
    fn escape_hatch_allows_listed_loopback_literal() {
        // Operator opted in to local testing against 127.0.0.1.
        let g = guard(&["127.0.0.1"]);
        assert!(!is_blocked(&g, "http://127.0.0.1:9/"));
        // But a *different* private literal is still blocked.
        assert!(is_blocked(&g, "http://10.0.0.1/"));
    }

    #[test]
    fn url_without_host_is_blocked() {
        // `data:` / opaque URLs have no host_str().
        assert!(is_blocked(&guard(&[]), "data:text/plain,hello"));
    }

    #[tokio::test]
    async fn hostname_resolving_to_private_ip_blocked_at_resolve_layer() {
        // ADR 0012 accepted a DNS-rebinding gap: a hostname that resolves to
        // a private IP passes the literal layer. ADR 0016 resolver pinning
        // rejects it at the resolve layer. `check_url` alone still passes
        // hostnames — the literal layer never resolves — so the layering
        // itself is asserted here too.
        use std::net::{Ipv4Addr, SocketAddr};

        use crate::transport::resolve::resolve_and_check_with;

        let g = guard(&[]);
        let u = url("http://localtest.me/");
        assert!(
            g.check_url(&u).is_ok(),
            "literal layer alone must still pass hostnames (resolution is ADR 0016's job)"
        );

        // `localtest.me` resolves to 127.0.0.1 in the real world; the seam
        // resolver reproduces that without touching DNS.
        let resolver = |_h: String, port: u16| {
            std::future::ready(Ok(vec![SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                port,
            )]))
        };
        let err = resolve_and_check_with(&u, &g, resolver)
            .await
            .expect_err("hostname resolving to loopback must now be blocked");
        let TransportError::Other(m) = err else {
            panic!("expected TransportError::Other, got {err:?}");
        };
        assert!(m.contains("blocked host"), "stable substring missing: {m}");
        assert!(m.contains("ADR 0016"), "ADR 0016 marker missing: {m}");
    }
}
