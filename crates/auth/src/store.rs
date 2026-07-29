//! In-memory, issuer-bound OAuth token storage.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::TokenSet;

/// Exact binding key preventing token reuse after authorization-server migration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenKey {
    /// Exact authorization-server issuer string.
    pub issuer: String,
    /// Exact canonical protected-resource URI.
    pub resource: String,
    /// Exact OAuth client identifier.
    pub client_id: String,
}

impl TokenKey {
    /// Create a token binding key.
    pub fn new(
        issuer: impl Into<String>,
        resource: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Self {
        Self {
            issuer: issuer.into(),
            resource: resource.into(),
            client_id: client_id.into(),
        }
    }
}

/// Minimal token-store interface used by [`crate::OAuthProvider`].
pub trait TokenStore: Send + Sync {
    /// Read a token set.
    fn get(&self, key: &TokenKey) -> Option<TokenSet>;
    /// Replace a token set.
    fn put(&self, key: TokenKey, tokens: TokenSet);
    /// Remove a token set.
    fn remove(&self, key: &TokenKey);
}

/// Process-local token store with no persistent storage.
///
/// This is the default so tokens never reach disk unless a future caller
/// explicitly supplies another `TokenStore`.
#[derive(Debug, Default)]
pub struct MemoryTokenStore {
    tokens: Mutex<HashMap<TokenKey, TokenSet>>,
}

impl MemoryTokenStore {
    /// Create an empty process-local token store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the number of issuer/resource/client bindings currently held.
    pub fn len(&self) -> usize {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Return whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl TokenStore for MemoryTokenStore {
    fn get(&self, key: &TokenKey) -> Option<TokenSet> {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    fn put(&self, key: TokenKey, tokens: TokenSet) {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, tokens);
    }

    fn remove(&self, key: &TokenKey) {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
}
