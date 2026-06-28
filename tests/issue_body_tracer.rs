use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn sync_query_get_status_round_trips_issue_body_from_authoritative_store() {
    let fixture = TestFixture::new("round-trip");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);

    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    assert_eq!(sync_json["ok"], true);
    assert_eq!(sync_json["data"]["issues"]["upserted"], 1);
    assert_eq!(sync_json["data"]["issues"]["skipped_pull_requests"], 1);
    assert_eq!(sync_json["data"]["comments"]["upserted"], 1);
    assert_eq!(sync_json["data"]["index"]["dirty_task_count"], 0);
    fixture.assert_sqlite_issue_metadata();
    fixture.assert_sqlite_comment_metadata(1);

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["ok"], true);
    assert_eq!(status_json["data"]["profile_id"], "work");
    assert_eq!(status_json["data"]["sources"]["issue_count"], 1);
    assert_eq!(status_json["data"]["sources"]["comment_count"], 1);
    assert_eq!(
        status_json["data"]["database"]["schema_version"],
        "qgh.db.v1"
    );
    assert_eq!(status_json["data"]["index"]["active_generation"], 1);
    assert_eq!(status_json["data"]["index"]["dirty_task_count"], 0);
    assert!(status_json["data"]["sync"]["last_sync_at"]
        .as_str()
        .is_some());
    assert!(status_json["data"]["paths"]["logs"].as_str().is_some());
    assert_eq!(
        status_json["data"]["privacy"]["classification"],
        "sensitive_derivative_data"
    );
    assert_eq!(
        status_json["data"]["privacy"]["hosted_provider_egress"],
        "disabled"
    );
    assert_eq!(
        status_json["data"]["privacy"]["default_network_egress"],
        "configured_github_host_only"
    );
    assert_eq!(server.request_count(), 2, "status must be local-only");

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    let result = &query_json["data"]["results"][0];
    let source_id = "qgh://github.com/issue/I_kwDOISSUE1";
    assert_eq!(result["source_id"], source_id);
    assert_eq!(result["entity_type"], "issue");
    assert!(
        result.get("body").is_none(),
        "query results are source candidates; authoritative bodies must come from get"
    );
    assert_eq!(
        result["canonical_url"],
        "https://github.com/owner/repo/issues/42"
    );
    assert_eq!(result["get_args"]["source_id"], source_id);
    assert_eq!(result["ranking"]["kind"], "bm25");
    assert!(result["ranking"]["lexical_score"]
        .as_f64()
        .is_some_and(f64::is_finite));
    assert!(result["ranking"].get("confidence").is_none());
    assert!(result["ranking"].get("probability").is_none());
    assert_eq!(
        result["source_version"]["github_updated_at"],
        "2026-01-02T03:04:05Z"
    );
    assert!(
        result["source_version"]["body_hash"]
            .as_str()
            .unwrap()
            .len()
            >= 32
    );
    assert!(result["source_version"]["indexed_at"].as_str().is_some());
    assert!(result["snippet"]
        .as_str()
        .unwrap()
        .contains("BM25 issue body tracer"));

    let search_alias = fixture.qgh(["search", "BM25 tracer", "--json"]);
    assert_success(&search_alias);
    assert_eq!(
        stdout_json(&search_alias)["data"]["results"][0]["source_id"],
        source_id
    );

    let pr_query = fixture.qgh(["query", "Do not index PRs", "--json"]);
    assert_success(&pr_query);
    assert_eq!(
        stdout_json(&pr_query)["data"]["results"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "pull_request items from the Issues endpoint must not be indexed"
    );

    let issue_url_lookup =
        fixture.qgh(["query", "https://github.com/owner/repo/issues/42", "--json"]);
    assert_success(&issue_url_lookup);
    assert_eq!(
        stdout_json(&issue_url_lookup)["data"]["results"][0]["source_id"],
        source_id
    );

    let number_lookup = fixture.qgh(["query", "#42", "--repo", "owner/repo", "--json"]);
    assert_success(&number_lookup);
    assert_eq!(
        stdout_json(&number_lookup)["data"]["results"][0]["source_id"],
        source_id
    );

    let filtered_issue = fixture.qgh([
        "query",
        "BM25 tracer",
        "--repo",
        "owner/repo",
        "--label",
        "bug",
        "--state",
        "open",
        "--author",
        "bob",
        "--json",
    ]);
    assert_success(&filtered_issue);
    assert_eq!(
        stdout_json(&filtered_issue)["data"]["results"][0]["source_id"],
        source_id
    );

    let narrowed_out = fixture.qgh(["query", "BM25 tracer", "--label", "missing", "--json"]);
    assert_success(&narrowed_out);
    assert_eq!(
        stdout_json(&narrowed_out)["data"]["results"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let get = fixture.qgh(["get", source_id, "--json"]);
    assert_success(&get);
    let get_json = stdout_json(&get);
    let source = &get_json["data"]["source"];
    assert_query_result_round_trips_to_get_result(result, source);
    assert_eq!(source["source_id"], source_id);
    assert_eq!(source["entity_type"], "issue");
    assert_eq!(source["repo"], "owner/repo");
    assert_eq!(source["issue_number"], 42);
    assert_eq!(source["title"], "Cache sync bug");
    assert_eq!(
        source["canonical_url"],
        "https://github.com/owner/repo/issues/42"
    );
    assert!(source["body"]
        .as_str()
        .unwrap()
        .contains("BM25 issue body tracer"));
    assert_eq!(
        source["source_version"]["github_updated_at"],
        "2026-01-02T03:04:05Z"
    );

    let comment_query = fixture.qgh(["query", "comment-only mitigation", "--json"]);
    assert_success(&comment_query);
    let comment_json = stdout_json(&comment_query);
    let comment_result = &comment_json["data"]["results"][0];
    let comment_source_id = "qgh://github.com/issue-comment/IC_kwDOCOMMENT1";
    assert_eq!(comment_result["source_id"], comment_source_id);
    assert_eq!(comment_result["entity_type"], "issue_comment");
    assert!(
        comment_result.get("body").is_none(),
        "query results are source candidates; authoritative bodies must come from get"
    );
    assert_eq!(
        comment_result["canonical_url"],
        "https://github.com/owner/repo/issues/42#issuecomment-5001"
    );
    assert_eq!(comment_result["get_args"]["source_id"], comment_source_id);
    assert_eq!(comment_result["ranking"]["kind"], "bm25");
    assert!(comment_result["ranking"]["lexical_score"]
        .as_f64()
        .is_some_and(f64::is_finite));
    assert_eq!(
        comment_result["parent_issue"]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );
    assert_eq!(comment_result["parent_issue"]["repo"], "owner/repo");
    assert_eq!(comment_result["parent_issue"]["number"], 42);
    assert_eq!(comment_result["parent_issue"]["title"], "Cache sync bug");
    assert_eq!(
        comment_result["parent_issue"]["canonical_url"],
        "https://github.com/owner/repo/issues/42"
    );
    assert_eq!(
        comment_result["source_version"]["github_updated_at"],
        "2026-01-03T04:05:06Z"
    );
    assert!(comment_result["snippet"]
        .as_str()
        .unwrap()
        .contains("comment-only mitigation"));

    let comment_url_lookup = fixture.qgh([
        "query",
        "https://github.com/owner/repo/issues/42#issuecomment-5001",
        "--json",
    ]);
    assert_success(&comment_url_lookup);
    assert_eq!(
        stdout_json(&comment_url_lookup)["data"]["results"][0]["source_id"],
        comment_source_id
    );

    let filtered_comment = fixture.qgh([
        "query",
        "comment-only mitigation",
        "--repo",
        "owner/repo",
        "--author",
        "carol",
        "--json",
    ]);
    assert_success(&filtered_comment);
    assert_eq!(
        stdout_json(&filtered_comment)["data"]["results"][0]["source_id"],
        comment_source_id
    );

    let comment_get = fixture.qgh(["get", comment_source_id, "--json"]);
    assert_success(&comment_get);
    let comment_get_json = stdout_json(&comment_get);
    let comment_source = &comment_get_json["data"]["source"];
    assert_query_result_round_trips_to_get_result(comment_result, comment_source);
    assert_eq!(comment_source["source_id"], comment_source_id);
    assert_eq!(comment_source["entity_type"], "issue_comment");
    assert_eq!(comment_source["repo"], "owner/repo");
    assert_eq!(comment_source["issue_number"], 42);
    assert_eq!(
        comment_source["canonical_url"],
        "https://github.com/owner/repo/issues/42#issuecomment-5001"
    );
    assert!(comment_source["body"]
        .as_str()
        .unwrap()
        .contains("comment-only mitigation"));
    assert_eq!(
        comment_source["parent_issue"]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );
    assert_eq!(
        comment_source["source_version"]["github_updated_at"],
        "2026-01-03T04:05:06Z"
    );

    let second_sync = fixture.qgh(["sync", "--json"]);
    assert_success(&second_sync);
    fixture.assert_sqlite_comment_metadata(1);
    fixture.assert_private_local_data_permissions();
}

#[test]
fn exact_lookup_uses_typed_ranking_without_non_finite_scores() {
    let fixture = TestFixture::new("exact-ranking");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let issue_lookup = fixture.qgh(["query", "#42", "--repo", "owner/repo", "--json"]);
    assert_success(&issue_lookup);
    let issue_json = stdout_json(&issue_lookup);
    let issue_result = &issue_json["data"]["results"][0];
    assert_eq!(issue_result["ranking"]["kind"], "exact");
    assert!(issue_result["ranking"]["lexical_score"].is_null());
    assert!(issue_result["ranking"].get("confidence").is_none());
    assert!(issue_result["ranking"].get("probability").is_none());

    let comment_lookup = fixture.qgh([
        "query",
        "https://github.com/owner/repo/issues/42#issuecomment-5001",
        "--json",
    ]);
    assert_success(&comment_lookup);
    let comment_json = stdout_json(&comment_lookup);
    assert_eq!(
        comment_json["data"]["results"][0]["ranking"]["kind"],
        "exact"
    );
    assert!(comment_json["data"]["results"][0]["ranking"]["lexical_score"].is_null());
}

#[test]
fn query_filters_unresolvable_index_hits_before_returning_results() {
    let fixture = TestFixture::new("unresolvable-index-hit");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let source_id = "qgh://github.com/issue/I_kwDOISSUE1";
    fixture.mark_source_unavailable_without_reindex(source_id);

    let query = fixture.qgh(["query", "BM25 issue body tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    assert_eq!(query_json["data"]["results"].as_array().unwrap().len(), 0);
    assert_eq!(
        query_json["data"]["result_filtering"]["unresolvable_hits"],
        1
    );

    let get = fixture.qgh(["get", source_id, "--json"]);
    assert_eq!(get.status.code(), Some(4));
    assert_eq!(stdout_json(&get)["error"]["code"], "source.not_found");
}

#[test]
fn citation_contract_schema_and_docs_are_issue_comment_only() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let schema_text =
        fs::read_to_string(root.join("docs/schemas/query-result.schema.json")).unwrap();
    let schema_json: Value = serde_json::from_str(&schema_text).unwrap();
    let properties = &schema_json["$defs"]["query_result"]["properties"];
    for key in [
        "source_id",
        "entity_type",
        "canonical_url",
        "snippet",
        "get_args",
        "source_version",
        "ranking",
    ] {
        assert!(
            properties.get(key).is_some(),
            "query result schema must define {key}"
        );
    }
    assert!(properties.get("body").is_none());
    assert!(properties["snippet"]["description"]
        .as_str()
        .unwrap()
        .contains("preview, not citation evidence"));
    assert!(!schema_text.to_ascii_lowercase().contains("wiki"));

    let docs_text = fs::read_to_string(root.join("docs/cli-json-contract.md")).unwrap();
    assert!(docs_text.contains("snippet is a preview, not citation evidence"));
    assert!(docs_text.contains("Use the `get` response"));
    assert!(docs_text.contains("Citation example from a `get` response"));
    assert!(docs_text.contains("Query results intentionally omit `body`"));
    assert!(!docs_text.to_ascii_lowercase().contains("wiki"));
}

#[test]
fn full_reconciliation_tombstones_deleted_comments_and_updates_status() {
    let fixture = TestFixture::new("deleted-comment-reconciliation");
    let server = LifecycleFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    let comment_source_id = "qgh://github.com/issue-comment/IC_kwDOCOMMENT1";
    let comment_query = fixture.qgh(["query", "comment-only mitigation", "--json"]);
    assert_success(&comment_query);
    assert_eq!(
        stdout_json(&comment_query)["data"]["results"][0]["source_id"],
        comment_source_id
    );

    server.set_mode(LIFECYCLE_DELETED_COMMENT);
    let reconcile = fixture.qgh(["sync", "--reconcile", "full", "--json"]);
    assert_success(&reconcile);
    let reconcile_json = stdout_json(&reconcile);
    assert_eq!(reconcile_json["data"]["reconciliation"]["mode"], "full");
    assert_eq!(
        reconcile_json["data"]["reconciliation"]["tombstoned_sources"],
        1
    );
    assert_eq!(
        reconcile_json["data"]["reconciliation"]["estimated_api_cost_class"],
        "low"
    );

    let deleted_query = fixture.qgh(["query", "comment-only mitigation", "--json"]);
    assert_success(&deleted_query);
    let deleted_query_json = stdout_json(&deleted_query);
    assert_eq!(
        deleted_query_json["data"]["results"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let deleted_get = fixture.qgh(["get", comment_source_id, "--json"]);
    assert_eq!(deleted_get.status.code(), Some(4));
    let deleted_get_json = stdout_json(&deleted_get);
    assert_eq!(deleted_get_json["error"]["code"], "source.tombstoned");
    assert_eq!(
        deleted_get_json["error"]["details"]["source_id"],
        comment_source_id
    );
    assert_eq!(deleted_get_json["error"]["details"]["reason"], "not_found");

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["sources"]["tombstone_count"], 1);
    assert!(status_json["data"]["reconciliation"]["last_full_at"]
        .as_str()
        .is_some());
    assert!(status_json["data"]["reconciliation"]["age_days"]
        .as_i64()
        .is_some());
    assert_eq!(
        status_json["data"]["reconciliation"]["estimated_api_cost_class"],
        "low"
    );
    assert_eq!(status_json["data"]["reconciliation"]["stale"], false);
}

#[test]
fn get_tombstones_unavailable_issue_and_filters_active_query() {
    let fixture = TestFixture::new("get-unavailable-issue");
    let server = LifecycleFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    let issue_source_id = "qgh://github.com/issue/I_kwDOISSUE1";

    server.set_mode(LIFECYCLE_UNAVAILABLE_ISSUE);
    let get = fixture.qgh(["get", issue_source_id, "--json"]);
    assert_eq!(get.status.code(), Some(4));
    let get_json = stdout_json(&get);
    assert_eq!(get_json["error"]["code"], "source.tombstoned");
    assert_eq!(get_json["error"]["details"]["source_id"], issue_source_id);
    assert_eq!(get_json["error"]["details"]["reason"], "not_found");
    assert!(get_json["error"]["details"]["observed_at"]
        .as_str()
        .is_some());

    let query = fixture.qgh(["query", "BM25 issue body tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    assert_eq!(query_json["data"]["results"].as_array().unwrap().len(), 0);
    assert_eq!(
        query_json["data"]["result_filtering"]["unresolvable_hits"],
        1
    );

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    assert_eq!(
        stdout_json(&status)["data"]["sources"]["tombstone_count"],
        1
    );
}

#[test]
fn get_tombstones_moved_issue_as_structured_lifecycle_state() {
    let fixture = TestFixture::new("get-moved-issue");
    let server = LifecycleFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    let issue_source_id = "qgh://github.com/issue/I_kwDOISSUE1";

    server.set_mode(LIFECYCLE_MOVED_ISSUE);
    let get = fixture.qgh(["get", issue_source_id, "--json"]);
    assert_eq!(get.status.code(), Some(4));
    let get_json = stdout_json(&get);
    assert_eq!(get_json["error"]["code"], "source.tombstoned");
    assert_eq!(get_json["error"]["details"]["source_id"], issue_source_id);
    assert_eq!(get_json["error"]["details"]["reason"], "moved");
    assert_eq!(
        get_json["error"]["details"]["lifecycle_state"],
        "tombstoned"
    );
}

#[test]
fn status_warns_about_stale_reconciliation_without_running_it() {
    let fixture = TestFixture::new("stale-reconciliation-status");
    let server = LifecycleFakeGitHub::start();
    fixture.write_config_with_reconcile_after(&server.base_url, Some(0));

    assert_success(&fixture.qgh(["sync", "--json"]));
    let requests_before_status = server.request_count();
    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    assert_eq!(
        server.request_count(),
        requests_before_status,
        "status must not run hidden reconciliation network work"
    );
    let status_json = stdout_json(&status);
    assert_eq!(
        status_json["data"]["reconciliation"]["last_full_at"],
        Value::Null
    );
    assert_eq!(
        status_json["data"]["reconciliation"]["age_days"],
        Value::Null
    );
    assert_eq!(status_json["data"]["reconciliation"]["stale"], true);
    assert_eq!(
        status_json["data"]["reconciliation"]["stale_warning"],
        "reconciliation.stale"
    );
    assert_eq!(
        status_json["data"]["reconciliation"]["estimated_api_cost_class"],
        "low"
    );
}

#[test]
fn sync_records_primary_rate_limit_backoff_and_local_reads_continue() {
    let fixture = TestFixture::new("primary-rate-limit");
    let server = RateLimitFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    server.set_mode(RATE_LIMIT_PRIMARY);

    let limited_sync = fixture.qgh(["sync", "--json"]);
    assert_success(&limited_sync);
    let limited_json = stdout_json(&limited_sync);
    assert_eq!(limited_json["data"]["sync_state"], "backoff");
    assert_eq!(
        limited_json["data"]["backoff"]["reason"],
        "primary_rate_limit"
    );
    assert_eq!(
        limited_json["data"]["backoff"]["scope"],
        "issues:owner/repo"
    );
    assert_eq!(limited_json["data"]["backoff"]["retry_after_seconds"], 0);
    assert!(limited_json["data"]["backoff"]["reset_at"]
        .as_str()
        .is_some());
    assert!(limited_json["data"]["backoff"]["last_successful_sync"]
        .as_str()
        .is_some());

    let local_query = fixture.qgh(["query", "BM25 issue body tracer", "--json"]);
    assert_success(&local_query);
    assert_eq!(
        stdout_json(&local_query)["data"]["results"][0]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );
    let local_get = fixture.qgh(["get", "qgh://github.com/issue/I_kwDOISSUE1", "--json"]);
    assert_success(&local_get);
    assert_eq!(
        stdout_json(&local_get)["data"]["source"]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(
        status_json["data"]["sync"]["backoff"]["reason"],
        "primary_rate_limit"
    );
    assert_eq!(
        status_json["data"]["sync"]["backoff"]["scope"],
        "issues:owner/repo"
    );
    assert!(status_json["data"]["sync"]["last_sync_at"]
        .as_str()
        .is_some());
}

#[test]
fn sync_records_secondary_rate_limit_retry_after_without_generic_failure() {
    let fixture = TestFixture::new("secondary-rate-limit");
    let server = RateLimitFakeGitHub::start();
    server.set_mode(RATE_LIMIT_SECONDARY);
    fixture.write_config(&server.base_url);

    let limited_sync = fixture.qgh(["sync", "--json"]);
    assert_success(&limited_sync);
    let limited_json = stdout_json(&limited_sync);
    assert_eq!(limited_json["data"]["sync_state"], "backoff");
    assert_eq!(
        limited_json["data"]["backoff"]["reason"],
        "secondary_rate_limit"
    );
    assert_eq!(
        limited_json["data"]["backoff"]["scope"],
        "issues:owner/repo"
    );
    assert_eq!(limited_json["data"]["backoff"]["retry_after_seconds"], 0);
    assert_eq!(
        limited_json["data"]["backoff"]["last_successful_sync"],
        Value::Null
    );

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(
        status_json["data"]["sync"]["backoff"]["reason"],
        "secondary_rate_limit"
    );
    assert_eq!(status_json["data"]["sources"]["issue_count"], 0);
}

#[test]
fn doctor_runs_explicit_checks_and_reports_cli_only_scope() {
    let fixture = TestFixture::new("doctor");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let before_doctor = server.request_count();
    let doctor = fixture.qgh(["doctor", "--json"]);
    assert_success(&doctor);
    assert!(
        server.request_count() > before_doctor,
        "doctor is the explicit command allowed to probe GitHub"
    );
    let doctor_json = stdout_json(&doctor);
    let checks = doctor_json["data"]["checks"].as_array().unwrap();
    for expected in [
        "config",
        "file_permissions",
        "sqlite",
        "tantivy",
        "github_auth_reachability",
        "rate_limit_headers",
    ] {
        assert!(
            checks
                .iter()
                .any(|check| check["name"] == expected && check["ok"] == true),
            "missing successful doctor check {expected}: {doctor_json:#}"
        );
    }
    assert_eq!(doctor_json["data"]["mcp"]["doctor_exposed"], false);
    assert_eq!(
        doctor_json["data"]["mcp"]["tools"],
        json!(["query", "get", "status"])
    );
}

#[test]
fn mcp_lists_only_read_only_query_get_status_tools_with_strict_schemas() {
    let fixture = TestFixture::new("mcp-tools");
    fixture.write_config("http://127.0.0.1:1");

    let output = fixture.mcp([
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {
                    "name": "qgh-test",
                    "version": "0"
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "query": "anything",
                    "bogus": true
                }
            }
        }),
    ]);
    assert_success(&output);
    assert!(stderr_text(&output).is_empty());
    let messages = stdout_json_lines(&output);
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["id"], 1);
    assert_eq!(messages[0]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(
        messages[0]["result"]["capabilities"]["tools"]["listChanged"],
        false
    );

    let tools = messages[1]["result"]["tools"].as_array().unwrap();
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["query", "get", "status"]);
    for forbidden in [
        "sync", "doctor", "eval", "write", "embed", "delete", "update",
    ] {
        assert!(!names.contains(&forbidden));
    }
    for tool in tools {
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert_eq!(tool["outputSchema"]["type"], "object");
        assert!(tool["outputSchema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("schema_version")));
    }

    let validation = &messages[2]["result"];
    assert_eq!(validation["isError"], true);
    assert_eq!(
        validation["structuredContent"]["error"]["code"],
        "validation.mcp"
    );
    assert_eq!(validation["structuredContent"]["schema_version"], "qgh.v1");
    assert!(validation["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("validation.mcp"));
}

#[test]
fn mcp_query_get_status_round_trips_issue_and_comment_sources() {
    let fixture = TestFixture::new("mcp-workflow");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let issue_source_id = "qgh://github.com/issue/I_kwDOISSUE1";
    let comment_source_id = "qgh://github.com/issue-comment/IC_kwDOCOMMENT1";
    let output = fixture.mcp([
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {
                    "name": "qgh-test",
                    "version": "0"
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "query": "BM25 issue body tracer",
                    "limit": 10
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "get",
                "arguments": {
                    "source_id": issue_source_id
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "query": "comment-only mitigation",
                    "limit": 10
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "get",
                "arguments": {
                    "source_id": comment_source_id
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "status",
                "arguments": {}
            }
        }),
    ]);
    assert_success(&output);
    assert!(stderr_text(&output).is_empty());
    let messages = stdout_json_lines(&output);
    assert_eq!(messages.len(), 6);
    for message in &messages {
        assert!(
            message.get("error").is_none(),
            "unexpected MCP error: {message}"
        );
    }

    let issue_query = &messages[1]["result"]["structuredContent"];
    assert_eq!(issue_query["ok"], true);
    assert_eq!(
        issue_query["data"]["results"][0]["source_id"],
        issue_source_id
    );
    assert!(messages[1]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains(issue_source_id));

    let issue_get = &messages[2]["result"]["structuredContent"];
    assert_eq!(issue_get["ok"], true);
    assert_eq!(issue_get["data"]["source"]["source_id"], issue_source_id);
    assert!(issue_get["data"]["source"]["body"]
        .as_str()
        .unwrap()
        .contains("BM25 issue body tracer"));

    let comment_query = &messages[3]["result"]["structuredContent"];
    assert_eq!(comment_query["ok"], true);
    assert_eq!(
        comment_query["data"]["results"][0]["source_id"],
        comment_source_id
    );

    let comment_get = &messages[4]["result"]["structuredContent"];
    assert_eq!(comment_get["ok"], true);
    assert_eq!(
        comment_get["data"]["source"]["source_id"],
        comment_source_id
    );
    assert_eq!(
        comment_get["data"]["source"]["canonical_url"],
        "https://github.com/owner/repo/issues/42#issuecomment-5001"
    );
    assert!(comment_get["data"]["source"]["body"]
        .as_str()
        .unwrap()
        .contains("comment-only mitigation"));

    let status = &messages[5]["result"]["structuredContent"];
    assert_eq!(status["ok"], true);
    assert_eq!(status["data"]["profile_id"], "work");
    assert_eq!(status["data"]["sources"]["issue_count"], 1);
    assert_eq!(status["data"]["sources"]["comment_count"], 1);
}

#[test]
fn sqlite_and_tantivy_publish_state_are_concurrency_hardened() {
    let fixture = TestFixture::new("concurrency-state");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let db_path = fixture.data_home.join("qgh/profiles/work/qgh.sqlite3");
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    let busy_timeout_ms: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    assert!(busy_timeout_ms >= 5_000);
    let migration_record_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM schema_migrations WHERE version = 'qgh.db.v1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(migration_record_count, 1);

    let (generation, active_path): (i64, String) = conn
        .query_row(
            "SELECT generation, path FROM index_generations WHERE active = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(generation, 1);
    assert!(active_path.contains("generation-1"));
    assert!(!active_path.ends_with("/active"));
    assert!(PathBuf::from(active_path).exists());
}

#[test]
fn concurrent_cli_sync_and_mcp_reads_keep_index_queryable() {
    let fixture = TestFixture::new("concurrent-sync-mcp");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let mut sync_cmd = fixture.base_command();
    sync_cmd
        .args(["--profile", "work", "sync", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let sync_child = sync_cmd.spawn().unwrap();

    let mcp = fixture.mcp([
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "qgh-test", "version": "0"}
            }
        }),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "query": "BM25 issue body tracer",
                    "limit": 5
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "status",
                "arguments": {}
            }
        }),
    ]);
    let sync = sync_child.wait_with_output().unwrap();
    assert_success(&sync);
    assert_success(&mcp);

    let messages = stdout_json_lines(&mcp);
    let query = &messages[1]["result"]["structuredContent"];
    assert_eq!(query["ok"], true);
    assert_eq!(
        query["data"]["results"][0]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );
    let status = &messages[2]["result"]["structuredContent"];
    assert_eq!(status["ok"], true);
    assert!(
        status["data"]["index"]["active_generation"]
            .as_i64()
            .unwrap()
            >= 1
    );

    let final_query = fixture.qgh(["query", "BM25 issue body tracer", "--json"]);
    assert_success(&final_query);
    assert_eq!(
        stdout_json(&final_query)["data"]["results"][0]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );
}

#[test]
fn privacy_docs_describe_sensitive_derivative_data_paths() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let docs = fs::read_to_string(root.join("docs/privacy.md")).unwrap();
    assert!(docs.contains("Sensitive Derivative Data"));
    assert!(docs.contains("SQLite"));
    assert!(docs.contains("Tantivy"));
    assert!(docs.contains("logs"));
    assert!(docs.contains("cache"));
    assert!(docs
        .to_ascii_lowercase()
        .contains("hosted provider paths are disabled"));
}

#[test]
fn schema_snapshots_define_envelope_outputs_and_error_taxonomy() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let schema_dir = root.join("docs/schemas");
    for file in [
        "envelope.schema.json",
        "error.schema.json",
        "query-result.schema.json",
        "sync-output.schema.json",
        "get-output.schema.json",
        "status-output.schema.json",
        "doctor-output.schema.json",
    ] {
        let text = fs::read_to_string(schema_dir.join(file)).unwrap();
        let json: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            json["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
    }

    let envelope: Value =
        serde_json::from_str(&fs::read_to_string(schema_dir.join("envelope.schema.json")).unwrap())
            .unwrap();
    let required = envelope["required"].as_array().unwrap();
    for field in ["schema_version", "ok", "warnings", "meta"] {
        assert!(required.iter().any(|value| value == field));
    }

    let error_schema: Value =
        serde_json::from_str(&fs::read_to_string(schema_dir.join("error.schema.json")).unwrap())
            .unwrap();
    let codes = error_schema["$defs"]["error_code"]["enum"]
        .as_array()
        .unwrap();
    for code in [
        "config.invalid",
        "validation.cli",
        "validation.mcp",
        "auth.token_unavailable",
        "github.request_failed",
        "source.not_found",
        "source.tombstoned",
        "storage.failure",
        "index.failure",
    ] {
        assert!(
            codes.iter().any(|value| value == code),
            "missing error code {code}"
        );
    }

    let docs = fs::read_to_string(root.join("docs/error-codes.md")).unwrap();
    assert!(docs.contains("No-result query responses are successful"));
    assert!(docs.contains("validation.cli"));
    assert!(docs.contains("source.tombstoned"));

    let cli_contract = fs::read_to_string(root.join("docs/cli-json-contract.md")).unwrap();
    assert!(cli_contract.contains("qgh.v1"));
    assert!(cli_contract.contains("docs/schemas/envelope.schema.json"));
    assert!(cli_contract.contains("docs/schemas/error.schema.json"));
}

#[test]
fn query_filter_errors_are_versioned_json_envelopes() {
    let fixture = TestFixture::new("filter-errors");
    fixture.write_config("http://127.0.0.1:1");

    let invalid_state = fixture.qgh(["query", "anything", "--state", "merged", "--json"]);
    assert_eq!(invalid_state.status.code(), Some(2));
    assert_eq!(
        stdout_json(&invalid_state)["error"]["code"],
        "validation.invalid_state"
    );

    let malformed_repo = fixture.qgh(["query", "anything", "--repo", "owner", "--json"]);
    assert_eq!(malformed_repo.status.code(), Some(2));
    assert_eq!(
        stdout_json(&malformed_repo)["error"]["code"],
        "validation.invalid_repo"
    );

    let wiki_filter = fixture.qgh(["query", "anything", "--wiki", "Home", "--json"]);
    assert_eq!(wiki_filter.status.code(), Some(2));
    assert_eq!(
        stdout_json(&wiki_filter)["error"]["code"],
        "validation.unsupported_filter"
    );

    let unknown_flag = fixture.qgh(["query", "anything", "--bogus", "--json"]);
    assert_eq!(unknown_flag.status.code(), Some(2));
    assert_eq!(
        stdout_json(&unknown_flag)["error"]["code"],
        "validation.cli"
    );

    let invalid_reconcile = fixture.qgh(["sync", "--reconcile", "bogus", "--json"]);
    assert_eq!(invalid_reconcile.status.code(), Some(2));
    assert_eq!(
        stdout_json(&invalid_reconcile)["error"]["code"],
        "validation.cli"
    );

    let human_invalid_state = fixture.qgh(["query", "anything", "--state", "merged"]);
    assert_eq!(human_invalid_state.status.code(), Some(2));
    assert!(stdout_text(&human_invalid_state).is_empty());
    assert!(stderr_text(&human_invalid_state).contains("validation.invalid_state"));
}

#[test]
fn missing_profile_is_a_structured_usage_error() {
    let fixture = TestFixture::new("missing-profile");
    let output = fixture.qgh_without_profile(["status", "--json"]);
    assert_eq!(output.status.code(), Some(2));

    let json = stdout_json(&output);
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "config.missing_profile");
    assert_eq!(json["error"]["exit_code"], 2);
    assert!(stderr_text(&output).is_empty());
}

#[test]
fn incremental_sync_records_new_versions_and_uses_since_overlap_and_etag() {
    let fixture = TestFixture::new("incremental-edit");
    let server = EditingFakeGitHub::start();
    fixture.write_config(&server.base_url);

    let first_sync = fixture.qgh(["sync", "--json"]);
    assert_success(&first_sync);
    let issue_id = "qgh://github.com/issue/I_kwDOISSUE1";
    let comment_id = "qgh://github.com/issue-comment/IC_kwDOCOMMENT1";
    let first_issue = stdout_json(&fixture.qgh(["get", issue_id, "--json"]));
    let first_issue_version = first_issue["data"]["source"]["source_version"].clone();
    assert_eq!(
        first_issue_version["github_updated_at"],
        "2026-01-02T03:04:05Z"
    );
    assert_eq!(first_issue_version["lifecycle_state"], "active");
    assert!(first_issue_version["sync_run_id"].as_str().is_some());

    server.set_mode(2);
    let second_sync = fixture.qgh(["sync", "--json"]);
    assert_success(&second_sync);
    let second_sync_json = stdout_json(&second_sync);
    assert_eq!(second_sync_json["data"]["issues"]["upserted"], 1);
    assert_eq!(second_sync_json["data"]["comments"]["upserted"], 1);
    assert_eq!(
        second_sync_json["data"]["cursors"]["not_modified_endpoints"],
        0
    );
    assert_eq!(
        second_sync_json["data"]["cursors"]["watermarks"]["issues:owner/repo"],
        "2026-01-04T00:00:00Z"
    );

    let requests = server.requests();
    assert!(
        requests.iter().any(|request| request.contains(
            "GET /repos/owner/repo/issues?state=all&sort=updated&direction=asc&per_page=100&since=2026-01-02T03%3A03%3A05Z"
        )),
        "second issue sync must use the previous issue watermark with a 60-second overlap: {requests:#?}"
    );
    assert!(
        requests.iter().any(|request| request
            .to_ascii_lowercase()
            .contains("if-none-match: \"issues-v1\"")),
        "second issue sync must send the stored issue ETag: {requests:#?}"
    );

    let edited_issue = stdout_json(&fixture.qgh(["get", issue_id, "--json"]));
    let edited_issue_source = &edited_issue["data"]["source"];
    assert!(edited_issue_source["body"]
        .as_str()
        .unwrap()
        .contains("updated issue body"));
    assert_eq!(edited_issue_source["title"], "Cache sync bug updated");
    assert_eq!(
        edited_issue_source["source_version"]["github_updated_at"],
        "2026-01-04T00:00:00Z"
    );
    assert_ne!(
        edited_issue_source["source_version"]["body_hash"],
        first_issue_version["body_hash"]
    );
    assert_ne!(
        edited_issue_source["source_version"]["sync_run_id"],
        first_issue_version["sync_run_id"]
    );
    assert_eq!(
        edited_issue_source["source_version"]["lifecycle_state"],
        "active"
    );
    let updated_query = stdout_json(&fixture.qgh(["query", "updated issue body", "--json"]));
    assert_eq!(updated_query["data"]["results"][0]["source_id"], issue_id);
    let old_query =
        stdout_json(&fixture.qgh(["query", "round-trip through get before citation", "--json"]));
    assert_eq!(old_query["data"]["results"].as_array().unwrap().len(), 0);

    let edited_comment = stdout_json(&fixture.qgh(["get", comment_id, "--json"]));
    let edited_comment_source = &edited_comment["data"]["source"];
    assert!(edited_comment_source["body"]
        .as_str()
        .unwrap()
        .contains("updated comment body"));
    assert_eq!(
        edited_comment_source["source_version"]["github_updated_at"],
        "2026-01-04T00:01:00Z"
    );
    fixture.assert_source_version_count(issue_id, 2);
    fixture.assert_source_version_count(comment_id, 2);

    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(
        status["data"]["sync"]["cursors"]["issues:owner/repo"]["watermark"],
        "2026-01-04T00:00:00Z"
    );
    assert_eq!(
        status["data"]["sync"]["cursors"]["comments:owner/repo#42"]["watermark"],
        "2026-01-04T00:01:00Z"
    );

    let third_sync = fixture.qgh(["sync", "--json"]);
    assert_success(&third_sync);
    let third_sync_json = stdout_json(&third_sync);
    assert_eq!(
        third_sync_json["data"]["cursors"]["not_modified_endpoints"],
        1
    );
    fixture.assert_source_version_count(issue_id, 2);
    fixture.assert_source_version_count(comment_id, 2);
}

fn issue_payload_with_pr() -> &'static str {
    r#"[
      {
        "id": 1001,
        "node_id": "I_kwDOISSUE1",
        "number": 42,
        "title": "Cache sync bug",
        "body": "The BM25 issue body tracer must round-trip through get before citation.",
        "state": "open",
        "locked": false,
        "comments": 1,
        "html_url": "https://github.com/owner/repo/issues/42",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:05Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}, {"name": "mvp"}],
        "milestone": {"title": "MVP"},
        "assignees": [{"login": "alice"}]
      },
      {
        "id": 2002,
        "node_id": "PR_kwDOPR1",
        "number": 43,
        "title": "Do not index PRs",
        "body": "This PR comes from the Issues endpoint but is out of MVP scope.",
        "state": "open",
        "comments": 0,
        "html_url": "https://github.com/owner/repo/pull/43",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T00:00:00Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [],
        "milestone": null,
        "assignees": [],
        "pull_request": {"url": "https://api.github.com/repos/owner/repo/pulls/43"}
      }
    ]"#
}

fn issue_comments_payload() -> &'static str {
    r#"[
      {
        "id": 5001,
        "node_id": "IC_kwDOCOMMENT1",
        "body": "The answer lives in this comment-only mitigation note.",
        "html_url": "https://github.com/owner/repo/issues/42#issuecomment-5001",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-03T04:05:06Z",
        "user": {"login": "carol"}
      }
    ]"#
}

fn issue_object_payload() -> &'static str {
    r#"{
        "id": 1001,
        "node_id": "I_kwDOISSUE1",
        "number": 42,
        "title": "Cache sync bug",
        "body": "The BM25 issue body tracer must round-trip through get before citation.",
        "state": "open",
        "locked": false,
        "comments": 1,
        "html_url": "https://github.com/owner/repo/issues/42",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:05Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}, {"name": "mvp"}],
        "milestone": {"title": "MVP"},
        "assignees": [{"login": "alice"}]
    }"#
}

fn issue_comment_object_payload() -> &'static str {
    r#"{
        "id": 5001,
        "node_id": "IC_kwDOCOMMENT1",
        "body": "The answer lives in this comment-only mitigation note.",
        "html_url": "https://github.com/owner/repo/issues/42#issuecomment-5001",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-03T04:05:06Z",
        "user": {"login": "carol"}
    }"#
}

fn rate_limit_payload() -> &'static str {
    r#"{"resources":{"core":{"limit":5000,"remaining":4999,"reset":0}}}"#
}

struct TestFixture {
    root: PathBuf,
    config_home: PathBuf,
    data_home: PathBuf,
    cache_home: PathBuf,
}

impl TestFixture {
    fn new(name: &str) -> Self {
        let root = unique_temp_dir(name);
        let config_home = root.join("config");
        let data_home = root.join("data");
        let cache_home = root.join("cache");
        fs::create_dir_all(config_home.join("qgh")).unwrap();
        fs::create_dir_all(&data_home).unwrap();
        fs::create_dir_all(&cache_home).unwrap();
        Self {
            root,
            config_home,
            data_home,
            cache_home,
        }
    }

    fn write_config(&self, api_base_url: &str) {
        self.write_config_with_reconcile_after(api_base_url, None);
    }

    fn write_config_with_reconcile_after(
        &self,
        api_base_url: &str,
        reconcile_after_days: Option<i64>,
    ) {
        let reconcile_after_days = reconcile_after_days
            .map(|days| format!("reconcile_after_days = {days}\n"))
            .unwrap_or_default();
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["owner/repo"]
{reconcile_after_days}

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn qgh<const N: usize>(&self, args: [&str; N]) -> Output {
        let mut cmd = self.base_command();
        cmd.args(["--profile", "work"]).args(args);
        cmd.output().unwrap()
    }

    fn qgh_without_profile<const N: usize>(&self, args: [&str; N]) -> Output {
        let mut cmd = self.base_command();
        cmd.args(args);
        cmd.output().unwrap()
    }

    fn mcp<const N: usize>(&self, messages: [Value; N]) -> Output {
        let mut cmd = self.base_command();
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

    fn base_command(&self) -> Command {
        let binary = std::env::var("CARGO_BIN_EXE_qgh").unwrap_or_else(|_| {
            let mut path = std::env::current_exe().unwrap();
            path.pop();
            if path.ends_with("deps") {
                path.pop();
            }
            path.push("qgh");
            path.to_string_lossy().into_owned()
        });
        let mut cmd = Command::new(binary);
        cmd.env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_DATA_HOME", &self.data_home)
            .env("XDG_CACHE_HOME", &self.cache_home)
            .env("QGH_TEST_TOKEN", "fixture-token")
            .env_remove("RUST_LOG");
        cmd
    }

    fn assert_sqlite_issue_metadata(&self) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let source_id: String = conn
            .query_row(
                "SELECT source_id FROM source_entities WHERE entity_type = 'issue'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source_id, "qgh://github.com/issue/I_kwDOISSUE1");

        let version_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM source_versions WHERE source_id = ?1",
                [&source_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version_count, 1);

        let canonical_alias_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM source_aliases WHERE source_id = ?1 AND alias_type = 'canonical_url' AND alias_value = 'https://github.com/owner/repo/issues/42'",
                [&source_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(canonical_alias_count, 1);

        let body: String = conn
            .query_row(
                "SELECT body FROM issue_metadata WHERE source_id = ?1",
                [&source_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(body.contains("BM25 issue body tracer"));
    }

    fn assert_sqlite_comment_metadata(&self, expected_version_count: i64) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let source_id: String = conn
            .query_row(
                "SELECT source_id FROM source_entities WHERE entity_type = 'issue_comment'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source_id, "qgh://github.com/issue-comment/IC_kwDOCOMMENT1");

        let version_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM source_versions WHERE source_id = ?1",
                [&source_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version_count, expected_version_count);

        let comment: (String, String, i64, String, String, String) = conn
            .query_row(
                "SELECT body, parent_issue_source_id, issue_number, parent_issue_title, parent_issue_canonical_url, canonical_url FROM comment_metadata WHERE source_id = ?1",
                [&source_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert!(comment.0.contains("comment-only mitigation"));
        assert_eq!(comment.1, "qgh://github.com/issue/I_kwDOISSUE1");
        assert_eq!(comment.2, 42);
        assert_eq!(comment.3, "Cache sync bug");
        assert_eq!(comment.4, "https://github.com/owner/repo/issues/42");
        assert_eq!(
            comment.5,
            "https://github.com/owner/repo/issues/42#issuecomment-5001"
        );
    }

    fn assert_source_version_count(&self, source_id: &str, expected: i64) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM source_versions WHERE source_id = ?1",
                [source_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, expected);
    }

    fn assert_private_local_data_permissions(&self) {
        #[cfg(unix)]
        {
            let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let active_index_path: String = conn
                .query_row(
                    "SELECT path FROM index_generations WHERE active = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            for path in [
                self.data_home.join("qgh/profiles/work"),
                db_path,
                PathBuf::from(active_index_path),
                self.cache_home.join("qgh"),
                self.cache_home.join("qgh/logs"),
            ] {
                let mode = fs::metadata(&path).unwrap().permissions().mode();
                assert_eq!(
                    mode & 0o077,
                    0,
                    "{} must not be readable or writable by group/other",
                    path.display()
                );
            }
        }
    }

    fn mark_source_unavailable_without_reindex(&self, source_id: &str) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let changed = conn
            .execute(
                "UPDATE source_entities SET lifecycle_state = 'unavailable' WHERE source_id = ?1",
                [source_id],
            )
            .unwrap();
        assert_eq!(changed, 1);
    }
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct FakeGitHub {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl FakeGitHub {
    fn start(issue_payload: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(stream) => handle_connection(stream, issue_payload, &thread_requests),
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for FakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    issue_payload: &'static str,
    requests: &Arc<Mutex<Vec<String>>>,
) {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().unwrap_or("").to_string();
    requests.lock().unwrap().push(request_line.clone());

    let body = if request_line.starts_with("GET /repos/owner/repo/issues?")
        && request_line.contains("state=all")
        && request_line.contains("sort=updated")
        && request_line.contains("direction=asc")
        && request_line.contains("per_page=100")
    {
        issue_payload
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42/comments?")
        && request_line.contains("per_page=100")
    {
        issue_comments_payload()
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42 ") {
        issue_object_payload()
    } else if request_line.starts_with("GET /repos/owner/repo/issues/comments/5001 ") {
        issue_comment_object_payload()
    } else if request_line.starts_with("GET /rate_limit ") {
        rate_limit_payload()
    } else {
        r#"{"message":"not found"}"#
    };
    let status = if body == issue_payload
        || body == issue_comments_payload()
        || body == issue_object_payload()
        || body == issue_comment_object_payload()
        || body == rate_limit_payload()
    {
        "200 OK"
    } else {
        "404 Not Found"
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\nx-ratelimit-remaining: 4999\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

const LIFECYCLE_ACTIVE: usize = 1;
const LIFECYCLE_DELETED_COMMENT: usize = 2;
const LIFECYCLE_UNAVAILABLE_ISSUE: usize = 3;
const LIFECYCLE_MOVED_ISSUE: usize = 4;

struct LifecycleFakeGitHub {
    base_url: String,
    mode: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl LifecycleFakeGitHub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);
        let mode = Arc::new(AtomicUsize::new(LIFECYCLE_ACTIVE));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_mode = Arc::clone(&mode);
        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(stream) => {
                        handle_lifecycle_connection(stream, &thread_mode, &thread_requests)
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url,
            mode,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn set_mode(&self, mode: usize) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for LifecycleFakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_lifecycle_connection(
    mut stream: TcpStream,
    mode: &Arc<AtomicUsize>,
    requests: &Arc<Mutex<Vec<String>>>,
) {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().unwrap_or("").to_string();
    requests.lock().unwrap().push(request_line.clone());
    let mode = mode.load(Ordering::SeqCst);

    let (status, body) = if request_line.starts_with("GET /repos/owner/repo/issues?")
        && request_line.contains("state=all")
        && request_line.contains("per_page=100")
    {
        ("200 OK", issue_payload_with_pr())
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42/comments?")
        && request_line.contains("per_page=100")
    {
        if mode == LIFECYCLE_DELETED_COMMENT {
            ("200 OK", "[]")
        } else {
            ("200 OK", issue_comments_payload())
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42 ") {
        if mode == LIFECYCLE_UNAVAILABLE_ISSUE {
            ("404 Not Found", r#"{"message":"not found"}"#)
        } else if mode == LIFECYCLE_MOVED_ISSUE {
            ("301 Moved Permanently", r#"{"message":"moved"}"#)
        } else {
            ("200 OK", issue_object_payload())
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/comments/5001 ") {
        if mode == LIFECYCLE_DELETED_COMMENT {
            ("404 Not Found", r#"{"message":"not found"}"#)
        } else {
            ("200 OK", issue_comment_object_payload())
        }
    } else {
        ("404 Not Found", r#"{"message":"not found"}"#)
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\nx-ratelimit-remaining: 4999\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

const RATE_LIMIT_ACTIVE: usize = 1;
const RATE_LIMIT_PRIMARY: usize = 2;
const RATE_LIMIT_SECONDARY: usize = 3;

struct RateLimitFakeGitHub {
    base_url: String,
    mode: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl RateLimitFakeGitHub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);
        let mode = Arc::new(AtomicUsize::new(RATE_LIMIT_ACTIVE));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_mode = Arc::clone(&mode);
        let thread_stop = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(stream) => handle_rate_limit_connection(stream, &thread_mode),
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url,
            mode,
            stop,
            handle: Some(handle),
        }
    }

    fn set_mode(&self, mode: usize) {
        self.mode.store(mode, Ordering::SeqCst);
    }
}

impl Drop for RateLimitFakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_rate_limit_connection(mut stream: TcpStream, mode: &Arc<AtomicUsize>) {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().unwrap_or("");
    let mode = mode.load(Ordering::SeqCst);

    if request_line.starts_with("GET /repos/owner/repo/issues?")
        && request_line.contains("state=all")
        && mode == RATE_LIMIT_PRIMARY
    {
        let body = r#"{"message":"primary rate limit"}"#;
        let response = format!(
            "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset: 0\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        return;
    }
    if request_line.starts_with("GET /repos/owner/repo/issues?")
        && request_line.contains("state=all")
        && mode == RATE_LIMIT_SECONDARY
    {
        let body = r#"{"message":"secondary rate limit"}"#;
        let response = format!(
            "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\nretry-after: 0\r\nx-ratelimit-remaining: 42\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        return;
    }

    let body = if request_line.starts_with("GET /repos/owner/repo/issues?")
        && request_line.contains("state=all")
        && request_line.contains("per_page=100")
    {
        issue_payload_with_pr()
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42/comments?")
        && request_line.contains("per_page=100")
    {
        issue_comments_payload()
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42 ") {
        issue_object_payload()
    } else if request_line.starts_with("GET /repos/owner/repo/issues/comments/5001 ") {
        issue_comment_object_payload()
    } else {
        r#"{"message":"not found"}"#
    };
    let status = if body == issue_payload_with_pr()
        || body == issue_comments_payload()
        || body == issue_object_payload()
        || body == issue_comment_object_payload()
    {
        "200 OK"
    } else {
        "404 Not Found"
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\nx-ratelimit-remaining: 4999\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("qgh-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    root
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

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not JSON: {error}\nstdout:\n{}\nstderr:\n{}",
            stdout_text(output),
            stderr_text(output)
        )
    })
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

fn assert_query_result_round_trips_to_get_result(result: &Value, source: &Value) {
    assert_eq!(result["get_args"]["source_id"], source["source_id"]);
    assert_eq!(result["source_id"], source["source_id"]);
    assert_eq!(result["entity_type"], source["entity_type"]);
    assert_eq!(result["canonical_url"], source["canonical_url"]);
    assert_eq!(result["source_version"], source["source_version"]);
}

struct EditingFakeGitHub {
    base_url: String,
    mode: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EditingFakeGitHub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);
        let mode = Arc::new(AtomicUsize::new(1));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_mode = Arc::clone(&mode);
        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(stream) => handle_editing_connection(stream, &thread_mode, &thread_requests),
                    Err(_) => break,
                }
            }
        });
        Self {
            base_url,
            mode,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn set_mode(&self, mode: usize) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for EditingFakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_editing_connection(
    mut stream: TcpStream,
    mode: &Arc<AtomicUsize>,
    requests: &Arc<Mutex<Vec<String>>>,
) {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
    let request_line = request.lines().next().unwrap_or("").to_string();
    requests.lock().unwrap().push(request.clone());
    let mode = mode.load(Ordering::SeqCst);
    let lower = request.to_ascii_lowercase();

    let (status, etag, body) = if request_line.starts_with("GET /repos/owner/repo/issues?") {
        if mode == 2 && lower.contains("if-none-match: \"issues-v2\"") {
            ("304 Not Modified", "\"issues-v2\"", "")
        } else if mode == 2 {
            ("200 OK", "\"issues-v2\"", edited_issue_payload())
        } else {
            ("200 OK", "\"issues-v1\"", issue_payload_with_pr())
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42/comments?") {
        if mode == 2 {
            ("200 OK", "\"comments-v2\"", edited_issue_comments_payload())
        } else {
            ("200 OK", "\"comments-v1\"", issue_comments_payload())
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42 ") {
        if mode == 2 {
            ("200 OK", "\"issue-v2\"", edited_issue_object_payload())
        } else {
            ("200 OK", "\"issue-v1\"", issue_object_payload())
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/comments/5001 ") {
        if mode == 2 {
            (
                "200 OK",
                "\"comment-v2\"",
                edited_issue_comment_object_payload(),
            )
        } else {
            ("200 OK", "\"comment-v1\"", issue_comment_object_payload())
        }
    } else {
        ("404 Not Found", "\"missing\"", r#"{"message":"not found"}"#)
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\netag: {etag}\r\nx-ratelimit-remaining: 4999\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

fn edited_issue_payload() -> &'static str {
    r#"[
      {
        "id": 1001,
        "node_id": "I_kwDOISSUE1",
        "number": 42,
        "title": "Cache sync bug updated",
        "body": "The updated issue body must replace the old active search version.",
        "state": "open",
        "locked": false,
        "comments": 1,
        "html_url": "https://github.com/owner/repo/issues/42",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-04T00:00:00Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}, {"name": "mvp"}],
        "milestone": {"title": "MVP"},
        "assignees": [{"login": "alice"}]
      }
    ]"#
}

fn edited_issue_object_payload() -> &'static str {
    r#"{
        "id": 1001,
        "node_id": "I_kwDOISSUE1",
        "number": 42,
        "title": "Cache sync bug updated",
        "body": "The updated issue body must replace the old active search version.",
        "state": "open",
        "locked": false,
        "comments": 1,
        "html_url": "https://github.com/owner/repo/issues/42",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-04T00:00:00Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}, {"name": "mvp"}],
        "milestone": {"title": "MVP"},
        "assignees": [{"login": "alice"}]
    }"#
}

fn edited_issue_comments_payload() -> &'static str {
    r#"[
      {
        "id": 5001,
        "node_id": "IC_kwDOCOMMENT1",
        "body": "The updated comment body must be the only active comment search version.",
        "html_url": "https://github.com/owner/repo/issues/42#issuecomment-5001",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-04T00:01:00Z",
        "user": {"login": "carol"}
      }
    ]"#
}

fn edited_issue_comment_object_payload() -> &'static str {
    r#"{
        "id": 5001,
        "node_id": "IC_kwDOCOMMENT1",
        "body": "The updated comment body must be the only active comment search version.",
        "html_url": "https://github.com/owner/repo/issues/42#issuecomment-5001",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-04T00:01:00Z",
        "user": {"login": "carol"}
    }"#
}
