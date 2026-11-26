//! Platform-independent domain, hardware boundaries, and IPC v1 codec.
#![forbid(unsafe_code)]

pub mod config;
pub mod ec;
pub mod ipc;
pub mod model;
pub mod system_tools;
pub mod traits;

pub use config::*;
pub use ec::*;
pub use ipc::*;
pub use model::*;
pub use system_tools::*;
pub use traits::*;
