use thiserror::Error;

/// Errors produced by configuration, device I/O and Windows integration.
#[derive(Debug, Error)]
pub enum BatteryTrayError {
    /// A filesystem operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization or parsing failed.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// An image could not be decoded or encoded.
    #[error("image error: {0}")]
    Image(#[from] image::ImageError),
    /// A Windows HID operation failed.
    #[error("HID error: {0}")]
    Hid(String),
    /// A Windows shell operation failed.
    #[error("shell error: {0}")]
    Shell(String),
    /// A native GUI operation failed.
    #[error("GUI error: {0}")]
    Gui(String),
    /// User-provided configuration is invalid.
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

/// Convenient result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, BatteryTrayError>;

impl From<String> for BatteryTrayError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

impl From<&str> for BatteryTrayError {
    fn from(message: &str) -> Self {
        Self::Invalid(message.to_owned())
    }
}

#[cfg(windows)]
impl From<windows::core::Error> for BatteryTrayError {
    fn from(error: windows::core::Error) -> Self {
        Self::Shell(error.to_string())
    }
}

#[cfg(windows)]
impl From<native_windows_gui::NwgError> for BatteryTrayError {
    fn from(error: native_windows_gui::NwgError) -> Self {
        Self::Gui(error.to_string())
    }
}
