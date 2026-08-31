//! JSON-RPC 2.0 and MCP wire types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An incoming JSON-RPC message. Requests carry an `id`, notifications do not.
#[derive(Debug, Deserialize)]
pub(crate) struct Request {
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub id: Option<Value>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct RpcError {
    pub code: i32,
    pub message: String,
}

/// JSON-RPC reserved error codes used by this runtime.
pub(crate) mod code {
    pub const INVALID_PARAMS: i32 = -32602;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const PARSE_ERROR: i32 = -32700;
}

/// One block of tool output.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ToolContent {
    /// A plain-text block.
    #[serde(rename = "text")]
    Text {
        /// The text shown to the model.
        text: String,
    },
}

/// The value a tool handler returns.
#[derive(Debug, Serialize)]
pub struct ToolResult {
    /// Human/model-readable blocks.
    pub content: Vec<ToolContent>,
    /// Machine-readable payload, mirrored into `structuredContent`.
    #[serde(rename = "structuredContent", skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    /// Whether the tool ran and failed.
    #[serde(rename = "isError", skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

impl ToolResult {
    /// A successful text-only result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text { text: text.into() }],
            structured: None,
            is_error: false,
        }
    }

    /// A successful result carrying both a rendered summary and structured data.
    pub fn structured(summary: impl Into<String>, value: Value) -> Self {
        Self {
            content: vec![ToolContent::Text {
                text: summary.into(),
            }],
            structured: Some(value),
            is_error: false,
        }
    }

    /// Mark this result as a tool-level failure.
    pub fn into_error(mut self) -> Self {
        self.is_error = true;
        self
    }
}
