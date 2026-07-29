//! Token endpoint exchanges, refresh rotation, and redacted bearer headers.

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde::Deserialize;
use url::Url;

use crate::discovery::{AuthorizationServerMetadata, bounded_body};
use crate::pkce::CompletedAuthorization;
use crate::registration::{ClientRegistration, DynamicClientMetadata, TokenEndpointAuthMethod};
use crate::secret::{ClientSecret, SecretValue};
use crate::{AuthError, AuthResult, EndpointPolicy, ScopeSet};

/// OAuth tokens bound to one issuer, resource, and client ID.
#[derive(Clone)]
pub struct TokenSet {
    access_token: SecretValue,
    refresh_token: Option<SecretValue>,
    expires_at: Option<Instant>,
    scopes: ScopeSet,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenSet")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl TokenSet {
    /// Return the scopes associated with this token set.
    pub fn scopes(&self) -> &ScopeSet {
        &self.scopes
    }

    /// Return whether a refresh token exists without exposing it.
    pub fn has_refresh_token(&self) -> bool {
        self.refresh_token.is_some()
    }

    /// Return whether the access token expires within `leeway`.
    pub fn expires_within(&self, leeway: Duration) -> bool {
        self.expires_at
            .is_some_and(|expires_at| expires_at <= Instant::now() + leeway)
    }

    pub(crate) fn authorization_header(&self) -> AuthResult<AuthorizationHeader> {
        AuthorizationHeader::bearer(self.access_token.expose())
    }
}

/// Opaque sensitive HTTP `Authorization` value.
#[derive(Clone)]
pub struct AuthorizationHeader(HeaderValue);

impl AuthorizationHeader {
    fn bearer(access_token: &str) -> AuthResult<Self> {
        let mut value = HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|_| AuthError::InvalidAuthorizationHeader)?;
        value.set_sensitive(true);
        Ok(Self(value))
    }

    /// Apply the sensitive header to an outbound reqwest request.
    pub fn apply(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.header(AUTHORIZATION, self.0.clone())
    }
}

impl std::fmt::Debug for AuthorizationHeader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AuthorizationHeader([REDACTED])")
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DynamicRegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct TokenClient {
    policy: EndpointPolicy,
}

impl TokenClient {
    pub(crate) fn new(policy: EndpointPolicy) -> AuthResult<Self> {
        Ok(Self { policy })
    }

    pub(crate) async fn exchange_authorization_code(
        &self,
        metadata: &AuthorizationServerMetadata,
        registration: &ClientRegistration,
        completed: &CompletedAuthorization,
    ) -> AuthResult<TokenSet> {
        if completed.client_id != registration.client_id() {
            return Err(AuthError::InvalidTokenResponse);
        }
        let fields = vec![
            ("grant_type", "authorization_code".to_owned()),
            ("code", completed.code.expose().to_owned()),
            ("redirect_uri", completed.redirect_uri.as_str().to_owned()),
            ("code_verifier", completed.verifier.expose().to_owned()),
            ("resource", completed.resource.as_str().to_owned()),
        ];
        self.exchange(
            metadata,
            registration,
            fields,
            completed.scopes.clone(),
            None,
        )
        .await
    }

    pub(crate) async fn client_credentials(
        &self,
        metadata: &AuthorizationServerMetadata,
        registration: &ClientRegistration,
        resource: &Url,
        scopes: ScopeSet,
    ) -> AuthResult<TokenSet> {
        if !metadata.grant_types_supported.is_empty()
            && !metadata
                .grant_types_supported
                .iter()
                .any(|grant| grant == "client_credentials")
        {
            return Err(AuthError::MissingMetadata {
                field: "client_credentials grant",
            });
        }
        let mut fields = vec![
            ("grant_type", "client_credentials".to_owned()),
            ("resource", resource.as_str().to_owned()),
        ];
        if !scopes.is_empty() {
            fields.push(("scope", scopes.to_oauth_string()));
        }
        self.exchange(metadata, registration, fields, scopes, None)
            .await
    }

    pub(crate) async fn refresh(
        &self,
        metadata: &AuthorizationServerMetadata,
        registration: &ClientRegistration,
        resource: &Url,
        previous: &TokenSet,
    ) -> AuthResult<TokenSet> {
        let refresh_token = previous
            .refresh_token
            .as_ref()
            .ok_or(AuthError::MissingRefreshToken)?;
        let mut fields = vec![
            ("grant_type", "refresh_token".to_owned()),
            ("refresh_token", refresh_token.expose().to_owned()),
            ("resource", resource.as_str().to_owned()),
        ];
        if !previous.scopes.is_empty() {
            fields.push(("scope", previous.scopes.to_oauth_string()));
        }
        self.exchange(
            metadata,
            registration,
            fields,
            previous.scopes.clone(),
            previous.refresh_token.clone(),
        )
        .await
    }

    async fn exchange(
        &self,
        metadata: &AuthorizationServerMetadata,
        registration: &ClientRegistration,
        mut fields: Vec<(&'static str, String)>,
        requested_scopes: ScopeSet,
        prior_refresh_token: Option<SecretValue>,
    ) -> AuthResult<TokenSet> {
        let endpoint = metadata.token_endpoint()?;
        let http = self.policy.client_for(endpoint).await?;
        let method = resolve_auth_method(metadata, registration)?;
        let mut request = http
            .post(endpoint.clone())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded");
        match method {
            TokenEndpointAuthMethod::ClientSecretBasic => {
                let secret = registration
                    .secret()
                    .ok_or(AuthError::MissingClientSecret)?
                    .resolve()?;
                let credentials = format!(
                    "{}:{}",
                    percent_encode(registration.client_id()),
                    percent_encode(secret.expose())
                );
                let mut header = HeaderValue::from_str(&format!(
                    "Basic {}",
                    STANDARD.encode(credentials.as_bytes())
                ))
                .map_err(|_| AuthError::InvalidAuthorizationHeader)?;
                header.set_sensitive(true);
                request = request.header(AUTHORIZATION, header);
            }
            TokenEndpointAuthMethod::ClientSecretPost => {
                let secret = registration
                    .secret()
                    .ok_or(AuthError::MissingClientSecret)?
                    .resolve()?;
                fields.push(("client_id", registration.client_id().to_owned()));
                fields.push(("client_secret", secret.expose().to_owned()));
            }
            TokenEndpointAuthMethod::None => {
                fields.push(("client_id", registration.client_id().to_owned()));
            }
            TokenEndpointAuthMethod::Auto => unreachable!("automatic method is resolved"),
        }
        let body = encode_form(&fields);
        let response = request
            .body(body)
            .send()
            .await
            .map_err(|_| AuthError::Network {
                operation: "token exchange",
            })?;
        if !response.status().is_success() {
            return Err(AuthError::HttpStatus {
                operation: "token exchange",
                status: response.status().as_u16(),
            });
        }
        let body = bounded_body(
            response,
            self.policy.maximum_response_bytes(),
            "token exchange",
        )
        .await?;
        let response: TokenResponse =
            serde_json::from_slice(&body).map_err(|_| AuthError::InvalidJson {
                operation: "token exchange",
            })?;
        token_set(response, requested_scopes, prior_refresh_token)
    }

    pub(crate) async fn dynamic_register(
        &self,
        metadata: &AuthorizationServerMetadata,
        client_metadata: &DynamicClientMetadata,
    ) -> AuthResult<ClientRegistration> {
        let endpoint = metadata
            .registration_endpoint
            .as_ref()
            .ok_or(AuthError::DynamicRegistrationUnsupported)?;
        let http = self.policy.client_for(endpoint).await?;
        let response = http
            .post(endpoint.clone())
            .json(client_metadata)
            .send()
            .await
            .map_err(|_| AuthError::Network {
                operation: "dynamic client registration",
            })?;
        if !response.status().is_success() {
            return Err(AuthError::HttpStatus {
                operation: "dynamic client registration",
                status: response.status().as_u16(),
            });
        }
        let body = bounded_body(
            response,
            self.policy.maximum_response_bytes(),
            "dynamic client registration",
        )
        .await?;
        let response: DynamicRegistrationResponse =
            serde_json::from_slice(&body).map_err(|_| AuthError::InvalidJson {
                operation: "dynamic client registration",
            })?;
        let method = parse_auth_method(
            response
                .token_endpoint_auth_method
                .as_deref()
                .unwrap_or("none"),
        )?;
        let secret = response
            .client_secret
            .map(ClientSecret::from_runtime)
            .transpose()?;
        ClientRegistration::from_dynamic_response(response.client_id, secret, method)
    }
}

fn token_set(
    response: TokenResponse,
    requested_scopes: ScopeSet,
    prior_refresh_token: Option<SecretValue>,
) -> AuthResult<TokenSet> {
    if response.access_token.is_empty()
        || response
            .token_type
            .as_deref()
            .is_some_and(|token_type| !token_type.eq_ignore_ascii_case("bearer"))
    {
        return Err(AuthError::InvalidTokenResponse);
    }
    let scopes = response
        .scope
        .as_deref()
        .map(ScopeSet::parse)
        .unwrap_or(requested_scopes);
    Ok(TokenSet {
        access_token: SecretValue::new(response.access_token),
        refresh_token: response
            .refresh_token
            .map(SecretValue::new)
            .or(prior_refresh_token),
        expires_at: response
            .expires_in
            .and_then(|seconds| Instant::now().checked_add(Duration::from_secs(seconds))),
        scopes,
    })
}

fn resolve_auth_method(
    metadata: &AuthorizationServerMetadata,
    registration: &ClientRegistration,
) -> AuthResult<TokenEndpointAuthMethod> {
    let supported = &metadata.token_endpoint_auth_methods_supported;
    let configured = registration.configured_auth_method();
    if configured != TokenEndpointAuthMethod::Auto {
        if !supported.is_empty()
            && !supported
                .iter()
                .any(|method| Some(method.as_str()) == configured.metadata_name())
        {
            return Err(AuthError::UnsupportedTokenEndpointAuthMethod);
        }
        return Ok(configured);
    }

    let has_secret = registration.secret().is_some();
    let supports = |name: &str| supported.is_empty() || supported.iter().any(|item| item == name);
    if has_secret && supports("client_secret_basic") {
        Ok(TokenEndpointAuthMethod::ClientSecretBasic)
    } else if has_secret && supports("client_secret_post") {
        Ok(TokenEndpointAuthMethod::ClientSecretPost)
    } else if supports("none") {
        Ok(TokenEndpointAuthMethod::None)
    } else if has_secret {
        Err(AuthError::UnsupportedTokenEndpointAuthMethod)
    } else {
        Err(AuthError::MissingClientSecret)
    }
}

fn parse_auth_method(value: &str) -> AuthResult<TokenEndpointAuthMethod> {
    match value {
        "none" => Ok(TokenEndpointAuthMethod::None),
        "client_secret_basic" => Ok(TokenEndpointAuthMethod::ClientSecretBasic),
        "client_secret_post" => Ok(TokenEndpointAuthMethod::ClientSecretPost),
        _ => Err(AuthError::UnsupportedTokenEndpointAuthMethod),
    }
}

fn percent_encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn encode_form(fields: &[(&str, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_rotation_keeps_old_token_when_omitted() {
        let previous = SecretValue::new("refresh-old".to_owned());
        let tokens = token_set(
            TokenResponse {
                access_token: "access-new".to_owned(),
                token_type: Some("Bearer".to_owned()),
                expires_in: Some(60),
                refresh_token: None,
                scope: None,
            },
            ScopeSet::parse("mcp:read"),
            Some(previous),
        )
        .expect("token");
        assert!(tokens.has_refresh_token());
    }

    #[test]
    fn basic_credentials_use_oauth_form_encoding_before_base64() {
        assert_eq!(percent_encode("client:id"), "client%3Aid");
        assert_eq!(percent_encode("s ecret"), "s+ecret");
    }
}
