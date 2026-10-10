//! Books for habitfocus: a reading library whose sessions come from reading
//! habits. habitd owns the storage (`store` feature); clients get the model
//! types over IPC and compute the stats themselves.

pub mod api;
pub mod covers;
pub mod googlebooks;
pub mod model;
pub mod stats;

#[cfg(feature = "store")]
pub mod db;
#[cfg(feature = "store")]
mod http;
#[cfg(feature = "store")]
pub mod import;
#[cfg(feature = "store")]
mod inbox;
#[cfg(feature = "store")]
pub mod openlibrary;
#[cfg(feature = "store")]
mod sessions;
#[cfg(feature = "store")]
pub mod sync;

#[cfg(feature = "store")]
pub use db::Db;
#[cfg(feature = "store")]
pub use inbox::Recorded;
pub use model::{Book, Format, Pending, Pin, Reading, Session, Source, Status};
