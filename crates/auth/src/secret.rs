//! Opaque, zeroized secret values and environment-backed inputs.

use std::fmt;
use std::sync::Arc;

use zeroize::Zeroizing;

use crate::{AuthError, AuthResult};

/// A credential source that can only resolve from an environment variable.
///
/// Configuration files store the variable name, never the credential value.
#[derive(Clone)]
pub struct ClientSecret {
    source: ClientSecretSource,
}

#[derive(Clone)]
enum ClientSecretSource {
    Environment(String),
    Runtime(SecretValue),
    Resolver(Arc<dyn Fn() -> AuthResult<String> + Send + Sync>),
}

impl ClientSecret {
    /// Create an environment-backed credential source.
    pub fn from_environment(name: impl Into<String>) -> AuthResult<Self> {
        let name = name.into();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(AuthError::MissingCredential);
        }
        Ok(Self {
            source: ClientSecretSource::Environment(name),
        })
    }

    /// Create a secret source backed by an opaque runtime resolver.
    ///
    /// This is intended for credential managers and conformance harnesses.
    /// The resolver is invoked only at the token endpoint and its returned
    /// value is immediately wrapped in zeroizing storage.
    pub fn from_resolver(
        resolver: impl Fn() -> AuthResult<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            source: ClientSecretSource::Resolver(Arc::new(resolver)),
        }
    }

    pub(crate) fn resolve(&self) -> AuthResult<SecretValue> {
        match &self.source {
            ClientSecretSource::Environment(name) => {
                let value = std::env::var(name).map_err(|_| AuthError::MissingCredential)?;
                if value.is_empty() {
                    return Err(AuthError::MissingCredential);
                }
                Ok(SecretValue::new(value))
            }
            ClientSecretSource::Runtime(value) => Ok(value.clone()),
            ClientSecretSource::Resolver(resolver) => {
                let value = resolver()?;
                if value.is_empty() {
                    return Err(AuthError::MissingCredential);
                }
                Ok(SecretValue::new(value))
            }
        }
    }

    pub(crate) fn from_runtime(value: String) -> AuthResult<Self> {
        if value.is_empty() {
            return Err(AuthError::MissingCredential);
        }
        Ok(Self {
            source: ClientSecretSource::Runtime(SecretValue::new(value)),
        })
    }
}

impl fmt::Debug for ClientSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClientSecret([environment-backed])")
    }
}

#[derive(Clone)]
pub(crate) struct SecretValue(Zeroizing<String>);

impl SecretValue {
    pub(crate) fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub(crate) fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}
