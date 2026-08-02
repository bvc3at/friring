//! `vt100`, as Friring builds it: every item of [`panoptes-vt100`], re-exported
//! under the name Cargo's `[patch]` requires.
//!
//! See this crate's `Cargo.toml` for why the indirection exists at all. The
//! upstream API surface is flat — `Parser`, `Screen`, `Cell`, `Color`,
//! `Callbacks`, `MouseProtocolMode`, `MouseProtocolEncoding` — so one glob
//! carries it, and `use vt100::…` reads the same everywhere.
//!
//! [`panoptes-vt100`]: https://github.com/ivan-brko/panoptes-vt100
pub use fork::*;
