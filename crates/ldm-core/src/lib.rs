//! LDM core library (original work, MIT).
pub mod config;
pub mod engine;
pub mod error;
pub mod ftp;
pub mod http;
pub mod manager;
pub mod media;
pub mod pac;
pub mod paths;
pub mod store;
pub mod tools;
pub mod util;
pub mod ytdlp;

pub use config::Config;
pub use error::{LdmError, Result};
