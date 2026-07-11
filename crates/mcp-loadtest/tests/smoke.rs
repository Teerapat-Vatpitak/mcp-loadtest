//! Smoke test — proves the test infrastructure works.
//!
//! Real integration tests are added in M1+. This file should always remain green;
//! if it breaks, the build pipeline itself is broken.

use mcp_loadtest::VERSION;

#[test]
fn library_loads() {
    // Just calling into the crate proves the link works.
    let parts = VERSION.split('.').count();
    assert!(parts >= 3, "VERSION should look like semver, got {VERSION}");
}

#[test]
fn smoke_arithmetic() {
    assert_eq!(2 + 2, 4);
}
