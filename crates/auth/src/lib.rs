//! OAuth authorization support for MCP 2026-07-28 clients.
//!
//! The crate intentionally does not launch a browser and does not persist
//! credentials. Callers explicitly start an authorization flow, present the
//! returned URL to a user, and complete it with a loopback callback URL.

mod callback;
mod challenge;
mod discovery;
mod error;
mod pkce;
mod policy;
mod provider;
mod registration;
mod scope;
mod secret;
mod store;
mod token;

pub use callback::{AuthorizationCallback, LoopbackCallback};
pub use challenge::BearerChallenge;
pub use discovery::{
    AuthorizationContext, AuthorizationServerMetadata, DiscoveryClient, ProtectedResourceMetadata,
};
pub use error::{AuthError, AuthResult};
pub use pkce::{CompletedAuthorization, PendingAuthorization};
pub use policy::EndpointPolicy;
pub use provider::OAuthProvider;
pub use registration::{
    ClientRegistration, DynamicClientMetadata, PreRegisteredClient, TokenEndpointAuthMethod,
};
pub use scope::{ScopeSet, StepUpTracker};
pub use secret::ClientSecret;
pub use store::{MemoryTokenStore, TokenKey, TokenStore};
pub use token::{AuthorizationHeader, TokenSet};
