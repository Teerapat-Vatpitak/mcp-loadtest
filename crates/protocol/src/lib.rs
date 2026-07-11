//! MCP wire protocol, transports, session, and hang detection for mcp-loadtest.

pub mod factory;
pub mod hang_detector;
pub mod jsonrpc;
pub mod mcp;
pub mod schema;
pub mod session;
pub mod transport;

pub use factory::SessionFactory;
pub use hang_detector::{HangOutcome, hang_detect};
pub use session::{Session, SessionError};
pub use transport::{Transport, TransportError};

/// This crate's version (used as the advertised MCP client version).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
