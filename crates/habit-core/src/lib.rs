//! Pure habit/blocking logic. No I/O: the daemon feeds [`engine::Input`]s and
//! carries out the returned [`engine::Effect`]s, which keeps everything testable.

pub mod archive;
pub mod config;
pub mod duration;
pub mod engine;
pub mod lock;
pub mod snapshot;
pub mod state;
pub mod stats;

pub use config::Config;
pub use engine::{BrowserTab, Effect, Engine, Input, UsageSource, WindowInfo};
pub use snapshot::Snapshot;
pub use state::State;

/// Example configuration, shipped by `hf init`.
pub const EXAMPLE_CONFIG: &str = include_str!("../../../contrib/config.example.toml");
