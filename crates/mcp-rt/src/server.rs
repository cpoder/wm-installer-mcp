//! The stdio dispatch loop.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

use crate::protocol::{code, Request, Response, ToolResult};
use crate::{ToolError, PROTOCOL_VERSION};

/// A tool handler: takes the `arguments` object, returns a result.
pub type ToolFn = Box<dyn Fn(&Value) -> Result<ToolResult, ToolError> + Send + Sync>;

/// One registered tool.
pub struct Tool {
    name: String,
    description: String,
    schema: Value,
    handler: ToolFn,
}

impl Tool {
    /// Register a tool with its JSON Schema for `arguments`.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        schema: Value,
        handler: ToolFn,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
            handler,
        }
    }
}

/// An MCP server bound to stdin/stdout.
pub struct Server {
    name: String,
    version: String,
    instructions: Option<String>,
    tools: BTreeMap<String, Tool>,
}

impl Server {
    /// Create a server advertising `name` and `version`.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            instructions: None,
            tools: BTreeMap::new(),
        }
    }

    /// Set the `instructions` string returned by `initialize`.
    pub fn instructions(mut self, text: impl Into<String>) -> Self {
        self.instructions = Some(text.into());
        self
    }

    /// Register a tool. A duplicate name replaces the earlier registration.
    pub fn tool(mut self, tool: Tool) -> Self {
        self.tools.insert(tool.name.clone(), tool);
        self
    }

    /// Read requests from stdin until EOF, writing responses to stdout.
    ///
    /// Diagnostics go to stderr: stdout carries the protocol and nothing else.
    pub fn run(self) -> io::Result<()> {
        let stdin = io::stdin();
        let mut stdout = io::stdout().lock();
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let Some(response) = self.handle_line(&line) else {
                continue; // notification: nothing to send back
            };
            serde_json::to_writer(&mut stdout, &response)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
        Ok(())
    }

    fn handle_line(&self, line: &str) -> Option<Response> {
        let request: Request = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                return Some(Response::err(Value::Null, code::PARSE_ERROR, e.to_string()));
            }
        };
        // Notifications carry no id and expect no reply.
        let id = request.id.clone()?;
        Some(self.dispatch(&request.method, &request.params, id))
    }

    fn dispatch(&self, method: &str, params: &Value, id: Value) -> Response {
        match method {
            "initialize" => Response::ok(id, self.initialize_result()),
            "ping" => Response::ok(id, json!({})),
            "tools/list" => Response::ok(id, json!({ "tools": self.tool_descriptors() })),
            "tools/call" => self.call_tool(params, id),
            other => Response::err(
                id,
                code::METHOD_NOT_FOUND,
                format!("unsupported method: {other}"),
            ),
        }
    }

    fn initialize_result(&self) -> Value {
        let mut result = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": self.name, "version": self.version },
        });
        if let Some(text) = &self.instructions {
            result["instructions"] = json!(text);
        }
        result
    }

    fn tool_descriptors(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.schema,
                })
            })
            .collect()
    }

    fn call_tool(&self, params: &Value, id: Value) -> Response {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Response::err(id, code::INVALID_PARAMS, "missing tool name");
        };
        let Some(tool) = self.tools.get(name) else {
            return Response::err(id, code::INVALID_PARAMS, format!("unknown tool: {name}"));
        };
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        match (tool.handler)(&args) {
            Ok(result) => Response::ok(id, to_value(result)),
            // The call itself was malformed: report it as a protocol error.
            Err(ToolError::Invalid(msg)) => Response::err(id, code::INVALID_PARAMS, msg),
            // The tool ran and failed: MCP wants a successful envelope with isError.
            Err(ToolError::Failed(msg)) => {
                Response::ok(id, to_value(ToolResult::text(msg).into_error()))
            }
        }
    }
}

fn to_value(result: ToolResult) -> Value {
    serde_json::to_value(result).unwrap_or_else(|e| {
        json!({
            "content": [{ "type": "text", "text": format!("failed to serialize result: {e}") }],
            "isError": true,
        })
    })
}
