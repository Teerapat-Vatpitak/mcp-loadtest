//! PKCE S256 authorization-code request and callback validation.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use url::Url;

use crate::callback::AuthorizationCallback;
use crate::discovery::AuthorizationContext;
use crate::registration::ClientRegistration;
use crate::registration::is_loopback_redirect;
use crate::secret::SecretValue;
use crate::{AuthError, AuthResult, ScopeSet};

/// An authorization request awaiting its exact loopback callback.
#[derive(Clone)]
pub struct PendingAuthorization {
    authorization_url: Url,
    redirect_uri: Url,
    resource: Url,
    client_id: String,
    verifier: SecretValue,
    state: SecretValue,
    expected_issuer: String,
    issuer_required: bool,
    scopes: ScopeSet,
}

impl std::fmt::Debug for PendingAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingAuthorization")
            .field("authorization_url", &"[URL WITH OPAQUE STATE]")
            .field("redirect_uri", &self.redirect_uri)
            .field("resource", &self.resource)
            .field("client_id", &self.client_id)
            .field("verifier", &"[REDACTED]")
            .field("state", &"[REDACTED]")
            .field("expected_issuer", &self.expected_issuer)
            .field("issuer_required", &self.issuer_required)
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl PendingAuthorization {
    /// Build an OAuth authorization URL with PKCE S256, state, resource, and
    /// optional scopes.
    pub fn begin(
        context: &AuthorizationContext,
        registration: &ClientRegistration,
        redirect_uri: Url,
        scopes: ScopeSet,
    ) -> AuthResult<Self> {
        if !is_loopback_redirect(&redirect_uri) {
            return Err(AuthError::RedirectMismatch);
        }
        if !context
            .authorization_server
            .code_challenge_methods_supported
            .iter()
            .any(|method| method == "S256")
        {
            return Err(AuthError::PkceS256Unsupported);
        }
        let mut authorization_url = context
            .authorization_server
            .authorization_endpoint()?
            .clone();
        let verifier = SecretValue::new(random_urlsafe());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.expose().as_bytes()));
        let state = SecretValue::new(random_urlsafe());
        {
            let mut query = authorization_url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", registration.client_id())
                .append_pair("redirect_uri", redirect_uri.as_str())
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", state.expose())
                .append_pair("resource", context.resource.as_str());
            if !scopes.is_empty() {
                query.append_pair("scope", &scopes.to_oauth_string());
            }
        }
        Ok(Self {
            authorization_url,
            redirect_uri,
            resource: context.resource.clone(),
            client_id: registration.client_id().to_owned(),
            verifier,
            state,
            expected_issuer: context.authorization_server.issuer.clone(),
            issuer_required: context
                .authorization_server
                .authorization_response_iss_parameter_supported,
            scopes,
        })
    }

    /// Return the URL the caller may present to the user.
    ///
    /// The URL contains an opaque state value and should not be logged.
    pub fn authorization_url(&self) -> &Url {
        &self.authorization_url
    }

    /// Validate and consume an authorization callback.
    pub fn complete(self, callback_url: &Url) -> AuthResult<CompletedAuthorization> {
        if !same_redirect(&self.redirect_uri, callback_url) {
            return Err(AuthError::RedirectMismatch);
        }
        let callback = AuthorizationCallback::from_url(callback_url)?;
        if !constant_time_equal(self.state.expose().as_bytes(), callback.state.as_bytes()) {
            return Err(AuthError::StateMismatch);
        }
        match callback.issuer.as_deref() {
            Some(issuer) if issuer != self.expected_issuer => {
                return Err(AuthError::CallbackIssuerMismatch);
            }
            None if self.issuer_required => return Err(AuthError::MissingCallbackIssuer),
            _ => {}
        }
        Ok(CompletedAuthorization {
            code: callback.code,
            verifier: self.verifier,
            redirect_uri: self.redirect_uri,
            resource: self.resource,
            client_id: self.client_id,
            scopes: self.scopes,
        })
    }
}

/// Validated authorization-code exchange inputs.
#[derive(Clone)]
pub struct CompletedAuthorization {
    pub(crate) code: SecretValue,
    pub(crate) verifier: SecretValue,
    pub(crate) redirect_uri: Url,
    pub(crate) resource: Url,
    pub(crate) client_id: String,
    pub(crate) scopes: ScopeSet,
}

impl std::fmt::Debug for CompletedAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompletedAuthorization")
            .field("code", &"[REDACTED]")
            .field("verifier", &"[REDACTED]")
            .field("redirect_uri", &self.redirect_uri)
            .field("resource", &self.resource)
            .field("client_id", &self.client_id)
            .field("scopes", &self.scopes)
            .finish()
    }
}

fn random_urlsafe() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

fn same_redirect(expected: &Url, callback: &Url) -> bool {
    expected.scheme() == callback.scheme()
        && expected.host() == callback.host()
        && expected.port_or_known_default() == callback.port_or_known_default()
        && expected.path() == callback.path()
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthorizationServerMetadata, ProtectedResourceMetadata};

    fn context(issuer_supported: bool) -> AuthorizationContext {
        AuthorizationContext {
            resource: Url::parse("https://mcp.example/mcp").expect("url"),
            protected_resource: ProtectedResourceMetadata {
                resource: "https://mcp.example/mcp".to_owned(),
                authorization_servers: vec!["https://as.example".to_owned()],
                scopes_supported: vec![],
            },
            authorization_server: AuthorizationServerMetadata {
                issuer: "https://as.example".to_owned(),
                authorization_endpoint: Some(
                    Url::parse("https://as.example/authorize").expect("url"),
                ),
                token_endpoint: Some(Url::parse("https://as.example/token").expect("url")),
                registration_endpoint: None,
                code_challenge_methods_supported: vec!["S256".to_owned()],
                token_endpoint_auth_methods_supported: vec!["none".to_owned()],
                scopes_supported: vec![],
                grant_types_supported: vec!["authorization_code".to_owned()],
                authorization_response_iss_parameter_supported: issuer_supported,
            },
        }
    }

    #[test]
    fn authorization_request_has_mandatory_security_parameters() {
        let registration =
            ClientRegistration::pre_registered(crate::PreRegisteredClient::new("client").unwrap());
        let pending = PendingAuthorization::begin(
            &context(true),
            &registration,
            Url::parse("http://127.0.0.1:8080/callback").expect("url"),
            ScopeSet::parse("mcp:read"),
        )
        .expect("pending");
        let query: std::collections::BTreeMap<_, _> = pending
            .authorization_url()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(query.get("code_challenge_method").unwrap(), "S256");
        assert_eq!(query.get("resource").unwrap(), "https://mcp.example/mcp");
        assert_eq!(query.get("scope").unwrap(), "mcp:read");
        assert!(query.contains_key("state"));
    }

    #[test]
    fn callback_issuer_is_an_exact_non_normalized_match() {
        let registration =
            ClientRegistration::pre_registered(crate::PreRegisteredClient::new("client").unwrap());
        let redirect = Url::parse("http://127.0.0.1:8080/callback").expect("url");
        let pending = PendingAuthorization::begin(
            &context(true),
            &registration,
            redirect.clone(),
            ScopeSet::default(),
        )
        .expect("pending");
        let state = pending
            .authorization_url()
            .query_pairs()
            .find(|(name, _)| name == "state")
            .expect("state")
            .1
            .into_owned();
        let mut callback = redirect;
        callback
            .query_pairs_mut()
            .append_pair("code", "code")
            .append_pair("state", &state)
            .append_pair("iss", "https://as.example/");
        assert!(matches!(
            pending.complete(&callback),
            Err(AuthError::CallbackIssuerMismatch)
        ));
    }

    #[test]
    fn advertised_issuer_parameter_may_not_be_omitted() {
        let registration =
            ClientRegistration::pre_registered(crate::PreRegisteredClient::new("client").unwrap());
        let redirect = Url::parse("http://127.0.0.1:8080/callback").expect("url");
        let pending = PendingAuthorization::begin(
            &context(true),
            &registration,
            redirect.clone(),
            ScopeSet::default(),
        )
        .expect("pending");
        let state = pending
            .authorization_url()
            .query_pairs()
            .find(|(name, _)| name == "state")
            .expect("state")
            .1
            .into_owned();
        let mut callback = redirect;
        callback
            .query_pairs_mut()
            .append_pair("code", "code")
            .append_pair("state", &state);
        assert!(matches!(
            pending.complete(&callback),
            Err(AuthError::MissingCallbackIssuer)
        ));
    }
}
