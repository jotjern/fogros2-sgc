//! Error types for the utils crate.

use std::fmt;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug)]
pub struct Error {
    pub msg: String,
    #[cfg(feature = "nightly")]
    backtrace: std::backtrace::Backtrace,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl Default for Error {
    fn default() -> Self {
        Error {
            msg: String::new(),
            #[cfg(feature = "nightly")]
            backtrace: std::backtrace::Backtrace::capture(),
            source: None,
        }
    }
}

impl Error {
    pub fn new(msg: &str) -> Self {
        Error {
            msg: msg.to_string(),
            #[cfg(feature = "nightly")]
            backtrace: std::backtrace::Backtrace::capture(),
            source: None,
        }
    }
}

impl From<config::ConfigError> for Error {
    fn from(err: config::ConfigError) -> Self {
        Error {
            msg: format!("Config error: {}", err),
            #[cfg(feature = "nightly")]
            backtrace: std::backtrace::Backtrace::capture(),
            source: Some(Box::new(err)),
        }
    }
}

impl<T> From<std::sync::PoisonError<T>> for Error {
    fn from(_err: std::sync::PoisonError<T>) -> Self {
        Error {
            msg: String::from("Lock poisoned"),
            #[cfg(feature = "nightly")]
            backtrace: std::backtrace::Backtrace::capture(),
            source: None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error {
            msg: format!("IO error: {}", err),
            #[cfg(feature = "nightly")]
            backtrace: std::backtrace::Backtrace::capture(),
            source: Some(Box::new(err)),
        }
    }
}

impl From<clap::Error> for Error {
    fn from(err: clap::Error) -> Self {
        Error {
            msg: format!("CLI error: {}", err),
            #[cfg(feature = "nightly")]
            backtrace: std::backtrace::Backtrace::capture(),
            source: Some(Box::new(err)),
        }
    }
}
