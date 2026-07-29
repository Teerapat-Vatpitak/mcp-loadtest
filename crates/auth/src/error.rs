//! Redacted authorization errors.

use thiserror::Error;

/// Result type used by this crate.
pub type AuthResult<T> = Result<T, AuthError>;

/// An OAuth or MCP authorization failure.
///
/// Variants deliberately avoid carrying response bodies, authorization
/// codes, tokens, full URLs, or underlying HTTP errors because those values
/// can contain credentials.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// A URL or endpoint violates the configured security policy.
    #[error("authorization endpoint rejected by security policy: {reason}")]
    UnsafeEndpoint {
        /// A non-secret reason suitable for terminal output.
        reason: &'static str,
    },
    /// An endpoint URL was syntactically invalid.
    #[error("invalid {kind} URL")]
    InvalidUrl {
        /// The non-secret role of the URL.
        kind: &'static str,
    },
    /// A discovery endpoint could not be reached.
    #[error("authorization network operation failed during {operation}")]
    Network {
        /// The non-secret operation name.
        operation: &'static str,
    },
    /// An endpoint returned an unsuccessful HTTP status.
    #[error("{operation} returned HTTP {status}")]
    HttpStatus {
        /// The non-secret operation name.
        operation: &'static str,
        /// Numeric HTTP status.
        status: u16,
    },
    /// A response exceeded the configured byte limit.
    #[error("{operation} response exceeded the configured size limit")]
    ResponseTooLarge {
        /// The non-secret operation name.
        operation: &'static str,
    },
    /// JSON metadata or a token response was malformed.
    #[error("invalid JSON response during {operation}")]
    InvalidJson {
        /// The non-secret operation name.
        operation: &'static str,
    },
    /// A challenge could not be parsed safely.
    #[error("invalid WWW-Authenticate Bearer challenge")]
    InvalidChallenge,
    /// Protected-resource metadata identifies another resource.
    #[error("protected-resource metadata resource does not contain the requested resource")]
    ResourceMismatch,
    /// Protected-resource metadata did not name an authorization server.
    #[error("protected-resource metadata did not list an authorization server")]
    MissingAuthorizationServer,
    /// Authorization-server metadata has a non-exact issuer.
    #[error("authorization-server metadata issuer does not exactly match the discovery issuer")]
    IssuerMismatch,
    /// Required metadata is absent.
    #[error("authorization-server metadata is missing {field}")]
    MissingMetadata {
        /// The non-secret metadata field name.
        field: &'static str,
    },
    /// The authorization server does not advertise PKCE S256.
    #[error("authorization server does not advertise PKCE S256")]
    PkceS256Unsupported,
    /// The callback state is absent or does not match.
    #[error("authorization callback state did not match")]
    StateMismatch,
    /// The callback issuer is absent when required.
    #[error("authorization callback omitted the required issuer")]
    MissingCallbackIssuer,
    /// The callback issuer is not an exact string match.
    #[error("authorization callback issuer did not exactly match")]
    CallbackIssuerMismatch,
    /// The callback contains an OAuth error.
    #[error("authorization server rejected the authorization request ({code})")]
    AuthorizationRejected {
        /// A sanitized OAuth error code.
        code: String,
    },
    /// The callback did not contain an authorization code.
    #[error("authorization callback did not contain a code")]
    MissingAuthorizationCode,
    /// The callback repeated or malformed a security-sensitive parameter.
    #[error("authorization callback is malformed")]
    InvalidAuthorizationCallback,
    /// A required credential environment variable was not available.
    #[error("required credential environment variable is unavailable")]
    MissingCredential,
    /// The configured token endpoint authentication method needs a secret.
    #[error("token endpoint authentication requires a client secret")]
    MissingClientSecret,
    /// The configured endpoint authentication method is not advertised.
    #[error("configured token endpoint authentication method is not supported")]
    UnsupportedTokenEndpointAuthMethod,
    /// A token response was missing a usable bearer access token.
    #[error("token response did not contain a usable bearer access token")]
    InvalidTokenResponse,
    /// A token could not be represented as an HTTP Authorization value.
    #[error("access token cannot be represented as an HTTP Authorization header")]
    InvalidAuthorizationHeader,
    /// A refresh was requested but no refresh token exists.
    #[error("no refresh token is available")]
    MissingRefreshToken,
    /// A dynamic registration request was not supported by the server.
    #[error("authorization server does not advertise dynamic client registration")]
    DynamicRegistrationUnsupported,
    /// A scope challenge exhausted the bounded retry budget.
    #[error("authorization scope step-up retry limit reached")]
    StepUpRetryLimit,
    /// The loopback callback listener failed.
    #[error("loopback authorization callback failed during {operation}")]
    LoopbackCallback {
        /// The non-secret callback operation.
        operation: &'static str,
    },
    /// The callback was not sent to the expected redirect URI.
    #[error("authorization callback redirect URI did not match")]
    RedirectMismatch,
}

impl AuthError {
    pub(crate) fn oauth_rejected(code: &str) -> Self {
        let sanitized: String = code
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
            .take(64)
            .collect();
        Self::AuthorizationRejected {
            code: if sanitized.is_empty() {
                "unknown_error".to_owned()
            } else {
                sanitized
            },
        }
    }
}
