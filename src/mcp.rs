use crate::cli::QueryArgs;
use crate::commands;
use crate::error::QghError;
use crate::output::{error_envelope, success_envelope};
use serde_json::{json, Map, Value};
use std::io::{self, BufRead, Write};

const PROTOCOL_VERSION: &str = "2025-11-25";
const JSON_SCHEMA: &str = "https://json-schema.org/draft/2020-12/schema";

pub async fn run_stdio(profile_id: &str) -> Result<(), QghError> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| QghError::storage(error.to_string()))?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle_message(profile_id, message).await,
            Err(_) => Some(protocol_error(Value::Null, -32700, "Parse error")),
        };
        if let Some(response) = response {
            writeln!(
                stdout,
                "{}",
                serde_json::to_string(&response).expect("MCP response must serialize")
            )
            .map_err(|error| QghError::storage(error.to_string()))?;
            stdout
                .flush()
                .map_err(|error| QghError::storage(error.to_string()))?;
        }
    }
    Ok(())
}

async fn handle_message(profile_id: &str, message: Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str);
    let Some(method) = method else {
        return Some(protocol_error(
            id.unwrap_or(Value::Null),
            -32600,
            "Invalid Request",
        ));
    };

    let id = id?;

    Some(match method {
        "initialize" => success_response(id, initialize_result()),
        "ping" => success_response(id, json!({})),
        "tools/list" => success_response(id, json!({ "tools": tool_list() })),
        "tools/call" => call_tool(profile_id, id, message.get("params")).await,
        _ => protocol_error(id, -32601, "Method not found"),
    })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": {
                "listChanged": false
            }
        },
        "serverInfo": {
            "name": "qgh",
            "title": "qgh",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

async fn call_tool(profile_id: &str, id: Value, params: Option<&Value>) -> Value {
    let result = match parse_call(params) {
        Ok(call) => match call.name.as_str() {
            "query" => parse_query_args(&call.arguments)
                .and_then(|args| commands::query(profile_id, args))
                .map(tool_success)
                .unwrap_or_else(tool_error),
            "get" => parse_get_args(&call.arguments)
                .and_then(|source_id| commands::get_local(profile_id, &source_id))
                .map(tool_success)
                .unwrap_or_else(tool_error),
            "status" => parse_empty_args(&call.arguments)
                .and_then(|()| commands::status(profile_id))
                .map(tool_success)
                .unwrap_or_else(tool_error),
            _ => {
                return protocol_error(id, -32601, "Tool not found");
            }
        },
        Err(error) => tool_error(error),
    };
    success_response(id, result)
}

struct ToolCall {
    name: String,
    arguments: Value,
}

fn parse_call(params: Option<&Value>) -> Result<ToolCall, QghError> {
    let params = params
        .ok_or_else(|| validation_error("tools/call params must be an object."))?
        .as_object()
        .ok_or_else(|| validation_error("tools/call params must be an object."))?;
    reject_unknown(params, &["name", "arguments"])?;
    let name = required_string(params, "name")?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Err(validation_error("tools/call arguments must be an object."));
    }
    Ok(ToolCall { name, arguments })
}

fn parse_query_args(arguments: &Value) -> Result<QueryArgs, QghError> {
    let object = argument_object(arguments)?;
    reject_unknown(
        object,
        &[
            "query", "limit", "repo", "label", "state", "author", "issue",
        ],
    )?;
    Ok(QueryArgs {
        query: required_string(object, "query")?,
        limit: optional_usize(object, "limit")?.unwrap_or(10),
        repo: optional_string(object, "repo")?,
        label: optional_string_array(object, "label")?,
        state: optional_string(object, "state")?,
        author: optional_string(object, "author")?,
        issue: optional_i64(object, "issue")?,
        wiki: None,
        json: false,
    })
}

fn parse_get_args(arguments: &Value) -> Result<String, QghError> {
    let object = argument_object(arguments)?;
    reject_unknown(object, &["source_id"])?;
    required_string(object, "source_id")
}

fn parse_empty_args(arguments: &Value) -> Result<(), QghError> {
    let object = argument_object(arguments)?;
    reject_unknown(object, &[])?;
    Ok(())
}

fn argument_object(arguments: &Value) -> Result<&Map<String, Value>, QghError> {
    arguments
        .as_object()
        .ok_or_else(|| validation_error("Tool arguments must be an object."))
}

fn reject_unknown(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), QghError> {
    if let Some(key) = object.keys().find(|key| {
        !allowed
            .iter()
            .any(|allowed_key| allowed_key == &key.as_str())
    }) {
        return Err(validation_error(format!("Unknown MCP parameter `{key}`.")));
    }
    Ok(())
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, QghError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| validation_error(format!("MCP parameter `{key}` must be a string.")))
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, QghError> {
    object
        .get(key)
        .map(|value| {
            value
                .as_str()
                .map(ToString::to_string)
                .ok_or_else(|| validation_error(format!("MCP parameter `{key}` must be a string.")))
        })
        .transpose()
}

fn optional_string_array(object: &Map<String, Value>, key: &str) -> Result<Vec<String>, QghError> {
    let Some(value) = object.get(key) else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err(validation_error(format!(
            "MCP parameter `{key}` must be an array of strings."
        )));
    };
    values
        .iter()
        .map(|value| {
            value.as_str().map(ToString::to_string).ok_or_else(|| {
                validation_error(format!(
                    "MCP parameter `{key}` must be an array of strings."
                ))
            })
        })
        .collect()
}

fn optional_i64(object: &Map<String, Value>, key: &str) -> Result<Option<i64>, QghError> {
    object
        .get(key)
        .map(|value| {
            value.as_i64().ok_or_else(|| {
                validation_error(format!("MCP parameter `{key}` must be an integer."))
            })
        })
        .transpose()
}

fn optional_usize(object: &Map<String, Value>, key: &str) -> Result<Option<usize>, QghError> {
    object
        .get(key)
        .map(|value| {
            let value = value.as_u64().ok_or_else(|| {
                validation_error(format!("MCP parameter `{key}` must be a positive integer."))
            })?;
            usize::try_from(value)
                .map_err(|_| validation_error(format!("MCP parameter `{key}` is too large.")))
        })
        .transpose()
}

fn tool_success(data: Value) -> Value {
    let envelope = success_envelope(data);
    tool_result(envelope, false)
}

fn tool_error(error: QghError) -> Value {
    let envelope = error_envelope(&error);
    tool_result(envelope, true)
}

fn tool_result(envelope: Value, is_error: bool) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": serde_json::to_string(&envelope).expect("qgh envelope must serialize")
            }
        ],
        "structuredContent": envelope,
        "isError": is_error
    })
}

fn validation_error(message: impl Into<String>) -> QghError {
    QghError::validation("validation.mcp", message)
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn protocol_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn tool_list() -> Vec<Value> {
    vec![
        tool(
            "query",
            "Search local GitHub Issue and issue comment sources.",
            query_input_schema(),
            envelope_output_schema(),
        ),
        tool(
            "get",
            "Fetch one authoritative local source by qgh source_id.",
            get_input_schema(),
            envelope_output_schema(),
        ),
        tool(
            "status",
            "Read local profile, source, database, index, and privacy status.",
            empty_input_schema(),
            envelope_output_schema(),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value, output_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
        "outputSchema": output_schema,
        "annotations": {
            "readOnlyHint": true
        }
    })
}

fn query_input_schema() -> Value {
    json!({
        "$schema": JSON_SCHEMA,
        "type": "object",
        "required": ["query"],
        "properties": {
            "query": { "type": "string" },
            "limit": { "type": "integer", "minimum": 0 },
            "repo": { "type": "string", "pattern": "^[^/]+/[^/]+$" },
            "label": {
                "type": "array",
                "items": { "type": "string" }
            },
            "state": { "type": "string", "enum": ["open", "closed"] },
            "author": { "type": "string" },
            "issue": { "type": "integer", "minimum": 1 }
        },
        "additionalProperties": false
    })
}

fn get_input_schema() -> Value {
    json!({
        "$schema": JSON_SCHEMA,
        "type": "object",
        "required": ["source_id"],
        "properties": {
            "source_id": { "type": "string" }
        },
        "additionalProperties": false
    })
}

fn empty_input_schema() -> Value {
    json!({
        "$schema": JSON_SCHEMA,
        "type": "object",
        "additionalProperties": false
    })
}

fn envelope_output_schema() -> Value {
    json!({
        "$schema": JSON_SCHEMA,
        "type": "object",
        "required": ["schema_version", "ok", "warnings", "meta"],
        "properties": {
            "schema_version": { "const": "qgh.v1" },
            "ok": { "type": "boolean" },
            "data": { "type": "object" },
            "error": { "type": "object" },
            "warnings": { "type": "array" },
            "meta": { "type": "object" }
        },
        "additionalProperties": false
    })
}
