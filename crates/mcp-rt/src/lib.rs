//! A minimal Model Context Protocol server over stdio.
//!
//! The protocol surface a tool server actually needs is small and stable:
//! `initialize`, `tools/list`, `tools/call` and `ping`, framed as JSON-RPC 2.0
//! messages separated by newlines. Implementing it directly keeps this binary
//! dependency-light — it is meant to be dropped onto installation hosts.

pub mod args;

mod protocol;
mod server;

pub use protocol::{ToolContent, ToolResult};
pub use server::{Server, Tool, ToolFn};

/// Protocol revision this runtime speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Error returned by a tool handler.
///
/// [`ToolError::Invalid`] maps to a JSON-RPC error (the call was malformed and
/// no work was attempted); [`ToolError::Failed`] maps to a successful response
/// carrying `isError: true`, which is how MCP reports a tool that ran and
/// failed — the distinction matters to the calling model.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The arguments could not be understood; nothing was executed.
    #[error("{0}")]
    Invalid(String),
    /// The tool executed and failed.
    #[error("{0}")]
    Failed(String),
}

impl ToolError {
    /// Build an [`ToolError::Invalid`] from anything printable.
    pub fn invalid(msg: impl std::fmt::Display) -> Self {
        Self::Invalid(msg.to_string())
    }

    /// Build a [`ToolError::Failed`] from anything printable.
    pub fn failed(msg: impl std::fmt::Display) -> Self {
        Self::Failed(msg.to_string())
    }
}

/// Result type for tool handlers.
pub type ToolOutcome = Result<ToolResult, ToolError>;
