#![doc = "Pure concrete-resource resolution boundary and provisional permission-policy composition for `XGENy`. This crate does not issue authority that an Executor may consume."]

mod broker;
mod resolution;

pub use broker::*;
pub use resolution::*;
