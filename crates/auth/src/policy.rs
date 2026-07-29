//! Security policy for outbound OAuth endpoints.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use url::{Host, Url};

use crate::{AuthError, AuthResult};

/// HTTPS and response-size policy for OAuth network operations.
#[derive(Debug, Clone)]
pub struct EndpointPolicy {
    allow_loopback_http: bool,
    timeout: Duration,
    maximum_response_bytes: usize,
}

impl EndpointPolicy {
    /// Build the production policy: HTTPS only, public DNS answers pinned per
    /// request, redirects and system proxies disabled, a 15-second request
    /// timeout, and one MiB response limit.
    pub fn strict() -> Self {
        Self {
            allow_loopback_http: false,
            timeout: Duration::from_secs(15),
            maximum_response_bytes: 1024 * 1024,
        }
    }

    /// Build an explicit test policy that also permits plain HTTP only for
    /// `localhost`, `127.0.0.1`, and `::1`.
    ///
    /// This policy must not be used for production authorization servers.
    pub fn loopback_for_tests() -> Self {
        Self {
            allow_loopback_http: true,
            ..Self::strict()
        }
    }

    /// Set the per-request network timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the maximum accepted response body size.
    #[must_use]
    pub fn with_maximum_response_bytes(mut self, bytes: usize) -> Self {
        self.maximum_response_bytes = bytes;
        self
    }

    pub(crate) fn validate(&self, url: &Url) -> AuthResult<()> {
        if url.cannot_be_a_base() || url.host().is_none() {
            return Err(AuthError::UnsafeEndpoint {
                reason: "URL must be an absolute hierarchical URL",
            });
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(AuthError::UnsafeEndpoint {
                reason: "URL userinfo is forbidden",
            });
        }
        if url.fragment().is_some() {
            return Err(AuthError::UnsafeEndpoint {
                reason: "URL fragments are forbidden",
            });
        }
        if is_unsafe_host(url.host()) && !(self.allow_loopback_http && is_loopback_host(url.host()))
        {
            return Err(AuthError::UnsafeEndpoint {
                reason: "local, private, and reserved IP endpoints are forbidden",
            });
        }
        match url.scheme() {
            "https" => Ok(()),
            "http" if self.allow_loopback_http && is_loopback_host(url.host()) => Ok(()),
            _ => Err(AuthError::UnsafeEndpoint {
                reason: "HTTPS is required",
            }),
        }
    }

    /// Build a request-scoped client whose DNS answers have been validated and
    /// pinned. Re-resolving for every request avoids stale trust decisions,
    /// while the reqwest override closes the lookup-to-connect rebinding gap.
    pub(crate) async fn client_for(&self, url: &Url) -> AuthResult<reqwest::Client> {
        self.validate(url)?;
        if let Some(Host::Domain(domain)) = url.host() {
            let port = url
                .port_or_known_default()
                .ok_or(AuthError::InvalidUrl { kind: "endpoint" })?;
            let mut addresses = tokio::net::lookup_host((domain, port))
                .await
                .map_err(|_| AuthError::Network {
                    operation: "endpoint DNS resolution",
                })?
                .collect::<Vec<_>>();
            addresses.sort_unstable();
            addresses.dedup();
            return self.pinned_client(url, domain, &addresses);
        }

        self.build_client(None)
    }

    fn pinned_client(
        &self,
        url: &Url,
        domain: &str,
        addresses: &[SocketAddr],
    ) -> AuthResult<reqwest::Client> {
        self.validate(url)?;
        if url.host_str() != Some(domain) {
            return Err(AuthError::InvalidUrl { kind: "endpoint" });
        }
        if addresses.is_empty() {
            return Err(AuthError::Network {
                operation: "endpoint DNS resolution",
            });
        }
        if addresses
            .iter()
            .any(|address| !self.address_allowed(domain, address.ip()))
        {
            return Err(AuthError::UnsafeEndpoint {
                reason: "DNS resolved to a local, private, or reserved address",
            });
        }
        self.build_client(Some((domain, addresses)))
    }

    fn build_client(&self, pinned: Option<(&str, &[SocketAddr])>) -> AuthResult<reqwest::Client> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(self.timeout)
            // A system proxy could resolve the hostname again and bypass the
            // vetted address set, so OAuth endpoint traffic is always direct.
            .no_proxy();
        if let Some((domain, addresses)) = pinned {
            builder = builder.resolve_to_addrs(domain, addresses);
        }
        builder.build().map_err(|_| AuthError::Network {
            operation: "HTTP client setup",
        })
    }

    pub(crate) async fn validate_resolved(&self, url: &Url) -> AuthResult<()> {
        let _ = self.client_for(url).await?;
        Ok(())
    }

    fn address_allowed(&self, domain: &str, address: IpAddr) -> bool {
        if self.allow_loopback_http && domain.eq_ignore_ascii_case("localhost") {
            address.is_loopback()
        } else {
            is_public_ip(address)
        }
    }

    pub(crate) fn maximum_response_bytes(&self) -> usize {
        self.maximum_response_bytes
    }
}

impl Default for EndpointPolicy {
    fn default() -> Self {
        Self::strict()
    }
}

fn is_loopback_host(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

fn is_unsafe_host(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => !is_public_ipv4(address),
        Some(Host::Ipv6(address)) => !is_public_ipv6(address),
        None => true,
    }
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, d] = address.octets();
    let _ = (c, d);
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    !(address.is_unspecified()
        || address.is_loopback()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] & 0xff00) == 0xff00
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || is_non_public_nat64(address))
}

fn is_non_public_nat64(address: Ipv6Addr) -> bool {
    let octets = address.octets();
    let well_known_prefix = octets[..12] == [0x00, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0];
    let local_prefix = octets[..6] == [0x00, 0x64, 0xff, 0x9b, 0x00, 0x01];
    if !well_known_prefix && !local_prefix {
        return false;
    }
    !is_public_ipv4(Ipv4Addr::new(
        octets[12], octets[13], octets[14], octets[15],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_rejects_plaintext_and_userinfo() {
        let policy = EndpointPolicy::strict();
        assert!(
            policy
                .validate(&Url::parse("http://example.com").expect("url"))
                .is_err()
        );
        assert!(
            policy
                .validate(&Url::parse("https://127.0.0.1/token").expect("url"))
                .is_err()
        );
        assert!(
            policy
                .validate(&Url::parse("https://user:pass@example.com").expect("url"))
                .is_err()
        );
    }

    #[test]
    fn test_policy_only_allows_http_loopback() {
        let policy = EndpointPolicy::loopback_for_tests();
        assert!(
            policy
                .validate(&Url::parse("http://127.0.0.1:1234").expect("url"))
                .is_ok()
        );
        assert!(
            policy
                .validate(&Url::parse("http://example.com").expect("url"))
                .is_err()
        );
        assert!(
            policy
                .validate(&Url::parse("https://192.168.1.10").expect("url"))
                .is_err()
        );
    }

    #[test]
    fn reserved_and_embedded_private_addresses_are_not_public() {
        for address in [
            "0.0.0.1",
            "100.64.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "203.0.113.1",
            "224.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:192.168.1.1",
            "64:ff9b::192.168.1.1",
        ] {
            assert!(
                !is_public_ip(address.parse().expect("IP address")),
                "{address} must not be treated as public"
            );
        }
        assert!(is_public_ip("8.8.8.8".parse().expect("IP address")));
        assert!(is_public_ip(
            "2606:4700:4700::1111".parse().expect("IP address")
        ));
    }

    #[tokio::test]
    async fn test_policy_pins_loopback_only_in_explicit_test_mode() {
        let url = Url::parse("http://localhost:9/token").expect("url");
        assert!(
            EndpointPolicy::loopback_for_tests()
                .client_for(&url)
                .await
                .is_ok()
        );
        assert!(EndpointPolicy::strict().client_for(&url).await.is_err());
    }

    #[test]
    fn strict_pin_rejects_private_and_mixed_dns_answers() {
        let policy = EndpointPolicy::strict();
        let url = Url::parse("https://public.example/token").expect("url");
        let private = "127.0.0.1:443".parse().expect("socket address");
        let public = "93.184.216.34:443".parse().expect("socket address");
        assert!(
            policy
                .pinned_client(&url, "public.example", &[private])
                .is_err()
        );
        assert!(
            policy
                .pinned_client(&url, "public.example", &[public, private])
                .is_err()
        );
        assert!(
            policy
                .pinned_client(&url, "public.example", &[public])
                .is_ok()
        );
    }
}
