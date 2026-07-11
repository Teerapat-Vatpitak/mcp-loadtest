//! [`SessionFactory`] — a cloneable handle that asynchronously produces a
//! fresh [`Session`].
//!
//! Built by the run orchestrator (capturing an owned config clone plus the
//! run's stderr-capture options) and attached to every scenario's run
//! context, so scenarios that need a **new server process per measurement** —
//! `cold_start` today, session pools later — can respawn without knowing any
//! transport details.
//!
//! The spawn recipe passed to [`SessionFactory::new`] /
//! [`SessionFactory::new_versioned`] speaks [`SessionError`]. The run layer's
//! own `RunError` → `SessionError` adapter lives beside `RunError` (in the
//! engine crate), because that is the error type scenario code already buckets
//! via `classify_error`.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::mcp::ProtocolVersion;
use crate::session::{Session, SessionError};

/// Boxed future produced by one factory invocation.
type SpawnFuture = Pin<Box<dyn Future<Output = Result<Session, SessionError>> + Send>>;

/// The stored closure: each call starts one fresh spawn + handshake. The
/// argument is the factory's protocol-version override (`None` = the spawn
/// recipe's own default) — version-blind recipes ignore it.
type MakeFn = dyn Fn(Option<ProtocolVersion>) -> SpawnFuture + Send + Sync;

/// Cloneable handle that asynchronously produces a fresh [`Session`].
///
/// Cloning is cheap (an `Arc` bump); every clone produces sessions from the
/// same captured spawn recipe.
///
/// ```no_run
/// use mcp_loadtest_protocol::Session;
/// use mcp_loadtest_protocol::SessionFactory;
///
/// let factory = SessionFactory::new(|| async {
///     Session::spawn("python", ["-m", "my_mcp"]).await
/// });
/// # async fn _use(factory: SessionFactory) -> Result<(), mcp_loadtest_protocol::SessionError> {
/// let session = factory.spawn().await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct SessionFactory {
    make: Arc<MakeFn>,
    /// Advertised-revision override applied to every spawn from this handle
    /// (ADR 0018). `None` = the recipe's default. Set via
    /// [`SessionFactory::with_version`].
    version_override: Option<ProtocolVersion>,
}

impl SessionFactory {
    /// Build a factory from a **version-blind** closure. The closure is
    /// invoked once per [`SessionFactory::spawn`] call and must produce an
    /// independent spawn-and-handshake future each time.
    ///
    /// A factory built this way ignores [`SessionFactory::with_version`]
    /// overrides (the recipe has no version input) — use
    /// [`SessionFactory::new_versioned`] when overrides must be honored, as
    /// `Run::execute`'s factory does.
    pub fn new<F, Fut>(f: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Session, SessionError>> + Send + 'static,
    {
        Self {
            make: Arc::new(move |_| Box::pin(f()) as SpawnFuture),
            version_override: None,
        }
    }

    /// Build a factory from a **version-aware** closure: each spawn receives
    /// the handle's revision override (`None` = use the recipe's default,
    /// e.g. the run config's `[server] protocol_version`). This is what
    /// makes [`SessionFactory::with_version`] — and the `version_matrix`
    /// scenario built on it — work.
    pub fn new_versioned<F, Fut>(f: F) -> Self
    where
        F: Fn(Option<ProtocolVersion>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Session, SessionError>> + Send + 'static,
    {
        Self {
            make: Arc::new(move |v| Box::pin(f(v)) as SpawnFuture),
            version_override: None,
        }
    }

    /// Derive a factory whose spawns advertise `version` instead of the
    /// recipe's default. Cheap (`Arc` bump); the parent handle is unchanged.
    /// No effect on factories built with the version-blind
    /// [`SessionFactory::new`].
    #[must_use]
    pub fn with_version(&self, version: ProtocolVersion) -> Self {
        Self {
            make: Arc::clone(&self.make),
            version_override: Some(version),
        }
    }

    /// Spawn + handshake one fresh session. The returned [`Session`] is
    /// ready to drive (the `initialize` round-trip has completed). The await
    /// covers the full spawn → `initialize` path, which is exactly what
    /// `cold_start` measures.
    pub async fn spawn(&self) -> Result<Session, SessionError> {
        (self.make)(self.version_override).await
    }
}

impl fmt::Debug for SessionFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionFactory").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::transport::TransportError;

    #[tokio::test]
    async fn factory_invokes_closure_once_per_spawn_including_clones() {
        let calls = Arc::new(AtomicU32::new(0));
        let counter = calls.clone();
        let factory = SessionFactory::new(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err::<Session, SessionError>(SessionError::Transport(TransportError::Closed))
            }
        });

        let clone = factory.clone();
        assert!(factory.spawn().await.is_err());
        assert!(clone.spawn().await.is_err());
        assert!(factory.spawn().await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn debug_impl_is_opaque() {
        let factory = SessionFactory::new(|| async {
            Err::<Session, SessionError>(SessionError::Transport(TransportError::Closed))
        });
        let dbg = format!("{factory:?}");
        assert!(dbg.contains("SessionFactory"), "got: {dbg}");
    }
}
