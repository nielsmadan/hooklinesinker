mod agents;
mod app;
mod consumers;
mod environment;
mod events;
mod hooks;
mod ingest;
mod install;
mod lifecycle;
mod normalize;
mod paths;
mod persistence;
mod processes;
pub mod protocol;
mod sinks;
mod state;

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
