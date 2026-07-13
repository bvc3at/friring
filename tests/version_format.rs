//! Verify that `build.rs` sets `FRIRING_VERSION` correctly.
//!
//! The version embedded at compile time must:
//! - Never start with `v` (the UI prepends it: `concat!("v", env!("FRIRING_VERSION"))`)
//! - Be a valid semver-ish string (digits and dots, optionally with a pre-release suffix)
//!
//! In dev builds (no `FRIRING_RELEASE_VERSION` env var), this equals the
//! Cargo.toml version (`0.0.0-dev`).  In CI release builds the env var is
//! stripped of its `v` prefix (e.g. `v0.7.0` → `0.7.0`).

const VERSION: &str = env!("FRIRING_VERSION");

#[test]
fn version_has_no_v_prefix() {
    assert!(
        !VERSION.starts_with('v'),
        "FRIRING_VERSION must not start with 'v' (got {VERSION:?}); \
         the UI already prepends the prefix"
    );
}

#[test]
fn version_starts_with_digit() {
    assert!(
        VERSION.starts_with(|c: char| c.is_ascii_digit()),
        "FRIRING_VERSION must start with a digit (got {VERSION:?})"
    );
}

#[test]
fn dev_build_version_matches_cargo_toml() {
    // When FRIRING_RELEASE_VERSION is not set, build.rs falls back to CARGO_PKG_VERSION.
    // This test validates that contract for the default (dev) build.
    if VERSION.contains("-dev") {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }
}
