//! Resolver pinning for the outbound network transports (ADR 0016).
//!
//! Closes the DNS-rebinding gap ADR 0012 accepted: the literal layer
//! ([`HostGuard::check_url`]) never sees the *resolved* address of a hostname
//! URL, so `localtest.me` → `127.0.0.1` (or attacker-controlled DNS that flips
//! between check and connect) sailed through. Here we resolve **once**, vet
//! every resolved address against the same blocklist the literal layer uses,
//! and hand the vetted addresses back so the transport dials exactly the
//! address that was checked: resolve → reject-private → pin → connect, with
//! no second lookup for a rebinding server to exploit.
//!
//! Escape-hatch semantics mirror ADR 0012: a hostname listed in
//! `[server].allowed_hosts` is explicitly trusted by the operator, so its
//! resolution may be private (e.g. `allowed_hosts = ["localhost"]` for local
//! testing) — but the result is still pinned, so even a trusted name cannot
//! rebind mid-session.

use std::future::Future;
use std::io;
use std::net::SocketAddr;

use tokio::net::TcpStream;
use url::Url;

use super::TransportError;
use super::guard::{HostGuard, is_blocked_ip};

/// Resolve `url`'s host, vet every resolved address, and return the vetted
/// addresses for pinning. Layering:
///
/// 1. [`HostGuard::check_url`] runs first — IP-literal block + allowlist,
///    unchanged ADR 0012 semantics.
/// 2. IP-literal URLs short-circuit: the literal (already vetted or
///    escape-hatched by layer 1) *is* the pin; no DNS is involved.
/// 3. Hostname URLs are resolved via `tokio::net::lookup_host` (non-blocking).
///    Unless the hostname is allowlisted, **any** resolved address in
///    private/loopback/link-local/ULA/reserved space rejects the whole URL.
pub(crate) async fn resolve_and_check(
    url: &Url,
    guard: &HostGuard,
) -> Result<Vec<SocketAddr>, TransportError> {
    resolve_and_check_with(url, guard, |host: String, port: u16| async move {
        Ok(tokio::net::lookup_host((host.as_str(), port))
            .await?
            .collect())
    })
    .await
}

/// Seam-injectable core of [`resolve_and_check`]. `resolve` is only invoked
/// for *hostname* URLs (never for IP literals, never for hosts the allowlist
/// already rejected), which is what lets unit tests drive the rebinding logic
/// without real DNS.
pub(crate) async fn resolve_and_check_with<F, Fut>(
    url: &Url,
    guard: &HostGuard,
    resolve: F,
) -> Result<Vec<SocketAddr>, TransportError>
where
    F: FnOnce(String, u16) -> Fut,
    Fut: Future<Output = io::Result<Vec<SocketAddr>>>,
{
    // Layer 1 (ADR 0012): literal block + allowlist. Must stay first so the
    // allowlist rejects un-listed hostnames *before* any lookup happens.
    guard.check_url(url)?;

    let port = url.port_or_known_default().ok_or_else(|| {
        TransportError::Other(format!(
            "cannot determine port for `{url}` (resolver pinning, ADR 0016)"
        ))
    })?;
    // `check_url` already rejected host-less URLs; this is defensive.
    let host = url.host().ok_or_else(|| {
        TransportError::Other("blocked host: URL has no host (SSRF guard, ADR 0012)".into())
    })?;

    let domain = match host {
        url::Host::Ipv4(v4) => return Ok(vec![SocketAddr::new(v4.into(), port)]),
        url::Host::Ipv6(v6) => return Ok(vec![SocketAddr::new(v6.into(), port)]),
        url::Host::Domain(d) => d.to_string(),
    };

    let addrs = resolve(domain.clone(), port).await.map_err(|e| {
        TransportError::Other(format!(
            "dns lookup for `{domain}` failed: {e} (resolver pinning, ADR 0016)"
        ))
    })?;
    if addrs.is_empty() {
        return Err(TransportError::Other(format!(
            "dns lookup for `{domain}` returned no addresses (resolver pinning, ADR 0016)"
        )));
    }

    // Allowlisted hostname = operator escape hatch (explicit trust), same
    // semantics ADR 0012 gives listed IP literals. Still pinned below.
    if !guard.host_allowed(&domain)
        && let Some(bad) = addrs.iter().find(|sa| is_blocked_ip(sa.ip()))
    {
        return Err(TransportError::Other(format!(
            "blocked host `{domain}`: resolves to private/loopback/link-local/reserved \
             address {ip} and is not in allowed_hosts (DNS-rebinding guard, ADR 0016)",
            ip = bad.ip()
        )));
    }
    Ok(addrs)
}

/// Build the `reqwest::Client` the http / sse transports share: redirects off
/// (ADR 0007 / 0012) and — for hostname URLs — DNS pinned to `addrs`
/// (ADR 0016), so the address we vetted is the address reqwest dials.
///
/// IP-literal URLs need no pin: reqwest dials the literal directly without
/// consulting the resolver.
pub(crate) fn pinned_client(
    url: &Url,
    addrs: &[SocketAddr],
) -> Result<reqwest::Client, TransportError> {
    // reason: follow-redirect is a SSRF foothold when the target URL is
    // operator-supplied; force users to point at the final endpoint directly.
    // Without this, an attacker can redirect us into 169.254.169.254 (cloud
    // metadata) or localhost-bound admin endpoints.
    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if let Some(url::Host::Domain(d)) = url.host() {
        // reqwest ignores the port inside pinned SocketAddrs and uses the
        // URL's port — exactly the override we want.
        builder = builder.resolve_to_addrs(d, addrs);
    }
    builder
        .build()
        .map_err(|e| TransportError::Http(format!("client build: {e}")))
}

/// Dial the first reachable vetted address, in resolver order. The caller
/// (ws transport) completes the WebSocket / TLS handshake with the original
/// *hostname* URL, so TLS SNI and the `Host` header stay correct while the
/// socket goes only where [`resolve_and_check`] approved.
pub(crate) async fn dial_pinned(
    host: &str,
    addrs: &[SocketAddr],
) -> Result<TcpStream, TransportError> {
    let mut last_err: Option<io::Error> = None;
    for addr in addrs {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }
    Err(TransportError::Other(format!(
        "connect to `{host}` failed on all {n} pinned address(es): {err} (resolver pinning, \
         ADR 0016)",
        n = addrs.len(),
        err = last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "no addresses to dial".into()),
    )))
}

#[cfg(test)]
mod tests {
    use std::future::{Ready, ready};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::*;
    use mcp_loadtest_core::config::ServerConfig;

    fn guard(hosts: &[&str]) -> HostGuard {
        let mut cfg = ServerConfig::stdio("python".into(), vec![]);
        cfg.allowed_hosts = hosts.iter().map(|s| s.to_string()).collect();
        HostGuard::from_config(&cfg)
    }

    fn url(s: &str) -> Url {
        Url::parse(s).expect("test url must parse")
    }

    /// A resolver that returns `ips` (each paired with the requested port).
    fn fixed(ips: Vec<IpAddr>) -> impl FnOnce(String, u16) -> Ready<io::Result<Vec<SocketAddr>>> {
        move |_host, port| {
            ready(Ok(ips
                .into_iter()
                .map(|ip| SocketAddr::new(ip, port))
                .collect()))
        }
    }

    /// A resolver that must never run — used to prove a path short-circuits
    /// before any DNS happens.
    fn must_not_resolve(_host: String, _port: u16) -> Ready<io::Result<Vec<SocketAddr>>> {
        panic!("resolver must not be called on this path")
    }

    /// Assert `r` is a rejection carrying the stable substrings.
    fn assert_blocked(r: Result<Vec<SocketAddr>, TransportError>, marker: &str) -> String {
        match r {
            Err(TransportError::Other(m)) => {
                assert!(
                    m.contains("blocked host"),
                    "rejection must carry the stable `blocked host` substring, got: {m}"
                );
                assert!(m.contains(marker), "expected `{marker}` marker, got: {m}");
                m
            }
            other => panic!("expected TransportError::Other rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hostname_resolving_to_loopback_is_blocked() {
        // Closes ADR 0012's accepted gap: `localtest.me` → 127.0.0.1 is
        // rejected here at the resolve layer.
        let r = resolve_and_check_with(
            &url("http://localtest.me/"),
            &guard(&[]),
            fixed(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        )
        .await;
        assert_blocked(r, "ADR 0016");
    }

    #[tokio::test]
    async fn hostname_resolving_to_private_ip_is_blocked() {
        let r = resolve_and_check_with(
            &url("http://internal.corp/"),
            &guard(&[]),
            fixed(vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))]),
        )
        .await;
        let msg = assert_blocked(r, "ADR 0016");
        assert!(msg.contains("10.0.0.1"), "should name the bad IP: {msg}");
    }

    #[tokio::test]
    async fn public_resolution_passes_and_pins_scheme_default_port() {
        let ip = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
        let addrs = resolve_and_check_with(
            &url("https://api.example.com/v1"),
            &guard(&[]),
            fixed(vec![ip]),
        )
        .await
        .expect("public resolution must pass");
        assert_eq!(addrs, vec![SocketAddr::new(ip, 443)]);
    }

    #[tokio::test]
    async fn wss_scheme_pins_port_443() {
        let ip = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
        let addrs =
            resolve_and_check_with(&url("wss://api.example.com/"), &guard(&[]), fixed(vec![ip]))
                .await
                .expect("public wss resolution must pass");
        assert_eq!(addrs, vec![SocketAddr::new(ip, 443)]);
    }

    #[tokio::test]
    async fn mixed_records_with_one_private_reject_whole_host() {
        // Classic rebinding setup: one legit-looking public A record plus one
        // private record. ANY private record poisons the host.
        let r = resolve_and_check_with(
            &url("http://half-evil.example.com/"),
            &guard(&[]),
            fixed(vec![
                IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            ]),
        )
        .await;
        let msg = assert_blocked(r, "ADR 0016");
        assert!(msg.contains("192.168.1.1"), "should name the bad IP: {msg}");
    }

    #[tokio::test]
    async fn ipv6_private_resolutions_are_blocked() {
        for ip in [
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            // ULA fc00::/7
            IpAddr::V6("fd00::1".parse().expect("ipv6")),
            // link-local fe80::/10
            IpAddr::V6("fe80::1".parse().expect("ipv6")),
            // IPv4-mapped private
            IpAddr::V6("::ffff:10.0.0.1".parse().expect("ipv6")),
        ] {
            let r = resolve_and_check_with(
                &url("http://v6.example.com/"),
                &guard(&[]),
                fixed(vec![ip]),
            )
            .await;
            assert_blocked(r, "ADR 0016");
        }
    }

    #[tokio::test]
    async fn allowlisted_hostname_escape_hatch_permits_private_resolution() {
        // Operator explicitly trusts the name (same escape-hatch semantics
        // ADR 0012 gives IP literals) — private resolution allowed, but the
        // result is still pinned.
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let addrs = resolve_and_check_with(
            &url("http://localhost:8080/"),
            &guard(&["localhost"]),
            fixed(vec![ip]),
        )
        .await
        .expect("allowlisted hostname must pass even when resolving private");
        assert_eq!(addrs, vec![SocketAddr::new(ip, 8080)]);
    }

    #[tokio::test]
    async fn public_ip_literal_passes_through_without_resolution() {
        let addrs =
            resolve_and_check_with(&url("http://8.8.8.8:9000/"), &guard(&[]), must_not_resolve)
                .await
                .expect("public IP literal must pass");
        assert_eq!(
            addrs,
            vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 9000)]
        );
    }

    #[tokio::test]
    async fn blocked_ip_literal_still_rejected_by_literal_layer() {
        let r = resolve_and_check_with(
            &url("http://169.254.169.254/latest/meta-data/"),
            &guard(&[]),
            must_not_resolve,
        )
        .await;
        // Literal layer rejection — carries the ADR 0012 marker.
        assert_blocked(r, "ADR 0012");
    }

    #[tokio::test]
    async fn unlisted_hostname_rejected_before_any_lookup() {
        // With a non-empty allowlist, the literal layer rejects un-listed
        // hostnames first — the resolver must not even run.
        let r = resolve_and_check_with(
            &url("https://b.example.com/"),
            &guard(&["a.example.com"]),
            must_not_resolve,
        )
        .await;
        assert_blocked(r, "ADR 0012");
    }

    #[tokio::test]
    async fn dns_failure_surfaces_as_error_not_pass() {
        let failing =
            |_h: String, _p: u16| ready(Err::<Vec<SocketAddr>, _>(io::Error::other("nxdomain")));
        let err = resolve_and_check_with(&url("http://gone.example.com/"), &guard(&[]), failing)
            .await
            .expect_err("dns failure must be an error (fail closed)");
        let TransportError::Other(m) = err else {
            panic!("expected Other");
        };
        assert!(m.contains("dns lookup"), "got: {m}");
        assert!(m.contains("ADR 0016"), "got: {m}");
    }

    #[tokio::test]
    async fn empty_resolution_is_an_error() {
        let err = resolve_and_check_with(
            &url("http://empty.example.com/"),
            &guard(&[]),
            fixed(vec![]),
        )
        .await
        .expect_err("empty resolution must be an error (fail closed)");
        let TransportError::Other(m) = err else {
            panic!("expected Other");
        };
        assert!(m.contains("no addresses"), "got: {m}");
    }

    #[tokio::test]
    async fn dial_pinned_falls_back_across_addrs() {
        // Bind a real listener, plus a port we just freed (very likely
        // refused) in front of it — dial must fall through to the live one.
        let live = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let live_addr = live.local_addr().expect("addr");
        let dead_addr = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            l.local_addr().expect("addr")
            // listener dropped here — port closed
        };
        let stream = dial_pinned("localhost", &[dead_addr, live_addr])
            .await
            .expect("must fall back to the live address");
        assert_eq!(stream.peer_addr().expect("peer"), live_addr);
    }

    #[tokio::test]
    async fn dial_pinned_all_unreachable_reports_adr_0016() {
        let dead_addr = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            l.local_addr().expect("addr")
        };
        let err = dial_pinned("example.com", &[dead_addr])
            .await
            .expect_err("dead address must fail");
        let TransportError::Other(m) = err else {
            panic!("expected Other");
        };
        assert!(m.contains("ADR 0016"), "got: {m}");
    }
}
