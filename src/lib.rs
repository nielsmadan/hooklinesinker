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

#[cfg(test)]
mod integration_tests {
    use crate as hooklinesinker;

    include!("../tests/integration.rs");
}

#[cfg(test)]
mod lifecycle_tests {
    use crate as hooklinesinker;

    include!("../tests/lifecycle.rs");
}
