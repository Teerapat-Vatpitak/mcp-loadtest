//! Re-export of the trace layer (split across core and engine).
pub use mcp_loadtest_core::trace::{TraceError, format};
pub use mcp_loadtest_engine::trace::{replay, writer};

pub use format::{Direction, FORMAT_VERSION, TraceFrame, TraceHeader};
pub use replay::{Divergence, ReplayReport};
pub use writer::{TraceWriter, TracingTransport};
