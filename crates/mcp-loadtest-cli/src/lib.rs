//! Library half of the `mcp-loadtest-cli` crate.
//!
//! Exposes subcommand modules whose pure logic is useful to test in isolation
//! (without spawning the binary). The binary half (`main.rs`) is the actual
//! `mcp-loadtest` entrypoint and stays small — it delegates to the modules
//! re-exported here.

#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod cmd_compare;
pub mod cmd_cross;
pub mod cmd_deadlock;
pub mod cmd_doctor;
pub mod cmd_run;
pub mod emit;
pub mod explain;
pub mod hints;
