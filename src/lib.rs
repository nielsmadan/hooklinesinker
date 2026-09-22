pub(crate) mod agents;
pub(crate) mod app;
pub(crate) mod consumers;
pub(crate) mod environment;
pub(crate) mod events;
pub(crate) mod hooks;
pub(crate) mod ingest;
pub(crate) mod install;
pub(crate) mod lifecycle;
pub(crate) mod normalize;
pub(crate) mod paths;
pub(crate) mod persistence;
pub(crate) mod processes;
pub mod protocol;
pub(crate) mod sinks;
pub(crate) mod state;

pub use app::run;

// Declared as modules rather than `include!`d so rustfmt reaches them; they need
// `pub(crate)` access, so they cannot be ordinary test targets.
#[cfg(test)]
#[path = "../tests/integration.rs"]
mod integration_tests;

#[cfg(test)]
#[path = "../tests/lifecycle.rs"]
mod lifecycle_tests;
