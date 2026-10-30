//! Helpers shared by the relay's integration tests.
//!
//! A workspace crate rather than a module included by each test crate, so
//! that no test crate carries the helpers it leaves unused. It is a
//! dev-dependency of `peloton-relay` alone and is never built into a release.
//!
//! # Panics
//!
//! Every helper panics when its setup fails (an unreadable fixture, a state
//! directory that cannot be created, a malformed literal), which fails the
//! calling test. Only a panic that a caller can provoke by its arguments gets
//! a `# Panics` section of its own.

#![expect(
    clippy::missing_panics_doc,
    reason = "every helper panics when its setup fails; the crate documentation states it once"
)]

pub mod api;
pub mod common;
pub mod gated;
pub mod raw_http;
