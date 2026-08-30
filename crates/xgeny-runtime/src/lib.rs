#![doc = "Capability catalog and routing, durable effect execution, and conservative recovery for `XGENy`."]

mod admission;
mod executor;
mod frontier;
mod lease;
mod material;
mod registry;
mod router;
mod runtime;
mod verification;

#[cfg(test)]
mod runtime_tests;

pub use admission::*;
pub use executor::*;
pub use frontier::*;
pub use lease::*;
pub use material::*;
pub use registry::*;
pub use router::*;
pub use runtime::*;
pub use verification::*;

pub(crate) const LOCAL_EXECUTOR_ID: &str = "xgeny-local";

pub(crate) fn local_executor_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}
