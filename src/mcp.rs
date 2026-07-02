use crate::cli::QueryArgs;
use crate::commands;
use crate::error::QghError;
use crate::freshness;
use crate::output::{error_envelope, success_envelope_with_meta_and_warnings};
use crate::resolution::{
    repo_scope_from_command_arg, repo_scope_from_worktree, resolve_context,
    resolve_explicit_context, ResolvedCommandContext, ResolvedRepoScope,
};
use serde_json::{json, Map, Value};
use std::io::{self, BufRead, Write};

const PROTOCOL_VERSION: &str = "2025-11-25";
const JSON_SCHEMA: &str = "https://json-schema.org/draft/2020-12/schema";

pub async fn run_stdio(profile_arg: Option<String>) -> Result<(), QghError> {
    let session = McpSession { profile_arg };
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| QghError::storage(error.to_string()))?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle_message(&session, message).await,
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

struct McpSession {
    profile_arg: Option<String>,
}

async fn handle_message(session: &McpSession, message: Value) -> Option<Value> {
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
        "tools/call" => call_tool(session, id, message.get("params")).await,
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

async fn call_tool(session: &McpSession, id: Value, params: Option<&Value>) -> Value {
    let result = match parse_call(params) {
        Ok(call) => match call.name.as_str() {
            "query" => tool_query(session, &call.arguments).unwrap_or_else(tool_error),
            "get" => tool_get(session, &call.arguments)
                .await
                .unwrap_or_else(tool_error),
            "status" => tool_status(session, &call.arguments).unwrap_or_else(tool_error),
            _ => {
                return protocol_error(id, -32601, "Tool not found");
            }
        },
        Err(error) => tool_error(error),
    };
    success_response(id, result)
}

fn tool_query(session: &McpSession, arguments: &Value) -> Result<Value, QghError> {
    let args = parse_query_args(arguments)?;
    let repo_scope = effective_query_repo_scope(&args)?;
    let context = resolve_mcp_context(session, repo_scope)?;
    let outcome = commands::query(&context.profile_id, args, context.repo_scope.as_ref())?;
    Ok(tool_success(
        outcome.data,
        outcome.warnings,
        context.meta_json(),
    ))
}

async fn tool_get(session: &McpSession, arguments: &Value) -> Result<Value, QghError> {
    let args = parse_get_args(arguments)?;
    let context = resolve_mcp_get_context(session, args.profile_id.as_deref())?;
    let data = commands::get(
        &context.profile_id,
        &args.source_id,
        context.repo_scope.as_ref(),
        false,
    )
    .await?;
    Ok(tool_success(data, Vec::new(), context.meta_json()))
}

fn tool_status(session: &McpSession, arguments: &Value) -> Result<Value, QghError> {
    let args = parse_status_args(arguments)?;
    let context = resolve_mcp_context(session, repo_scope_from_worktree()?)?;
    let mut outcome = commands::status(&context.profile_id, &args, context.repo_scope.as_ref())?;
    let data = &mut outcome.data;
    data["resolution"] = context.resolution_json();
    Ok(tool_success(
        outcome.data,
        outcome.warnings,
        context.meta_json(),
    ))
}

fn effective_query_repo_scope(args: &QueryArgs) -> Result<Option<ResolvedRepoScope>, QghError> {
    if let Some(repo) = &args.repo {
        return repo_scope_from_command_arg(repo).map(Some);
    }
    repo_scope_from_worktree()
}

fn resolve_mcp_context(
    session: &McpSession,
    repo_scope: Option<ResolvedRepoScope>,
) -> Result<ResolvedCommandContext, QghError> {
    resolve_context(session.profile_arg.as_deref(), repo_scope)
}

fn resolve_mcp_get_context(
    session: &McpSession,
    get_args_profile_id: Option<&str>,
) -> Result<ResolvedCommandContext, QghError> {
    if let Some(session_profile_id) = session.profile_arg.as_deref() {
        reject_profile_mismatch(session_profile_id, get_args_profile_id, "server --profile")?;
        if get_args_profile_id.is_some() {
            return resolve_explicit_context(session_profile_id, "get_args", None);
        }
        return resolve_context(Some(session_profile_id), repo_scope_from_worktree()?);
    }
    if let Ok(env_profile_id) = std::env::var("QGH_PROFILE") {
        reject_profile_mismatch(&env_profile_id, get_args_profile_id, "QGH_PROFILE")?;
        if get_args_profile_id.is_some() {
            return resolve_explicit_context(&env_profile_id, "get_args", None);
        }
        return resolve_context(None, repo_scope_from_worktree()?);
    }
    if let Some(profile_id) = get_args_profile_id {
        return resolve_explicit_context(profile_id, "get_args", None);
    }
    resolve_context(None, repo_scope_from_worktree()?)
}

fn reject_profile_mismatch(
    boundary_profile_id: &str,
    get_args_profile_id: Option<&str>,
    boundary_source: &str,
) -> Result<(), QghError> {
    if get_args_profile_id.is_some_and(|profile_id| profile_id != boundary_profile_id) {
        return Err(QghError::validation(
            "validation.mcp",
            format!("MCP get_args.profile_id cannot differ from {boundary_source}."),
        )
        .with_details(json!({
            "boundary_profile_id": boundary_profile_id,
            "get_args_profile_id": get_args_profile_id
        }))
        .with_hint("Start a separate MCP server for a different profile."));
    }
    Ok(())
}

struct ToolCall {
    name: String,
    arguments: Value,
}

struct GetArgs {
    source_id: String,
    profile_id: Option<String>,
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
            "query",
            "limit",
            "repo",
            "label",
            "state",
            "author",
            "issue",
            "max_age",
            "require_fresh",
        ],
    )?;
    Ok(QueryArgs {
        query: required_string(object, "query")?,
        limit: optional_positive_usize(object, "limit")?,
        repo: optional_string(object, "repo")?,
        label: optional_string_array(object, "label")?,
        state: optional_string(object, "state")?,
        author: optional_string(object, "author")?,
        issue: optional_i64(object, "issue")?,
        wiki: None,
        max_age: optional_duration_string(object, "max_age")?,
        require_fresh: optional_bool(object, "require_fresh")?.unwrap_or(false),
        json: false,
    })
}

fn parse_status_args(arguments: &Value) -> Result<crate::cli::StatusArgs, QghError> {
    let object = argument_object(arguments)?;
    reject_unknown(object, &["max_age", "require_fresh"])?;
    Ok(crate::cli::StatusArgs {
        max_age: optional_duration_string(object, "max_age")?,
        require_fresh: optional_bool(object, "require_fresh")?.unwrap_or(false),
        json: false,
    })
}

fn parse_get_args(arguments: &Value) -> Result<GetArgs, QghError> {
    let object = argument_object(arguments)?;
    reject_unknown(object, &["source_id", "profile_id"])?;
    Ok(GetArgs {
        source_id: required_string(object, "source_id")?,
        profile_id: optional_string(object, "profile_id")?,
    })
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

fn optional_duration_string(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, QghError> {
    let value = optional_string(object, key)?;
    if let Some(value) = value.as_deref() {
        freshness::parse_duration_seconds(key, value)
            .map_err(|error| validation_error(error.message))?;
    }
    Ok(value)
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

fn optional_positive_usize(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<usize>, QghError> {
    object
        .get(key)
        .map(|value| {
            let value = value.as_u64().ok_or_else(|| {
                validation_error(format!("MCP parameter `{key}` must be a positive integer."))
            })?;
            if value == 0 {
                return Err(validation_error(format!(
                    "MCP parameter `{key}` must be greater than zero."
                )));
            }
            usize::try_from(value)
                .map_err(|_| validation_error(format!("MCP parameter `{key}` is too large.")))
        })
        .transpose()
}

fn optional_bool(object: &Map<String, Value>, key: &str) -> Result<Option<bool>, QghError> {
    object
        .get(key)
        .map(|value| {
            value.as_bool().ok_or_else(|| {
                validation_error(format!("MCP parameter `{key}` must be a boolean."))
            })
        })
        .transpose()
}

fn tool_success(data: Value, warnings: Vec<Value>, meta: Value) -> Value {
    let envelope = success_envelope_with_meta_and_warnings(data, meta, warnings);
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
            status_input_schema(),
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
            "limit": { "type": "integer", "minimum": 1 },
            "repo": { "type": "string", "pattern": "^[^/]+/[^/]+$" },
            "label": {
                "type": "array",
                "items": { "type": "string" }
            },
            "state": { "type": "string", "enum": ["open", "closed"] },
            "author": { "type": "string" },
            "issue": { "type": "integer", "minimum": 1 },
            "max_age": { "type": "string", "pattern": "^[1-9][0-9]*(s|m|h|d|mo)$" },
            "require_fresh": { "type": "boolean" }
        },
        "additionalProperties": false
    })
}

fn status_input_schema() -> Value {
    json!({
        "$schema": JSON_SCHEMA,
        "type": "object",
        "properties": {
            "max_age": { "type": "string", "pattern": "^[1-9][0-9]*(s|m|h|d|mo)$" },
            "require_fresh": { "type": "boolean" }
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
            "source_id": { "type": "string" },
            "profile_id": { "type": "string" }
        },
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
            "warnings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["code", "severity", "message"],
                    "properties": {
                        "code": { "type": "string" },
                        "severity": {
                            "type": "string",
                            "enum": ["warn", "warn_strong", "fail"]
                        },
                        "message": { "type": "string" }
                    },
                    "additionalProperties": false
                }
            },
            "meta": {
                "type": "object",
                "properties": {
                    "profile_id": {
                        "type": ["string", "null"]
                    },
                    "profile_source": {
                        "type": ["string", "null"],
                        "enum": ["cli", "env", "single_match", "get_args", null]
                    },
                    "repo": {
                        "type": ["string", "null"],
                        "pattern": "^[^/]+/[^/]+$"
                    },
                    "repo_source": {
                        "type": ["string", "null"],
                        "enum": ["cli", "repo_policy", "git_remote", "command", null]
                    },
                    "repo_policy_path": {
                        "type": ["string", "null"]
                    }
                },
                "additionalProperties": false
            }
        },
        "additionalProperties": false
    })
}
