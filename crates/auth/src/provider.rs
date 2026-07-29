//! High-level OAuth provider orchestration.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use url::Url;

use crate::discovery::{AuthorizationContext, AuthorizationServerMetadata, DiscoveryClient};
use crate::pkce::PendingAuthorization;
use crate::registration::{ClientRegistration, DynamicClientMetadata};
use crate::store::{MemoryTokenStore, TokenKey, TokenStore};
use crate::token::{AuthorizationHeader, TokenClient, TokenSet};
use crate::{AuthResult, BearerChallenge, EndpointPolicy, ScopeSet};

/// OAuth provider with issuer-bound token storage and single-flight refresh.
#[derive(Debug)]
pub struct OAuthProvider<S = MemoryTokenStore> {
    discovery: DiscoveryClient,
    token_client: TokenClient,
    registration: ClientRegistration,
    store: Arc<S>,
    refresh_guard: Mutex<()>,
}

impl OAuthProvider<MemoryTokenStore> {
    /// Create a provider using the non-persistent in-memory token store.
    pub fn new(policy: EndpointPolicy, registration: ClientRegistration) -> AuthResult<Self> {
        Self::with_store(policy, registration, Arc::new(MemoryTokenStore::new()))
    }

    /// Execute the optional/deprecated dynamic client-registration fallback.
    ///
    /// Callers should prefer pre-registration and Client ID Metadata Documents.
    pub async fn dynamic_register(
        policy: EndpointPolicy,
        metadata: &AuthorizationServerMetadata,
        client_metadata: &DynamicClientMetadata,
    ) -> AuthResult<ClientRegistration> {
        TokenClient::new(policy)?
            .dynamic_register(metadata, client_metadata)
            .await
    }
}

impl<S: TokenStore> OAuthProvider<S> {
    /// Create a provider with an explicit token store.
    pub fn with_store(
        policy: EndpointPolicy,
        registration: ClientRegistration,
        store: Arc<S>,
    ) -> AuthResult<Self> {
        Ok(Self {
            discovery: DiscoveryClient::new(policy.clone())?,
            token_client: TokenClient::new(policy)?,
            registration,
            store,
            refresh_guard: Mutex::new(()),
        })
    }

    /// Discover MCP protected-resource and authorization-server metadata.
    pub async fn discover(
        &self,
        resource: Url,
        challenge: Option<&BearerChallenge>,
    ) -> AuthResult<AuthorizationContext> {
        self.discovery.discover(resource, challenge).await
    }

    /// Start an interactive PKCE authorization-code flow.
    pub fn begin_authorization(
        &self,
        context: &AuthorizationContext,
        redirect_uri: Url,
        scopes: ScopeSet,
    ) -> AuthResult<PendingAuthorization> {
        PendingAuthorization::begin(context, &self.registration, redirect_uri, scopes)
    }

    /// Validate a callback, exchange its code, and cache the resulting tokens.
    pub async fn complete_authorization(
        &self,
        context: &AuthorizationContext,
        pending: PendingAuthorization,
        callback_url: &Url,
    ) -> AuthResult<TokenSet> {
        let completed = pending.complete(callback_url)?;
        let tokens = self
            .token_client
            .exchange_authorization_code(
                &context.authorization_server,
                &self.registration,
                &completed,
            )
            .await?;
        self.store.put(self.key(context), tokens.clone());
        Ok(tokens)
    }

    /// Run the MCP OAuth client-credentials extension and cache its token.
    pub async fn client_credentials(
        &self,
        context: &AuthorizationContext,
        scopes: ScopeSet,
    ) -> AuthResult<TokenSet> {
        let tokens = self
            .token_client
            .client_credentials(
                &context.authorization_server,
                &self.registration,
                &context.resource,
                scopes,
            )
            .await?;
        self.store.put(self.key(context), tokens.clone());
        Ok(tokens)
    }

    /// Return a bearer header, refreshing once when expiry is within 30 seconds.
    pub async fn authorization_header(
        &self,
        context: &AuthorizationContext,
    ) -> AuthResult<Option<AuthorizationHeader>> {
        let key = self.key(context);
        let Some(tokens) = self.store.get(&key) else {
            return Ok(None);
        };
        if !tokens.expires_within(Duration::from_secs(30)) {
            return tokens.authorization_header().map(Some);
        }
        if !tokens.has_refresh_token() {
            self.store.remove(&key);
            return Ok(None);
        }

        let _guard = self.refresh_guard.lock().await;
        let Some(current) = self.store.get(&key) else {
            return Ok(None);
        };
        if !current.expires_within(Duration::from_secs(30)) {
            return current.authorization_header().map(Some);
        }
        let refreshed = self
            .token_client
            .refresh(
                &context.authorization_server,
                &self.registration,
                &context.resource,
                &current,
            )
            .await?;
        let header = refreshed.authorization_header()?;
        self.store.put(key, refreshed);
        Ok(Some(header))
    }

    /// Remove locally cached tokens for this exact issuer/resource/client binding.
    pub fn clear_tokens(&self, context: &AuthorizationContext) {
        self.store.remove(&self.key(context));
    }

    /// Return the provider's exact client ID.
    pub fn client_id(&self) -> &str {
        self.registration.client_id()
    }

    /// Return the configured token store.
    pub fn token_store(&self) -> &Arc<S> {
        &self.store
    }

    fn key(&self, context: &AuthorizationContext) -> TokenKey {
        TokenKey::new(
            context.authorization_server.issuer.clone(),
            context.resource.as_str(),
            self.registration.client_id(),
        )
    }
}
