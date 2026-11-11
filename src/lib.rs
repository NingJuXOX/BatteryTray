//! BatteryTray core library.
//!
//! Portable protocol, configuration and refresh-policy code is separated from
//! the Windows implementation so behavior can be tested without real hardware.

pub mod battery;
pub mod config;
pub mod error;
pub mod icon;
pub mod policy;
pub mod profiles;
pub mod protocol;
pub mod refresh;
pub mod snapshot;

#[cfg(windows)]
pub mod candidates;
#[cfg(windows)]
pub mod device_api;
#[cfg(windows)]
pub mod platform;

pub use error::{BatteryTrayError, Result};
