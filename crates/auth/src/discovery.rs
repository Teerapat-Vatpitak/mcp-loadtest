//! RFC 9728 protected-resource and RFC 8414/OIDC issuer discovery.

use serde::Deserialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::{AuthError, AuthResult, BearerChallenge, EndpointPolicy, ScopeSet};

/// RFC 9728 OAuth protected-resource metadata used by MCP clients.
#[derive(Debug, Clone, Deserialize)]
pub struct ProtectedResourceMetadata {
    /// Protected resource identifier advertised by the metadata document.
    pub resource: String,
    /// Ordered authorization-server issuer identifiers.
    #[serde(default)]
    pub authorization_servers: Vec<String>,
    /// Scopes understood by the protected resource.
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

/// OAuth authorization-server metadata required by MCP clients.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizationServerMetadata {
    /// Exact issuer identifier as serialized by the server.
    pub issuer: String,
    /// Interactive authorization endpoint.
    pub authorization_endpoint: Option<Url>,
    /// Token endpoint.
    pub token_endpoint: Option<Url>,
    /// Optional dynamic client-registration endpoint.
    pub registration_endpoint: Option<Url>,
    /// Advertised PKCE challenge methods.
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,
    /// Supported token endpoint client authentication methods.
    #[serde(default)]
    pub token_endpoint_auth_methods_supported: Vec<String>,
    /// Supported OAuth scopes.
    #[serde(default)]
    pub scopes_supported: Vec<String>,
    /// Supported OAuth grant types.
    #[serde(default)]
    pub grant_types_supported: Vec<String>,
    /// Whether RFC 9207 `iss` is promised in authorization responses.
    #[serde(default)]
    pub authorization_response_iss_parameter_supported: bool,
}

impl AuthorizationServerMetadata {
    /// Return the required authorization endpoint.
    pub fn authorization_endpoint(&self) -> AuthResult<&Url> {
        self.authorization_endpoint
            .as_ref()
            .ok_or(AuthError::MissingMetadata {
                field: "authorization_endpoint",
            })
    }

    /// Return the required token endpoint.
    pub fn token_endpoint(&self) -> AuthResult<&Url> {
        self.token_endpoint
            .as_ref()
            .ok_or(AuthError::MissingMetadata {
                field: "token_endpoint",
            })
    }

    /// Return whether the server explicitly advertises a scope.
    pub fn supports_scope(&self, scope: &str) -> bool {
        self.scopes_supported.iter().any(|value| value == scope)
    }
}

/// Fully discovered authorization context for one exact MCP resource.
#[derive(Debug, Clone)]
pub struct AuthorizationContext {
    /// Canonical resource URI included in authorization and token requests.
    pub resource: Url,
    /// Protected-resource metadata.
    pub protected_resource: ProtectedResourceMetadata,
    /// Selected authorization-server metadata.
    pub authorization_server: AuthorizationServerMetadata,
}

impl AuthorizationContext {
    /// Select initial scopes according to MCP precedence: challenge scope,
    /// then protected-resource metadata, otherwise omit the scope parameter.
    /// `offline_access` is added only when the authorization server advertises it.
    pub fn initial_scopes(
        &self,
        challenge: Option<&BearerChallenge>,
        request_offline_access: bool,
    ) -> ScopeSet {
        let mut scopes = challenge
            .filter(|value| !value.scopes.is_empty())
            .map(|value| value.scopes.clone())
            .unwrap_or_else(|| {
                ScopeSet::from_tokens(self.protected_resource.scopes_supported.clone())
            });
        if request_offline_access && self.authorization_server.supports_scope("offline_access") {
            scopes.insert("offline_access");
        }
        scopes
    }

    /// Build the token-store binding key components.
    pub fn exact_issuer(&self) -> &str {
        &self.authorization_server.issuer
    }
}

/// Network client for MCP OAuth metadata discovery.
#[derive(Debug, Clone)]
pub struct DiscoveryClient {
    policy: EndpointPolicy,
}

impl DiscoveryClient {
    /// Create a discovery client using an explicit endpoint policy.
    pub fn new(policy: EndpointPolicy) -> AuthResult<Self> {
        Ok(Self { policy })
    }

    /// Discover protected-resource and authorization-server metadata.
    ///
    /// A challenge-provided metadata URL takes precedence. Without it, the
    /// path-specific RFC 9728 location is tried before the origin-wide location.
    pub async fn discover(
        &self,
        mut resource: Url,
        challenge: Option<&BearerChallenge>,
    ) -> AuthResult<AuthorizationContext> {
        // RFC 8707 resource identifiers cannot contain fragments. Keep the
        // requested path and query intact while discarding only the fragment.
        resource.set_fragment(None);
        self.policy.validate(&resource)?;
        let protected_resource = self
            .discover_protected_resource(&resource, challenge)
            .await?;
        let advertised_resource = Url::parse(&protected_resource.resource)
            .map_err(|_| AuthError::InvalidUrl { kind: "resource" })?;
        self.policy.validate(&advertised_resource)?;
        if !resource_is_allowed(&resource, &advertised_resource) {
            return Err(AuthError::ResourceMismatch);
        }
        let issuer = protected_resource
            .authorization_servers
            .first()
            .ok_or(AuthError::MissingAuthorizationServer)?;
        let issuer_url =
            Url::parse(issuer).map_err(|_| AuthError::InvalidUrl { kind: "issuer" })?;
        self.policy.validate(&issuer_url)?;
        let authorization_server = self
            .discover_authorization_server(&issuer_url, issuer)
            .await?;
        self.validate_metadata_endpoints(&authorization_server)
            .await?;
        Ok(AuthorizationContext {
            // A root PRM document can intentionally advertise an origin-wide
            // resource. Prefer that identifier in authorization/token requests
            // after verifying that it contains the requested MCP endpoint.
            resource: advertised_resource,
            protected_resource,
            authorization_server,
        })
    }

    async fn discover_protected_resource(
        &self,
        resource: &Url,
        challenge: Option<&BearerChallenge>,
    ) -> AuthResult<ProtectedResourceMetadata> {
        if let Some(metadata_url) = challenge.and_then(|value| value.resource_metadata.as_ref()) {
            self.policy.validate(metadata_url)?;
            return self
                .fetch_json(metadata_url, "protected-resource discovery")
                .await;
        }

        let candidates = protected_resource_candidates(resource)?;
        for (index, candidate) in candidates.iter().enumerate() {
            self.policy.validate(candidate)?;
            match self
                .fetch_json_optional::<ProtectedResourceMetadata>(
                    candidate,
                    "protected-resource discovery",
                )
                .await?
            {
                Some(metadata) => return Ok(metadata),
                None if index + 1 < candidates.len() => {}
                None => {
                    return Err(AuthError::HttpStatus {
                        operation: "protected-resource discovery",
                        status: 404,
                    });
                }
            }
        }
        Err(AuthError::MissingAuthorizationServer)
    }

    async fn discover_authorization_server(
        &self,
        issuer: &Url,
        exact_issuer: &str,
    ) -> AuthResult<AuthorizationServerMetadata> {
        for candidate in authorization_server_candidates(issuer)? {
            self.policy.validate(&candidate)?;
            let Some(metadata) = self
                .fetch_json_optional::<AuthorizationServerMetadata>(
                    &candidate,
                    "authorization-server discovery",
                )
                .await?
            else {
                continue;
            };
            if metadata.issuer != exact_issuer {
                return Err(AuthError::IssuerMismatch);
            }
            return Ok(metadata);
        }
        Err(AuthError::HttpStatus {
            operation: "authorization-server discovery",
            status: 404,
        })
    }

    async fn validate_metadata_endpoints(
        &self,
        metadata: &AuthorizationServerMetadata,
    ) -> AuthResult<()> {
        if let Some(endpoint) = metadata.authorization_endpoint.as_ref() {
            self.policy.validate_resolved(endpoint).await?;
        }
        if let Some(endpoint) = metadata.token_endpoint.as_ref() {
            self.policy.validate_resolved(endpoint).await?;
        }
        if let Some(endpoint) = metadata.registration_endpoint.as_ref() {
            self.policy.validate_resolved(endpoint).await?;
        }
        Ok(())
    }

    pub(crate) async fn fetch_json<T: DeserializeOwned>(
        &self,
        endpoint: &Url,
        operation: &'static str,
    ) -> AuthResult<T> {
        self.fetch_json_optional(endpoint, operation)
            .await?
            .ok_or(AuthError::HttpStatus {
                operation,
                status: 404,
            })
    }

    async fn fetch_json_optional<T: DeserializeOwned>(
        &self,
        endpoint: &Url,
        operation: &'static str,
    ) -> AuthResult<Option<T>> {
        let http = self.policy.client_for(endpoint).await?;
        let response = http
            .get(endpoint.clone())
            .send()
            .await
            .map_err(|_| AuthError::Network { operation })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(AuthError::HttpStatus {
                operation,
                status: response.status().as_u16(),
            });
        }
        let body = bounded_body(response, self.policy.maximum_response_bytes(), operation).await?;
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|_| AuthError::InvalidJson { operation })
    }
}

pub(crate) async fn bounded_body(
    mut response: reqwest::Response,
    maximum_bytes: usize,
    operation: &'static str,
) -> AuthResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum_bytes as u64)
    {
        return Err(AuthError::ResponseTooLarge { operation });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| AuthError::Network { operation })?
    {
        if body.len().saturating_add(chunk.len()) > maximum_bytes {
            return Err(AuthError::ResponseTooLarge { operation });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn resource_is_allowed(requested: &Url, advertised: &Url) -> bool {
    if requested.origin() != advertised.origin() {
        return false;
    }

    let requested_path = requested.path().trim_end_matches('/');
    let advertised_path = advertised.path().trim_end_matches('/');
    advertised_path.is_empty()
        || requested_path == advertised_path
        || requested_path
            .strip_prefix(advertised_path)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn protected_resource_candidates(resource: &Url) -> AuthResult<Vec<Url>> {
    let mut root = resource.clone();
    root.set_query(None);
    root.set_fragment(None);
    root.set_path("/.well-known/oauth-protected-resource");

    let path = resource.path();
    if path.is_empty() || path == "/" {
        return Ok(vec![root]);
    }
    let mut path_specific = root.clone();
    path_specific.set_path(&format!(
        "/.well-known/oauth-protected-resource/{}",
        path.trim_start_matches('/')
    ));
    Ok(vec![path_specific, root])
}

fn authorization_server_candidates(issuer: &Url) -> AuthResult<Vec<Url>> {
    if issuer.query().is_some() || issuer.fragment().is_some() {
        return Err(AuthError::InvalidUrl { kind: "issuer" });
    }
    let issuer_path = issuer.path();
    let path_suffix = issuer_path.trim_start_matches('/');
    let mut candidates = Vec::new();

    let mut oauth = issuer.clone();
    oauth.set_path(
        if path_suffix.is_empty() {
            "/.well-known/oauth-authorization-server".to_owned()
        } else {
            format!("/.well-known/oauth-authorization-server/{path_suffix}")
        }
        .as_str(),
    );
    candidates.push(oauth);

    let mut oidc_prefix = issuer.clone();
    oidc_prefix.set_path(
        if path_suffix.is_empty() {
            "/.well-known/openid-configuration".to_owned()
        } else {
            format!("/.well-known/openid-configuration/{path_suffix}")
        }
        .as_str(),
    );
    candidates.push(oidc_prefix);

    if !path_suffix.is_empty() {
        let mut oidc_suffix = issuer.clone();
        let base = issuer_path.trim_end_matches('/');
        oidc_suffix.set_path(&format!("{base}/.well-known/openid-configuration"));
        candidates.push(oidc_suffix);
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_resource_path_order_matches_rfc_9728() {
        let resource = Url::parse("https://mcp.example/a/b?ignored=yes").expect("url");
        let candidates = protected_resource_candidates(&resource).expect("candidates");
        assert_eq!(
            candidates.iter().map(Url::as_str).collect::<Vec<_>>(),
            vec![
                "https://mcp.example/.well-known/oauth-protected-resource/a/b",
                "https://mcp.example/.well-known/oauth-protected-resource"
            ]
        );
    }

    #[test]
    fn issuer_path_discovery_order_matches_final_spec() {
        let issuer = Url::parse("https://as.example/tenant").expect("url");
        let candidates = authorization_server_candidates(&issuer).expect("candidates");
        assert_eq!(
            candidates.iter().map(Url::as_str).collect::<Vec<_>>(),
            vec![
                "https://as.example/.well-known/oauth-authorization-server/tenant",
                "https://as.example/.well-known/openid-configuration/tenant",
                "https://as.example/tenant/.well-known/openid-configuration"
            ]
        );
    }

    #[test]
    fn root_issuer_has_two_candidates() {
        let issuer = Url::parse("https://as.example").expect("url");
        assert_eq!(
            authorization_server_candidates(&issuer)
                .expect("candidates")
                .len(),
            2
        );
    }

    #[test]
    fn protected_resource_may_cover_a_parent_path_but_not_a_prefix_collision() {
        let requested = Url::parse("https://mcp.example/api/mcp").expect("url");
        assert!(resource_is_allowed(
            &requested,
            &Url::parse("https://mcp.example/").expect("url")
        ));
        assert!(resource_is_allowed(
            &requested,
            &Url::parse("https://mcp.example/api").expect("url")
        ));
        assert!(!resource_is_allowed(
            &requested,
            &Url::parse("https://mcp.example/ap").expect("url")
        ));
        assert!(!resource_is_allowed(
            &requested,
            &Url::parse("https://evil.example/api").expect("url")
        ));
    }
}
