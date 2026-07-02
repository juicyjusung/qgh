use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

#[test]
fn release_contract_artifacts_match_cli_help_and_mcp_surface() {
    let help = qgh(&["--help"]);
    assert_success(&help);
    let help_text = stdout_text(&help);
    assert!(help_text.contains("human output by default"));
    assert!(help_text.contains("use --json for qgh.v1 envelopes"));
    for command in [
        "init", "sync", "query", "search", "get", "status", "doctor", "mcp",
    ] {
        assert!(
            help_text.contains(command),
            "missing top-level help command: {command}"
        );
    }
    for excluded in ["eval", "embed", "write", "delete", "update"] {
        assert!(
            !help_text.contains(&format!("  {excluded}")),
            "unexpected top-level help command: {excluded}"
        );
    }

    for args in [
        &["init", "--help"][..],
        &["init", "repo", "--help"][..],
        &["sync", "--help"][..],
        &["query", "--help"][..],
        &["get", "--help"][..],
        &["status", "--help"][..],
        &["doctor", "--help"][..],
    ] {
        let output = qgh(args);
        assert_success(&output);
    }
    let init_help = stdout_text(&qgh(&["init", "--help"]));
    assert!(init_help.contains("-y"));
    assert!(init_help.contains("--yes"));
    assert!(init_help.contains("github_cli"));
    assert!(init_help.contains("env"));
    assert!(
        !init_help.contains("credential"),
        "init help must not present credential_store as supported"
    );
    let get_help = stdout_text(&qgh(&["get", "--help"]));
    assert!(get_help.contains("One to 20 qgh source_id values"));
    assert!(get_help.contains("--verify-lifecycle"));

    let mcp = mcp([
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "qgh-release-test", "version": "0"}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    ]);
    assert_success(&mcp);
    let messages = stdout_json_lines(&mcp);
    let tools = messages[1]["result"]["tools"].as_array().unwrap();
    let tool_names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(tool_names, ["query", "get", "status"]);
    for tool in tools {
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        match tool["name"].as_str().unwrap() {
            "query" => {
                assert_eq!(
                    schema_property_names(&tool["inputSchema"]),
                    BTreeSet::from([
                        "author".to_string(),
                        "issue".to_string(),
                        "label".to_string(),
                        "limit".to_string(),
                        "max_age".to_string(),
                        "query".to_string(),
                        "repo".to_string(),
                        "require_fresh".to_string(),
                        "state".to_string(),
                    ])
                );
                assert_eq!(tool["inputSchema"]["required"], json!(["query"]));
                assert_eq!(
                    tool["inputSchema"]["properties"]["state"]["enum"],
                    json!(["open", "closed"])
                );
                assert_eq!(tool["inputSchema"]["properties"]["limit"]["minimum"], 1);
                assert_eq!(tool["inputSchema"]["properties"]["issue"]["minimum"], 1);
                assert_eq!(
                    tool["inputSchema"]["properties"]["repo"]["pattern"],
                    "^[^/]+/[^/]+$"
                );
            }
            "get" => {
                assert_eq!(
                    schema_property_names(&tool["inputSchema"]),
                    BTreeSet::from(["profile_id".to_string(), "source_id".to_string()])
                );
                assert_eq!(tool["inputSchema"]["required"], json!(["source_id"]));
                assert!(
                    tool["inputSchema"]["properties"]
                        .get("verify_lifecycle")
                        .is_none(),
                    "MCP get must stay local-only/read-only; lifecycle verification is CLI-only"
                );
            }
            "status" => {
                assert_eq!(
                    schema_property_names(&tool["inputSchema"]),
                    BTreeSet::from(["max_age".to_string(), "require_fresh".to_string()])
                );
                assert!(tool["inputSchema"].get("required").is_none());
            }
            name => panic!("unexpected MCP tool in release contract: {name}"),
        }
        assert_eq!(tool["outputSchema"]["type"], "object");
        assert_eq!(tool["outputSchema"]["additionalProperties"], false);
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let artifact: Value =
        serde_json::from_str(&fs::read_to_string(root.join("docs/release-artifact.json")).unwrap())
            .unwrap();
    assert_eq!(artifact["schema_version"], "qgh.release.v1");
    assert_eq!(
        artifact["contract"]["mcp_tools"],
        json!(["query", "get", "status"])
    );
    assert_eq!(
        artifact["contract"]["cli_only_commands"],
        json!(["init", "sync", "doctor"])
    );
    assert_eq!(
        artifact["contract"]["product_core"],
        "CLI-first local retrieval with strict --json envelopes and SQLite/Tantivy behavior"
    );
    assert_eq!(
        artifact["contract"]["contract_source_of_truth"],
        json!([
            "CLI args",
            "docs/schemas/*.schema.json",
            "local SQLite/Tantivy retrieval behavior"
        ])
    );
    assert_eq!(
        artifact["contract"]["supported_token_sources"],
        json!(["github_cli", "env"])
    );
    assert_eq!(
        artifact["contract"]["init_yes_aliases"],
        json!(["--yes", "-y"])
    );
    assert_eq!(artifact["contract"]["get_batch"]["max_source_ids"], 20);
    assert_eq!(
        artifact["contract"]["get_batch"]["item_errors"],
        json!([
            "source.not_found",
            "source.tombstoned",
            "source.outside_effective_scope"
        ])
    );
    assert_eq!(
        artifact["contract"]["get_batch"]["lifecycle_check_policy"],
        "CLI opt-in verify_lifecycle; sequential max_in_flight_requests=1 when enabled; MCP get remains local-only"
    );
    assert_eq!(
        artifact["contract"]["human_output"],
        "default successful CLI stdout is command-specific human summaries; pass --json for stable qgh.v1 envelopes"
    );
    assert!(artifact["contract"]["init_behavior"]
        .as_str()
        .unwrap()
        .contains("previews inferred profile/repo defaults"));
    assert_eq!(
        artifact["contract"]["mcp_role"],
        "optional read-only thin adapter over the CLI JSON/local retrieval contract"
    );
    assert!(artifact["contract"]["not_exposed_to_mcp"]
        .as_array()
        .unwrap()
        .iter()
        .any(|command| command == "init"));
    for path in artifact["schema_snapshots"].as_array().unwrap() {
        let path = path.as_str().unwrap();
        assert!(root.join(path).exists(), "missing schema snapshot: {path}");
    }
    let init_schema: Value = serde_json::from_str(
        &fs::read_to_string(root.join("docs/schemas/init-output.schema.json")).unwrap(),
    )
    .unwrap();
    let error_schema: Value = serde_json::from_str(
        &fs::read_to_string(root.join("docs/schemas/error.schema.json")).unwrap(),
    )
    .unwrap();
    let sync_schema: Value = serde_json::from_str(
        &fs::read_to_string(root.join("docs/schemas/sync-output.schema.json")).unwrap(),
    )
    .unwrap();
    let status_schema: Value = serde_json::from_str(
        &fs::read_to_string(root.join("docs/schemas/status-output.schema.json")).unwrap(),
    )
    .unwrap();
    let get_schema: Value = serde_json::from_str(
        &fs::read_to_string(root.join("docs/schemas/get-output.schema.json")).unwrap(),
    )
    .unwrap();
    for required in [
        "profile_config_path",
        "profile_id",
        "profile_action",
        "repo",
        "repo_allowlist_action",
        "repo_policy_action",
        "token_source",
        "next_steps",
    ] {
        assert!(
            init_schema["oneOf"][0]["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == required),
            "init output schema must require {required}"
        );
    }
    assert_eq!(
        init_schema["properties"]["repo_policy_action"]["enum"],
        json!(["created", "overwritten", "already_exists", "skipped"])
    );
    assert_eq!(
        init_schema["properties"]["token_source"]["properties"]["kind"]["enum"],
        json!(["github_cli", "env"])
    );
    let error_codes = error_schema["$defs"]["error_code"]["enum"]
        .as_array()
        .unwrap();
    for code in [
        "validation.init_cancelled",
        "validation.batch_size",
        "validation.invalid_issue_number",
        "validation.window_requires_recent",
        "validation.backfill_conflicts",
        "validation.requires_backfill",
        "validation.repo_required",
    ] {
        assert!(
            error_codes.contains(&json!(code)),
            "released error schema must include {code}"
        );
    }
    assert_eq!(
        sync_schema["properties"]["sync_state"]["enum"],
        json!(["ok", "backoff", "skipped_fresh"])
    );
    assert_eq!(sync_schema["additionalProperties"], false);
    assert_eq!(
        schema_property_names(&sync_schema),
        BTreeSet::from([
            "backfill".to_string(),
            "backoff".to_string(),
            "comment_listing".to_string(),
            "comments".to_string(),
            "cursors".to_string(),
            "index".to_string(),
            "issues".to_string(),
            "lifecycle".to_string(),
            "profile_id".to_string(),
            "reconciliation".to_string(),
            "scheduler".to_string(),
            "sources".to_string(),
            "sync".to_string(),
            "sync_run_id".to_string(),
            "sync_state".to_string(),
            "target".to_string(),
        ])
    );
    assert_eq!(sync_schema["properties"]["index"]["$ref"], "#/$defs/index");
    let sync_index = &sync_schema["$defs"]["index"];
    assert_eq!(
        sync_index["required"],
        json!(["active_generation", "dirty_task_count"])
    );
    assert_eq!(sync_index["additionalProperties"], false);
    assert_eq!(
        schema_property_names(sync_index),
        BTreeSet::from([
            "active_generation".to_string(),
            "dirty_task_count".to_string()
        ])
    );
    assert_eq!(sync_index["properties"]["active_generation"]["minimum"], 0);
    assert_eq!(sync_index["properties"]["dirty_task_count"]["minimum"], 0);
    assert_eq!(
        sync_schema["properties"]["issues"]["$ref"],
        "#/$defs/issues"
    );
    let sync_issues = &sync_schema["$defs"]["issues"];
    assert_eq!(
        sync_issues["required"],
        json!(["fetched", "upserted", "skipped_pull_requests"])
    );
    assert_eq!(sync_issues["additionalProperties"], false);
    assert_eq!(
        schema_property_names(sync_issues),
        BTreeSet::from([
            "fetched".to_string(),
            "skipped_pull_requests".to_string(),
            "tombstoned".to_string(),
            "upserted".to_string(),
        ])
    );
    for field in ["fetched", "upserted", "skipped_pull_requests", "tombstoned"] {
        assert_eq!(sync_issues["properties"][field]["minimum"], 0);
    }
    assert_eq!(
        sync_schema["properties"]["sources"]["$ref"],
        "#/$defs/sources"
    );
    let sync_sources = &sync_schema["$defs"]["sources"];
    assert_eq!(
        sync_sources["required"],
        json!(["issue_count", "comment_count", "tombstone_count"])
    );
    assert_eq!(sync_sources["additionalProperties"], false);
    assert_eq!(
        schema_property_names(sync_sources),
        BTreeSet::from([
            "comment_count".to_string(),
            "issue_count".to_string(),
            "tombstone_count".to_string(),
        ])
    );
    assert_eq!(sync_sources["properties"]["issue_count"]["minimum"], 0);
    assert_eq!(sync_sources["properties"]["comment_count"]["minimum"], 0);
    assert_eq!(sync_sources["properties"]["tombstone_count"]["minimum"], 0);
    assert_eq!(
        sync_schema["properties"]["scheduler"]["$ref"],
        "#/$defs/scheduler"
    );
    let sync_scheduler = &sync_schema["$defs"]["scheduler"];
    assert_eq!(
        sync_scheduler["required"],
        json!(["max_in_flight_requests", "hard_cap"])
    );
    assert_eq!(sync_scheduler["additionalProperties"], false);
    assert_eq!(
        sync_scheduler["properties"]["max_in_flight_requests"]["minimum"],
        1
    );
    assert_eq!(
        sync_scheduler["properties"]["max_in_flight_requests"]["maximum"],
        16
    );
    assert_eq!(sync_scheduler["properties"]["hard_cap"]["const"], 16);
    assert_eq!(
        sync_schema["properties"]["target"]["$ref"],
        "#/$defs/target"
    );
    let sync_target = &sync_schema["$defs"]["target"];
    assert_eq!(
        sync_target["required"],
        json!(["kind", "repo", "issue_number"])
    );
    assert_eq!(sync_target["additionalProperties"], false);
    assert_eq!(sync_target["properties"]["kind"]["const"], "issue");
    assert_eq!(
        sync_target["properties"]["repo"]["pattern"],
        "^[^/]+/[^/]+$"
    );
    assert_eq!(sync_target["properties"]["issue_number"]["minimum"], 1);
    assert_eq!(
        sync_schema["properties"]["lifecycle"]["$ref"],
        "#/$defs/lifecycle"
    );
    let sync_lifecycle = &sync_schema["$defs"]["lifecycle"];
    assert_eq!(
        sync_lifecycle["required"],
        json!(["status", "reason", "http_status", "alias_chain"])
    );
    assert_eq!(sync_lifecycle["additionalProperties"], false);
    assert_eq!(sync_lifecycle["properties"]["status"]["type"], "string");
    assert_eq!(
        sync_lifecycle["properties"]["reason"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        sync_lifecycle["properties"]["http_status"]["type"],
        json!(["integer", "null"])
    );
    assert_eq!(sync_lifecycle["properties"]["http_status"]["minimum"], 100);
    assert_eq!(sync_lifecycle["properties"]["http_status"]["maximum"], 599);
    assert_eq!(
        sync_lifecycle["properties"]["alias_chain"]["items"]["type"],
        "string"
    );
    assert_eq!(
        status_schema["properties"]["privacy"]["$ref"],
        "#/$defs/privacy"
    );
    assert_eq!(
        status_schema["properties"]["github"]["$ref"],
        "#/$defs/github"
    );
    assert_eq!(
        status_schema["properties"]["paths"]["$ref"],
        "#/$defs/paths"
    );
    assert_eq!(
        status_schema["properties"]["sources"]["$ref"],
        "#/$defs/sources"
    );
    assert_eq!(
        status_schema["properties"]["database"]["$ref"],
        "#/$defs/database"
    );
    assert_eq!(
        status_schema["properties"]["index"]["$ref"],
        "#/$defs/index"
    );
    assert_eq!(status_schema["properties"]["sync"]["$ref"], "#/$defs/sync");
    assert_eq!(
        status_schema["properties"]["reconciliation"]["$ref"],
        "#/$defs/reconciliation"
    );
    let status_github = &status_schema["$defs"]["github"];
    assert_eq!(
        status_github["required"],
        json!(["host", "api_base_url", "web_base_url"])
    );
    assert_eq!(status_github["additionalProperties"], false);
    assert_eq!(
        schema_property_names(status_github),
        BTreeSet::from([
            "api_base_url".to_string(),
            "host".to_string(),
            "web_base_url".to_string(),
        ])
    );
    let status_paths = &status_schema["$defs"]["paths"];
    assert_eq!(
        status_paths["required"],
        json!([
            "config",
            "profile_data",
            "database",
            "tantivy_index",
            "cache",
            "logs"
        ])
    );
    assert_eq!(status_paths["additionalProperties"], false);
    assert_eq!(
        schema_property_names(status_paths),
        BTreeSet::from([
            "cache".to_string(),
            "config".to_string(),
            "database".to_string(),
            "logs".to_string(),
            "profile_data".to_string(),
            "tantivy_index".to_string(),
        ])
    );
    for field in [
        "cache",
        "config",
        "database",
        "logs",
        "profile_data",
        "tantivy_index",
    ] {
        assert_eq!(status_paths["properties"][field]["type"], "string");
    }
    let status_sources = &status_schema["$defs"]["sources"];
    assert_eq!(
        status_sources["required"],
        json!(["issue_count", "comment_count", "tombstone_count"])
    );
    assert_eq!(status_sources["additionalProperties"], false);
    assert_eq!(
        schema_property_names(status_sources),
        BTreeSet::from([
            "comment_count".to_string(),
            "issue_count".to_string(),
            "tombstone_count".to_string(),
        ])
    );
    let status_database = &status_schema["$defs"]["database"];
    assert_eq!(status_database["required"], json!(["schema_version"]));
    assert_eq!(status_database["additionalProperties"], false);
    assert_eq!(
        status_database["properties"]["schema_version"]["const"],
        "qgh.db.v1"
    );
    assert_eq!(
        schema_property_names(status_database),
        BTreeSet::from(["schema_version".to_string()])
    );
    let status_index = &status_schema["$defs"]["index"];
    assert_eq!(
        status_index["required"],
        json!(["active_generation", "dirty_task_count"])
    );
    assert_eq!(status_index["additionalProperties"], false);
    assert_eq!(
        schema_property_names(status_index),
        BTreeSet::from([
            "active_generation".to_string(),
            "dirty_task_count".to_string()
        ])
    );
    assert_eq!(
        status_index["properties"]["active_generation"]["minimum"],
        0
    );
    assert_eq!(status_index["properties"]["dirty_task_count"]["minimum"], 0);
    let status_sync = &status_schema["$defs"]["sync"];
    assert_eq!(
        status_sync["required"],
        json!(["last_sync_at", "cursors", "backoff", "scheduler"])
    );
    assert_eq!(status_sync["additionalProperties"], false);
    assert_eq!(
        schema_property_names(status_sync),
        BTreeSet::from([
            "backoff".to_string(),
            "cursors".to_string(),
            "last_sync_at".to_string(),
            "scheduler".to_string(),
        ])
    );
    assert_eq!(
        status_sync["properties"]["last_sync_at"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        status_sync["properties"]["cursors"]["additionalProperties"]["$ref"],
        "#/$defs/sync_cursor"
    );
    assert_eq!(
        status_sync["properties"]["scheduler"]["$ref"],
        "#/$defs/sync_scheduler"
    );
    let sync_cursor = &status_schema["$defs"]["sync_cursor"];
    assert_eq!(sync_cursor["required"], json!(["watermark", "has_etag"]));
    assert_eq!(sync_cursor["additionalProperties"], false);
    assert_eq!(
        sync_cursor["properties"]["watermark"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(sync_cursor["properties"]["has_etag"]["type"], "boolean");
    let sync_backoff = &status_schema["$defs"]["sync_backoff"];
    assert_eq!(
        status_sync["properties"]["backoff"]["anyOf"][0]["$ref"],
        "#/$defs/sync_backoff"
    );
    assert_eq!(
        status_sync["properties"]["backoff"]["anyOf"][1]["type"],
        "null"
    );
    assert_eq!(
        sync_backoff["required"],
        json!([
            "reason",
            "scope",
            "retry_after_seconds",
            "reset_at",
            "observed_at",
            "last_successful_sync"
        ])
    );
    assert_eq!(sync_backoff["additionalProperties"], false);
    assert_eq!(
        sync_backoff["properties"]["retry_after_seconds"]["minimum"],
        0
    );
    assert_eq!(
        sync_backoff["properties"]["reset_at"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        sync_backoff["properties"]["last_successful_sync"]["type"],
        json!(["string", "null"])
    );
    let sync_scheduler = &status_schema["$defs"]["sync_scheduler"];
    assert_eq!(
        sync_scheduler["required"],
        json!(["max_in_flight_requests", "hard_cap"])
    );
    assert_eq!(sync_scheduler["additionalProperties"], false);
    assert_eq!(
        sync_scheduler["properties"]["max_in_flight_requests"]["minimum"],
        1
    );
    assert_eq!(
        sync_scheduler["properties"]["max_in_flight_requests"]["maximum"],
        16
    );
    assert_eq!(sync_scheduler["properties"]["hard_cap"]["const"], 16);
    let status_reconciliation = &status_schema["$defs"]["reconciliation"];
    assert_eq!(
        status_reconciliation["required"],
        json!([
            "last_full_at",
            "age_days",
            "stale",
            "stale_warning",
            "estimated_api_cost_class",
            "last_checked_source_count",
            "last_tombstoned_count",
            "last_estimated_api_cost_class"
        ])
    );
    assert_eq!(status_reconciliation["additionalProperties"], false);
    assert_eq!(
        schema_property_names(status_reconciliation),
        BTreeSet::from([
            "age_days".to_string(),
            "estimated_api_cost_class".to_string(),
            "last_checked_source_count".to_string(),
            "last_estimated_api_cost_class".to_string(),
            "last_full_at".to_string(),
            "last_tombstoned_count".to_string(),
            "stale".to_string(),
            "stale_warning".to_string(),
        ])
    );
    assert_eq!(
        status_reconciliation["properties"]["last_full_at"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        status_reconciliation["properties"]["age_days"]["type"],
        json!(["integer", "null"])
    );
    assert_eq!(
        status_reconciliation["properties"]["age_days"]["minimum"],
        0
    );
    assert_eq!(
        status_reconciliation["properties"]["stale"]["type"],
        "boolean"
    );
    assert_eq!(
        status_reconciliation["properties"]["stale_warning"]["enum"],
        json!(["reconciliation.stale", null])
    );
    assert_eq!(
        status_reconciliation["properties"]["estimated_api_cost_class"]["$ref"],
        "#/$defs/api_cost_class"
    );
    assert_eq!(
        status_reconciliation["properties"]["last_checked_source_count"]["type"],
        json!(["integer", "null"])
    );
    assert_eq!(
        status_reconciliation["properties"]["last_checked_source_count"]["minimum"],
        0
    );
    assert_eq!(
        status_reconciliation["properties"]["last_tombstoned_count"]["type"],
        json!(["integer", "null"])
    );
    assert_eq!(
        status_reconciliation["properties"]["last_tombstoned_count"]["minimum"],
        0
    );
    assert_eq!(
        status_reconciliation["properties"]["last_estimated_api_cost_class"]["anyOf"][0]["$ref"],
        "#/$defs/api_cost_class"
    );
    assert_eq!(
        status_reconciliation["properties"]["last_estimated_api_cost_class"]["anyOf"][1]["type"],
        "null"
    );
    assert_eq!(
        status_schema["$defs"]["api_cost_class"]["enum"],
        json!(["none", "low", "medium", "high"])
    );
    let status_privacy = &status_schema["$defs"]["privacy"];
    assert_eq!(
        status_privacy["required"],
        json!([
            "classification",
            "default_network_egress",
            "hosted_provider_egress",
            "local_paths_may_contain_private_content",
            "single_user_permissions"
        ])
    );
    assert_eq!(status_privacy["additionalProperties"], false);
    assert_eq!(
        status_privacy["properties"]["classification"]["const"],
        "sensitive_derivative_data"
    );
    assert_eq!(
        status_privacy["properties"]["default_network_egress"]["const"],
        "configured_github_host_only"
    );
    assert_eq!(
        status_privacy["properties"]["hosted_provider_egress"]["const"],
        "disabled"
    );
    assert_eq!(
        status_privacy["properties"]["local_paths_may_contain_private_content"]["const"],
        true
    );
    assert_eq!(
        get_schema["oneOf"][0]["required"],
        json!(["profile_id", "source"])
    );
    assert_eq!(
        get_schema["oneOf"][1]["properties"]["summary"]["properties"]["batch_size_cap"]["const"],
        20
    );
    assert_eq!(
        get_schema["oneOf"][1]["properties"]["items"]["maxItems"],
        20
    );
    assert_eq!(
        get_schema["oneOf"][1]["properties"]["lifecycle_check_policy"]["properties"]
            ["verify_lifecycle"]["type"],
        "boolean"
    );
    assert_eq!(
        get_schema["oneOf"][1]["properties"]["lifecycle_check_policy"]["properties"]["mode"]
            ["enum"],
        json!(["not_requested", "sequential"])
    );
    let get_source = &get_schema["$defs"]["source"];
    assert_eq!(
        get_source["required"],
        json!([
            "source_id",
            "entity_type",
            "repo",
            "issue_number",
            "canonical_url",
            "body",
            "source_version",
            "lifecycle_check"
        ])
    );
    assert_eq!(get_source["additionalProperties"], false);
    assert_eq!(
        schema_property_names(get_source),
        BTreeSet::from([
            "author".to_string(),
            "body".to_string(),
            "canonical_url".to_string(),
            "entity_type".to_string(),
            "issue_number".to_string(),
            "lifecycle_check".to_string(),
            "parent_issue".to_string(),
            "repo".to_string(),
            "source_id".to_string(),
            "source_version".to_string(),
            "title".to_string(),
        ])
    );
    assert_eq!(
        get_source["properties"]["source_version"]["$ref"],
        "#/$defs/source_version"
    );
    assert_eq!(
        get_source["properties"]["lifecycle_check"]["$ref"],
        "#/$defs/lifecycle_check"
    );
    assert_eq!(
        get_source["properties"]["parent_issue"]["$ref"],
        "#/$defs/parent_issue"
    );
    let get_parent_issue = &get_schema["$defs"]["parent_issue"];
    assert_eq!(
        get_parent_issue["required"],
        json!(["source_id", "repo", "number", "title", "canonical_url"])
    );
    assert_eq!(get_parent_issue["additionalProperties"], false);
    assert_eq!(
        schema_property_names(get_parent_issue),
        BTreeSet::from([
            "canonical_url".to_string(),
            "number".to_string(),
            "repo".to_string(),
            "source_id".to_string(),
            "title".to_string(),
        ])
    );
    let get_lifecycle_check = &get_schema["$defs"]["lifecycle_check"];
    assert_eq!(
        get_lifecycle_check["required"],
        json!(["status", "remote_checked"])
    );
    assert_eq!(
        get_lifecycle_check["properties"]["status"]["enum"],
        json!(["active", "not_checked"])
    );
    assert_eq!(get_lifecycle_check["additionalProperties"], false);
    assert_eq!(
        schema_property_names(get_lifecycle_check),
        BTreeSet::from([
            "error_code".to_string(),
            "reason".to_string(),
            "remote_checked".to_string(),
            "status".to_string(),
        ])
    );
    let get_source_version = &get_schema["$defs"]["source_version"];
    assert_eq!(
        get_source_version["required"],
        json!([
            "body_hash",
            "github_updated_at",
            "indexed_at",
            "sync_run_id",
            "lifecycle_state"
        ])
    );
    assert_eq!(get_source_version["additionalProperties"], false);
    assert_eq!(
        schema_property_names(get_source_version),
        BTreeSet::from([
            "body_hash".to_string(),
            "github_updated_at".to_string(),
            "indexed_at".to_string(),
            "lifecycle_state".to_string(),
            "sync_run_id".to_string(),
        ])
    );
    let included = artifact["acceptance_snapshot"]["included_in_mvp_gate"]
        .as_array()
        .unwrap();
    assert!(included.iter().any(|id| id == "AC-28"));
    assert!(!included.iter().any(|id| id == "AC-13"));
    assert!(!included.iter().any(|id| id == "AC-20"));
    assert_eq!(
        artifact["acceptance_snapshot"]["excluded_from_mvp_gate"][0]["id"],
        "AC-13"
    );
    assert_eq!(
        artifact["acceptance_snapshot"]["excluded_from_mvp_gate"][1]["id"],
        "AC-20"
    );

    let checklist = fs::read_to_string(root.join("docs/release-checklist.md")).unwrap();
    for required in [
        "Tantivy BM25-only path",
        "strict schema/envelope",
        "human CLI summaries",
        "get batch output",
        "init output",
        "MCP adapter parity smoke",
        "stdout cleanliness",
        "privacy no-egress",
        "DB/index permissions",
        "doctor output",
        "search eval result",
        "Supported MVP token sources",
        "Product contract source of truth",
        "qgh query --json",
        "qgh init --yes",
        "qgh init -y",
        "validation.init_cancelled",
        "validation.batch_size",
        "qgh get <source_id>... --json",
        "Human output",
        "MCP role",
        "Wiki",
        "vector",
        "shared server",
        "write-back",
        "user-facing eval",
    ] {
        assert!(
            checklist.contains(required),
            "missing release checklist phrase: {required}"
        );
    }
    assert!(checklist.contains("credential_store"));
    assert!(checklist.contains("validation.invalid_token_source"));
}

fn qgh(args: &[&str]) -> Output {
    let mut cmd = Command::new(binary());
    cmd.args(args).output().unwrap()
}

fn mcp<const N: usize>(messages: [Value; N]) -> Output {
    let mut cmd = Command::new(binary());
    cmd.args(["--profile", "work", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        for message in messages {
            writeln!(stdin, "{}", serde_json::to_string(&message).unwrap()).unwrap();
        }
    }
    child.wait_with_output().unwrap()
}

fn schema_property_names(schema: &Value) -> BTreeSet<String> {
    schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

fn binary() -> String {
    std::env::var("CARGO_BIN_EXE_qgh").unwrap_or_else(|_| {
        let mut path = std::env::current_exe().unwrap();
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        path.push("qgh");
        path.to_string_lossy().into_owned()
    })
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success\nstatus: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        stdout_text(output),
        stderr_text(output)
    );
}

fn stdout_json_lines(output: &Output) -> Vec<Value> {
    stdout_text(output)
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!(
                    "stdout line was not JSON: {error}\nline:\n{line}\nstdout:\n{}\nstderr:\n{}",
                    stdout_text(output),
                    stderr_text(output)
                )
            })
        })
        .collect()
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
