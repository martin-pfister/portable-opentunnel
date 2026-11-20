//! Process exit statuses and the error type every command returns.

use std::fmt;
use std::io;

pub const SUCCESS: i32 = 0;
pub const FAILURE: i32 = 1;
pub const NO_TUNNEL: i32 = 3;
pub const INTERRUPTED: i32 = 130;
pub const TERMINATED: i32 = 143;

#[derive(Debug)]
pub struct AppError {
    pub code: i32,
    pub message: String,
}

impl AppError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn fail(message: impl Into<String>) -> Self {
        Self::new(FAILURE, message)
    }

    pub fn no_tunnel(message: impl Into<String>) -> Self {
        Self::new(NO_TUNNEL, message)
    }

    /// A context-producing adapter for `io::Error`.
    pub fn io(context: impl Into<String>) -> impl FnOnce(io::Error) -> Self {
        let context = context.into();
        move |source| Self::fail(format!("{context}: {source}"))
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_adapter_keeps_the_context() {
        let error = AppError::io("reading state")(io::Error::other("denied"));
        assert_eq!(error.code, FAILURE);
        assert!(error.message.starts_with("reading state: "));
    }
}
