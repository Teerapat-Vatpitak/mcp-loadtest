//! OAuth client registration inputs.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::policy::EndpointPolicy;
use crate::secret::ClientSecret;
use crate::{AuthError, AuthResult};

/// Authentication method used at the OAuth token endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenEndpointAuthMethod {
    /// Select a compatible advertised method from metadata.
    #[default]
    Auto,
    /// Public client authentication (`none`).
    None,
    /// HTTP Basic client-secret authentication.
    ClientSecretBasic,
    /// Form-body client-secret authentication.
    ClientSecretPost,
}

impl TokenEndpointAuthMethod {
    pub(crate) fn metadata_name(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::None => Some("none"),
            Self::ClientSecretBasic => Some("client_secret_basic"),
            Self::ClientSecretPost => Some("client_secret_post"),
        }
    }
}

/// A pre-registered OAuth client.
#[derive(Debug, Clone)]
pub struct PreRegisteredClient {
    client_id: String,
    client_secret: Option<ClientSecret>,
    token_endpoint_auth_method: TokenEndpointAuthMethod,
}

impl PreRegisteredClient {
    /// Create a public pre-registered client using automatic endpoint
    /// authentication selection.
    pub fn new(client_id: impl Into<String>) -> AuthResult<Self> {
        let client_id = client_id.into();
        if client_id.is_empty() || client_id.chars().any(char::is_control) {
            return Err(AuthError::MissingMetadata { field: "client_id" });
        }
        Ok(Self {
            client_id,
            client_secret: None,
            token_endpoint_auth_method: TokenEndpointAuthMethod::Auto,
        })
    }

    /// Attach an environment-backed client secret.
    #[must_use]
    pub fn with_client_secret(mut self, client_secret: ClientSecret) -> Self {
        self.client_secret = Some(client_secret);
        self
    }

    /// Select a token endpoint authentication method.
    #[must_use]
    pub fn with_token_endpoint_auth_method(mut self, method: TokenEndpointAuthMethod) -> Self {
        self.token_endpoint_auth_method = method;
        self
    }
}

/// Client metadata sent to an optional dynamic-registration endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct DynamicClientMetadata {
    /// Human-readable client name.
    pub client_name: String,
    /// Exact loopback redirect URIs accepted by the client.
    pub redirect_uris: Vec<String>,
    /// OAuth grant types requested by this client.
    pub grant_types: Vec<String>,
    /// OAuth response types requested by this client.
    pub response_types: Vec<String>,
    /// Requested token endpoint authentication method.
    pub token_endpoint_auth_method: String,
    /// OIDC application type; CLI clients are native applications.
    pub application_type: String,
}

impl DynamicClientMetadata {
    /// Build metadata for an authorization-code client.
    pub fn authorization_code(
        client_name: impl Into<String>,
        redirect_uri: &Url,
    ) -> AuthResult<Self> {
        if !is_loopback_redirect(redirect_uri) {
            return Err(AuthError::RedirectMismatch);
        }
        Ok(Self {
            client_name: client_name.into(),
            redirect_uris: vec![redirect_uri.as_str().to_owned()],
            grant_types: vec!["authorization_code".to_owned(), "refresh_token".to_owned()],
            response_types: vec!["code".to_owned()],
            token_endpoint_auth_method: "none".to_owned(),
            application_type: "native".to_owned(),
        })
    }
}

/// Supported MCP OAuth client registration strategies.
#[derive(Debug, Clone)]
pub enum ClientRegistration {
    /// Credentials provisioned out of band.
    PreRegistered(PreRegisteredClient),
    /// Client ID Metadata Document URL used directly as the client ID.
    ClientIdMetadata {
        /// Exact HTTPS metadata-document URL.
        client_id_metadata_url: Url,
    },
}

impl ClientRegistration {
    /// Create a pre-registered strategy.
    pub fn pre_registered(client: PreRegisteredClient) -> Self {
        Self::PreRegistered(client)
    }

    /// Create a Client ID Metadata Document strategy.
    pub fn client_id_metadata(
        client_id_metadata_url: Url,
        policy: &EndpointPolicy,
    ) -> AuthResult<Self> {
        policy.validate(&client_id_metadata_url)?;
        Ok(Self::ClientIdMetadata {
            client_id_metadata_url,
        })
    }

    /// Return the exact OAuth client identifier.
    pub fn client_id(&self) -> &str {
        match self {
            Self::PreRegistered(client) => &client.client_id,
            Self::ClientIdMetadata {
                client_id_metadata_url,
            } => client_id_metadata_url.as_str(),
        }
    }

    pub(crate) fn secret(&self) -> Option<&ClientSecret> {
        match self {
            Self::PreRegistered(client) => client.client_secret.as_ref(),
            Self::ClientIdMetadata { .. } => None,
        }
    }

    pub(crate) fn configured_auth_method(&self) -> TokenEndpointAuthMethod {
        match self {
            Self::PreRegistered(client) => client.token_endpoint_auth_method,
            Self::ClientIdMetadata { .. } => TokenEndpointAuthMethod::None,
        }
    }

    pub(crate) fn from_dynamic_response(
        client_id: String,
        client_secret: Option<ClientSecret>,
        method: TokenEndpointAuthMethod,
    ) -> AuthResult<Self> {
        let mut client =
            PreRegisteredClient::new(client_id)?.with_token_endpoint_auth_method(method);
        if let Some(secret) = client_secret {
            client = client.with_client_secret(secret);
        }
        Ok(Self::PreRegistered(client))
    }
}

pub(crate) fn is_loopback_redirect(url: &Url) -> bool {
    if url.scheme() != "http"
        || url.fragment().is_some()
        || url.query().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}
