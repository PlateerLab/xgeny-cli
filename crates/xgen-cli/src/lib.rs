#![doc = "Composition primitives for the local-first `XGEN` CLI."]

mod allow_file;
mod allow_path;
mod allow_process;
mod composition;
mod driver;
mod environment;
mod manifest;
mod material_catalog;
mod model_profile;
mod run_layout;

pub use composition::*;
pub use driver::*;
pub use manifest::MAX_HOST_MODEL_TURNS;
pub use model_profile::*;

#[doc(hidden)]
pub use environment::compatible_environment;
