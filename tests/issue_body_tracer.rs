use chrono::{DateTime, Duration, SecondsFormat, Utc};
#[cfg(feature = "vector-search")]
use qgh::embedding::LOCAL_MODEL_REVISION;
#[cfg(feature = "fastembed-provider")]
use qgh::embedding::{
    ArtifactRole, FastembedProviderOptions, ModelArtifactV1, ModelManifestV1, ModelProviderKind,
    ModelSourceV1, NormalizationKind, QuantizationKind, TokenizerKind,
    MODEL_MANIFEST_SCHEMA_VERSION,
};
#[cfg(feature = "vector-search")]
use qgh::embedding::{
    EmbeddingFingerprintSeed, PoolingKind, DEFAULT_HF_MODEL_ID, DEFAULT_HF_MODEL_REVISION,
    DEFAULT_QUERY_PREFIX,
};
use serde_json::{json, Value};
#[cfg(feature = "fastembed-provider")]
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
#[cfg(feature = "fastembed-provider")]
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(feature = "vector-search")]
use std::os::raw::{c_char, c_int};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};

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
    fixture.assert_sqlite_chunks_empty();

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
    assert_eq!(status_json["data"]["freshness"]["decision"], "fresh");
    assert_eq!(status_json["data"]["freshness"]["remote_checked"], false);
    assert!(status_json["data"]["freshness"]["snapshot_age_seconds"]
        .as_i64()
        .is_some_and(|age| age >= 0));
    assert_eq!(status_json["data"]["freshness"]["max_age_seconds"], 604_800);
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
    assert_eq!(query_json["warnings"], json!([]));
    assert_eq!(query_json["meta"]["profile_id"], "work");
    assert_eq!(query_json["meta"]["profile_source"], "cli");
    assert_eq!(query_json["meta"]["repo"], Value::Null);
    assert_eq!(query_json["meta"]["repo_source"], Value::Null);
    assert_eq!(query_json["meta"]["repo_policy_path"], Value::Null);
    assert_eq!(query_json["data"]["freshness"]["decision"], "fresh");
    assert_eq!(query_json["data"]["freshness"]["remote_checked"], false);
    assert!(query_json["data"]["freshness"]["snapshot_age_seconds"]
        .as_i64()
        .is_some_and(|age| age >= 0));
    assert_eq!(query_json["data"]["freshness"]["max_age_seconds"], 604_800);
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
    assert!(result["ranking"].get("rrf_rank_score").is_none());
    assert!(result["ranking"].get("final_order_score").is_none());
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
    assert_eq!(source["lifecycle_check"]["status"], "not_checked");
    assert_eq!(source["lifecycle_check"]["reason"], "not_requested");
    assert_eq!(source["lifecycle_check"]["remote_checked"], false);
    assert_eq!(
        server.request_count(),
        2,
        "default get must read local source data without a lifecycle network check"
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
    assert_eq!(comment_source["lifecycle_check"]["status"], "not_checked");
    assert_eq!(comment_source["lifecycle_check"]["reason"], "not_requested");
    assert_eq!(comment_source["lifecycle_check"]["remote_checked"], false);
    assert_eq!(
        server.request_count(),
        2,
        "default comment get must not probe GitHub lifecycle"
    );

    let second_sync = fixture.qgh(["sync", "--json"]);
    assert_success(&second_sync);
    fixture.assert_sqlite_comment_metadata(1);
    fixture.assert_private_local_data_permissions();
}

#[test]
fn status_and_sync_fail_closed_on_a_future_store_schema_before_network_access() {
    let fixture = TestFixture::new("future-store-schema");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let db_path = fixture.data_home.join("qgh/profiles/work/qgh.sqlite3");
    let database = rusqlite::Connection::open(&db_path).unwrap();
    database
        .execute_batch(
            "UPDATE profile_meta
             SET value = 'qgh.db.v2'
             WHERE key = 'schema_version';
             CREATE TABLE future_schema_sentinel (
                 id INTEGER PRIMARY KEY,
                 value TEXT NOT NULL
             );
             INSERT INTO future_schema_sentinel (id, value)
             VALUES (1, 'must-survive');
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
    drop(database);
    let database_before = fs::read(&db_path).unwrap();
    let requests_before = server.request_count();

    for output in [
        fixture.qgh(["status", "--json"]),
        fixture.qgh(["sync", "--json"]),
    ] {
        assert_eq!(output.status.code(), Some(6));
        let body = stdout_json(&output);
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"]["code"], "storage.failure");
        assert_eq!(body["error"]["details"]["reason"], "unsupported_schema");
        assert_eq!(
            body["error"]["details"]["expected_schema_version"],
            "qgh.db.v1"
        );
        assert_eq!(
            body["error"]["details"]["actual_schema_version"],
            "qgh.db.v2"
        );
    }

    assert_eq!(
        server.request_count(),
        requests_before,
        "unsupported stores must fail before GitHub access"
    );
    assert_eq!(
        fs::read(&db_path).unwrap(),
        database_before,
        "read and write adapters must leave the rejected database byte-identical"
    );
    let unchanged = rusqlite::Connection::open(&db_path).unwrap();
    let marker: (String, String) = unchanged
        .query_row(
            "SELECT
                (SELECT value FROM profile_meta WHERE key = 'schema_version'),
                (SELECT value FROM future_schema_sentinel WHERE id = 1)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        marker,
        ("qgh.db.v2".to_string(), "must-survive".to_string())
    );
}

#[test]
fn malformed_tantivy_query_is_content_free_across_cli_and_mcp_errors() {
    let fixture = TestFixture::new("malformed-tantivy-query-content-free");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let private_marker = "PRIVATE_QUERY_FIELD_MARKER_0f92";
    let malformed_query = format!("{private_marker}:secret");

    let json_output = fixture.qgh(["query", &malformed_query, "--json"]);
    assert_eq!(json_output.status.code(), Some(2));
    assert_eq!(
        stdout_json(&json_output)["error"]["code"],
        "validation.invalid_query"
    );
    assert!(!stdout_text(&json_output).contains(private_marker));
    assert!(!stderr_text(&json_output).contains(private_marker));

    let human_output = fixture.qgh(["query", &malformed_query]);
    assert_eq!(human_output.status.code(), Some(2));
    assert!(stdout_text(&human_output).is_empty());
    assert!(stderr_text(&human_output).contains("validation.invalid_query"));
    assert!(!stderr_text(&human_output).contains(private_marker));

    let mcp_output = fixture.mcp([
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "qgh-test", "version": "0" }
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
                "arguments": { "query": malformed_query }
            }
        }),
    ]);
    assert_success(&mcp_output);
    let messages = stdout_json_lines(&mcp_output);
    let result = &messages[1]["result"];
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "validation.invalid_query"
    );
    assert!(!stdout_text(&mcp_output).contains(private_marker));
    assert!(!stderr_text(&mcp_output).contains(private_marker));
}

#[test]
fn normal_query_fails_closed_when_active_tantivy_artifact_is_missing() {
    let fixture = TestFixture::new("query-missing-active-tantivy");
    let server = RateLimitFakeGitHub::start();
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    fixture.remove_active_tantivy_generation();

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_eq!(query.status.code(), Some(6));
    assert_eq!(
        stdout_json(&query)["error"]["code"],
        "publication.tantivy_artifact_not_ready"
    );

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert!(status_json["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| { warning["code"] == "publication.tantivy_artifact_not_ready" }));

    let human_status = fixture.qgh(["status"]);
    assert_success(&human_status);
    let human_stdout = stdout_text(&human_status);
    assert!(human_stdout.contains("qgh status — search blocked"));
    assert!(human_stdout.contains("search: blocked until the local index is rebuilt"));
    assert!(human_stdout.contains("next: qgh sync --all --profile work"));

    server.set_mode(RATE_LIMIT_PRIMARY);
    let backoff = fixture.qgh(["sync", "--json"]);
    assert_eq!(backoff.status.code(), Some(5));
    let backoff_json = stdout_json(&backoff);
    assert_eq!(
        backoff_json["error"]["details"]["local_query_available"],
        false
    );
    assert_eq!(
        backoff_json["error"]["details"]["local_retrieval_available"],
        false
    );
    assert!(backoff_json["error"]["hint"]
        .as_str()
        .unwrap()
        .contains("Local query is not currently ready"));

    let blocked_backoff_status = fixture.qgh(["status"]);
    assert_success(&blocked_backoff_status);
    let blocked_backoff_stdout = stdout_text(&blocked_backoff_status);
    assert!(blocked_backoff_stdout.contains("qgh status — search blocked"));
    assert!(blocked_backoff_stdout.contains("next: retry now: qgh sync --all --profile work"));
}

#[test]
fn exact_query_fails_closed_but_get_survives_when_active_tantivy_is_missing() {
    let fixture = TestFixture::new("exact-query-missing-active-tantivy");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    fixture.remove_active_tantivy_generation();

    let exact = fixture.qgh(["query", "https://github.com/owner/repo/issues/42", "--json"]);
    assert_eq!(exact.status.code(), Some(6));
    assert_eq!(
        stdout_json(&exact)["error"]["code"],
        "publication.tantivy_artifact_not_ready"
    );

    let get = fixture.qgh(["get", "qgh://github.com/issue/I_kwDOISSUE1", "--json"]);
    assert_success(&get);
    assert_eq!(
        stdout_json(&get)["data"]["source"]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );
}

#[test]
fn doctor_reports_missing_tantivy_without_mutating_publication_pointer() {
    let fixture = TestFixture::new("doctor-missing-active-tantivy");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    let publication_id = fixture.active_retrieval_publication_id();
    fixture.remove_active_tantivy_generation();

    let doctor = fixture.qgh(["doctor", "--json"]);
    assert_success(&doctor);
    let doctor_json = stdout_json(&doctor);
    let tantivy = doctor_json["data"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "tantivy")
        .unwrap();
    assert_eq!(tantivy["ok"], false);
    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    assert_eq!(fixture.active_retrieval_publication_id(), publication_id);
}

#[test]
fn status_and_doctor_report_storage_repair_candidates_without_mutation() {
    let fixture = TestFixture::new("report-only-storage-diagnostics");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    let source_id = "qgh://github.com/issue/I_kwDOISSUE1";
    fixture.seed_open_repair_candidates(source_id, true);
    let before = fixture.open_repair_state(source_id);

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    assert_eq!(stdout_json(&status)["data"]["purge"]["pending_count"], 1);
    assert_eq!(fixture.open_repair_state(source_id), before);

    let doctor = fixture.qgh(["doctor", "--json"]);
    assert_success(&doctor);
    let doctor_json = stdout_json(&doctor);
    let checks = doctor_json["data"]["checks"].as_array().unwrap();
    assert!(checks
        .iter()
        .any(|check| check["name"] == "tantivy" && check["ok"] == false));
    assert!(checks
        .iter()
        .any(|check| check["name"] == "purge" && check["ok"] == false));
    assert_eq!(fixture.open_repair_state(source_id), before);
}

#[test]
fn query_and_default_get_do_not_repair_invalid_publication_on_open() {
    let fixture = TestFixture::new("read-only-storage-open");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    let source_id = "qgh://github.com/issue/I_kwDOISSUE1";
    fixture.seed_open_repair_candidates(source_id, false);
    let before = fixture.open_repair_state(source_id);

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_eq!(query.status.code(), Some(6));
    assert_eq!(
        stdout_json(&query)["error"]["code"],
        "publication.source_inventory_mismatch"
    );
    assert_eq!(fixture.open_repair_state(source_id), before);

    let get = fixture.qgh(["get", source_id, "--json"]);
    assert_success(&get);
    assert_eq!(stdout_json(&get)["data"]["source"]["source_id"], source_id);
    assert_eq!(fixture.open_repair_state(source_id), before);
}

#[test]
fn read_commands_do_not_bootstrap_a_missing_store_on_disk() {
    let fixture = TestFixture::new("missing-store-read-only");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    let profile_dir = fixture.data_home.join("qgh/profiles/work");
    let runtime_cache_dir = fixture.cache_home.join("qgh");

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    assert_eq!(
        stdout_json(&status)["data"]["freshness"]["decision"],
        "never_synced"
    );
    assert!(!profile_dir.exists());
    assert!(!runtime_cache_dir.exists());

    let doctor = fixture.qgh(["doctor", "--json"]);
    assert_success(&doctor);
    assert!(!profile_dir.exists());
    assert!(!runtime_cache_dir.exists());

    let query = fixture.qgh(["query", "nothing persisted", "--json"]);
    assert_success(&query);
    assert!(stdout_json(&query)["data"]["results"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(!profile_dir.exists());
    assert!(!runtime_cache_dir.exists());

    let get = fixture.qgh([
        "get",
        "qgh://github.com/issue/I_MISSING_READ_ONLY",
        "--json",
    ]);
    assert_eq!(get.status.code(), Some(4));
    assert_eq!(stdout_json(&get)["error"]["code"], "source.not_found");
    assert!(!profile_dir.exists());
    assert!(!runtime_cache_dir.exists());
}

#[test]
fn sync_sends_github_rest_headers_required_by_real_api() {
    let fixture = TestFixture::new("github-required-headers");
    let server = HeaderCheckingFakeGitHub::start();
    fixture.write_config(&server.base_url);

    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    assert_eq!(sync_json["data"]["sync_state"], "ok");
    assert_eq!(sync_json["data"]["issues"]["upserted"], 1);
    assert_eq!(sync_json["data"]["comments"]["upserted"], 1);
}

#[test]
fn doctor_does_not_forward_a_token_to_a_loopback_api_transport() {
    let fixture = TestFixture::new("doctor-loopback-token-boundary");
    let server = HeaderCheckingFakeGitHub::start();
    fixture.write_config(&server.base_url);

    let doctor = fixture.qgh(["doctor", "--json"]);
    assert_success(&doctor);
    let checks = stdout_json(&doctor)["data"]["checks"]
        .as_array()
        .unwrap()
        .clone();
    for name in ["github_auth_reachability", "rate_limit_headers"] {
        assert!(
            checks
                .iter()
                .any(|check| check["name"] == name && check["ok"] == true),
            "missing successful {name} check: {checks:#?}"
        );
    }
}

#[test]
fn doctor_does_not_follow_an_off_origin_redirect() {
    let fixture = TestFixture::new("doctor-off-origin-redirect");
    let server = DoctorRedirectFakeGitHub::start();
    fixture.write_config(&server.base_url);

    let doctor = fixture.qgh(["doctor", "--json"]);
    assert_success(&doctor);
    assert_eq!(
        server.redirected_request_count(),
        0,
        "doctor must not follow a configured GitHub probe to another origin"
    );
}

#[test]
fn sync_reports_human_progress_on_stderr_without_polluting_stdout() {
    let fixture = TestFixture::new("sync-progress");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);

    let sync = fixture.qgh(["sync"]);
    assert_success(&sync);
    let stdout = stdout_text(&sync);
    assert!(stdout.contains("qgh sync complete"));
    assert!(stdout.contains("synced repo scope: all profile repos"));
    assert!(stdout.contains("issues: fetched 1, upserted 1, skipped PRs 1"));
    assert!(stdout.contains("comments: fetched 1, upserted 1"));
    assert!(stdout.contains("active index generation: 1"));
    assert!(stdout.contains("next: qgh sync --backfill --all --profile work"));
    let stderr = stderr_text(&sync);
    assert!(stderr.contains("qgh sync: fetching GitHub issues/comments repos=1"));
    assert!(stderr.contains("qgh sync: fetching repo=owner/repo"));
    assert!(stderr.contains("qgh sync: received issue page repo=owner/repo items=2"));
    assert!(stderr.contains("qgh sync: received comment page repo=owner/repo issue=#42 items=1"));
    assert!(stderr.contains("qgh sync: complete sync_run_id="));

    let quiet = fixture.qgh(["sync", "--quiet"]);
    assert_success(&quiet);
    assert!(
        stderr_text(&quiet).is_empty(),
        "quiet stderr: {}",
        stderr_text(&quiet)
    );
    assert!(stdout_text(&quiet).contains("qgh sync complete"));
    assert!(!stdout_text(&quiet).starts_with('{'));

    let mut forced_terminal = fixture.base_command();
    let forced_quiet = forced_terminal
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .args(["--profile", "work", "sync", "--quiet"])
        .output()
        .unwrap();
    assert_success(&forced_quiet);
    let forced_quiet_stdout = stdout_text(&forced_quiet);
    assert!(forced_quiet_stdout.starts_with("qgh sync complete"));
    assert!(!forced_quiet_stdout.contains('\u{1b}'));
    assert!(!forced_quiet_stdout.contains('✓'));
    assert!(stderr_text(&forced_quiet).is_empty());

    let json = fixture.qgh(["sync", "--json"]);
    assert_success(&json);
    assert!(stderr_text(&json).is_empty());
    assert_eq!(stdout_json(&json)["data"]["sync_state"], "ok");
}

#[test]
fn non_json_cli_commands_print_human_summaries_without_weakening_json_contract() {
    let fixture = TestFixture::new("human-output");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let query = fixture.qgh(["query", "BM25 tracer"]);
    assert_success(&query);
    let query_stdout = stdout_text(&query);
    assert!(!query_stdout.starts_with('{'));
    assert!(query_stdout.contains("qgh query — 1 source candidates"));
    assert!(query_stdout.contains("These are source candidates, not answers"));
    assert!(query_stdout.contains("Snippets are previews, not citation evidence"));
    assert!(
        query_stdout.contains("get: qgh get qgh://github.com/issue/I_kwDOISSUE1 --profile-id work")
    );

    let search = fixture.qgh(["search", "BM25 tracer"]);
    assert_success(&search);
    assert!(stdout_text(&search).contains("qgh query — 1 source candidates"));

    let get = fixture.qgh(["get", "qgh://github.com/issue/I_kwDOISSUE1"]);
    assert_success(&get);
    let get_stdout = stdout_text(&get);
    assert!(get_stdout.contains("qgh source"));
    assert!(get_stdout.contains("canonical URL: https://github.com/owner/repo/issues/42"));
    assert!(get_stdout.contains("source version: body_hash="));
    assert!(get_stdout.contains("staleness metadata: github_updated_at=2026-01-02T03:04:05Z"));
    assert!(get_stdout.contains("lifecycle check: not_checked (not_requested)"));
    assert!(get_stdout
        .contains("The BM25 issue body tracer must round-trip through get before citation."));

    let verified_get = fixture.qgh([
        "get",
        "qgh://github.com/issue/I_kwDOISSUE1",
        "--verify-lifecycle",
    ]);
    assert_success(&verified_get);
    assert!(stdout_text(&verified_get).contains("lifecycle check: active"));

    let status = fixture.qgh(["status"]);
    assert_success(&status);
    let status_stdout = stdout_text(&status);
    assert!(status_stdout.contains("qgh status"));
    assert!(status_stdout.contains("selected profile: work"));
    assert!(status_stdout.contains("effective repo scope: all profile repos"));
    assert!(status_stdout.contains("DB path:"));
    assert!(status_stdout.contains("Tantivy index path:"));
    assert!(status_stdout.contains("default sync scope: all repos in the selected profile"));
    assert!(status_stdout.contains("next: qgh sync --backfill --all --profile work"));

    let doctor = fixture.qgh(["doctor"]);
    assert_success(&doctor);
    let doctor_stdout = stdout_text(&doctor);
    assert!(doctor_stdout.contains("qgh doctor"));
    assert!(doctor_stdout.contains("failed checks: 0"));
    assert!(doctor_stdout.contains("checks:"));
    assert!(doctor_stdout.contains("OK config"));
    assert!(doctor_stdout.contains("MCP tools: query, get, status"));

    let json_query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&json_query);
    assert_eq!(stdout_json(&json_query)["schema_version"], "qgh.v2");
    assert!(stderr_text(&json_query).is_empty());
}

#[test]
fn terminal_decoration_is_opt_in_and_no_color_is_authoritative() {
    let fixture = TestFixture::new("terminal-decoration");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let mut decorated_command = fixture.base_command();
    let decorated = decorated_command
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .args(["--profile", "work", "status"])
        .output()
        .unwrap();
    assert_success(&decorated);
    let decorated_stdout = stdout_text(&decorated);
    assert!(
        decorated_stdout.contains("\u{1b}["),
        "decorated stdout:\n{decorated_stdout}"
    );
    assert!(decorated_stdout.contains("✓ qgh status"));
    assert!(decorated_stdout.contains("→ qgh sync --backfill --all --profile work"));

    let mut plain_command = fixture.base_command();
    let plain = plain_command
        .env("CLICOLOR_FORCE", "1")
        .env("NO_COLOR", "1")
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .args(["--profile", "work", "status"])
        .output()
        .unwrap();
    assert_success(&plain);
    let plain_stdout = stdout_text(&plain);
    assert!(!plain_stdout.contains("\u{1b}["));
    assert!(plain_stdout.contains("✓ qgh status — search ready"));
    assert!(plain_stdout.contains("→ qgh sync --backfill --all --profile work"));

    let captured = fixture.qgh(["status"]);
    assert_success(&captured);
    assert!(!stdout_text(&captured).contains("\u{1b}["));

    let mut ascii_command = fixture.base_command();
    let ascii = ascii_command
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .env("TERM", "xterm-256color")
        .env("LC_ALL", "C")
        .args(["--profile", "work", "status"])
        .output()
        .unwrap();
    assert_success(&ascii);
    assert!(stdout_text(&ascii).contains("[ok] qgh status"));
    assert!(stdout_text(&ascii).contains("-> qgh sync --backfill --all --profile work"));

    let mut json_command = fixture.base_command();
    let json = json_command
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .env("TERM", "xterm-256color")
        .args(["--profile", "work", "status", "--json"])
        .output()
        .unwrap();
    assert_success(&json);
    assert_eq!(stdout_json(&json)["schema_version"], "qgh.v2");
    assert!(!stdout_text(&json).contains("\u{1b}["));
    assert!(stderr_text(&json).is_empty());

    let mut dumb_command = fixture.base_command();
    let dumb = dumb_command
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .env("TERM", "dumb")
        .args(["--profile", "work", "status"])
        .output()
        .unwrap();
    assert_success(&dumb);
    assert!(!stdout_text(&dumb).contains("\u{1b}["));
    assert!(!stdout_text(&dumb).contains('✓'));
    assert!(stdout_text(&dumb).contains("next: qgh sync --backfill --all --profile work"));

    let never_synced_fixture = TestFixture::new("terminal-decoration-not-ready");
    never_synced_fixture.write_config(&server.base_url);
    let mut not_ready_command = never_synced_fixture.base_command();
    let not_ready = not_ready_command
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .args(["--profile", "work", "status"])
        .output()
        .unwrap();
    assert_success(&not_ready);
    assert!(stdout_text(&not_ready).contains("! qgh status — search not ready"));
}

#[test]
fn terminal_decoration_preserves_authoritative_get_body_bytes() {
    let fixture = TestFixture::new("terminal-get-source-fidelity");
    let server = FakeGitHub::start(issue_payload_with_terminal_control_words());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let mut command = fixture.base_command();
    let get = command
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .args([
            "--profile",
            "work",
            "get",
            "qgh://github.com/issue/I_kwDOISSUE1",
        ])
        .output()
        .unwrap();
    assert_success(&get);
    let stdout = stdout_text(&get);
    let body = stdout.split_once("body:\n").unwrap().1;
    assert_eq!(
        body,
        "first line\r\nnext: reproduce this\r\nrepair: preserve this\r\nlast line\n"
    );
    assert!(!body.contains("\u{1b}["));
    assert!(!body.contains('→'));
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
    assert!(issue_result["ranking"].get("rrf_rank_score").is_none());
    assert!(issue_result["ranking"].get("final_order_score").is_none());
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
fn ghes_issue_url_resolves_as_exact_locator_and_round_trips() {
    let fixture = TestFixture::new("ghes-exact-issue-url");
    let server =
        FakeGitHub::start_with_comments(ghes_issue_payload(), ghes_issue_comments_payload());
    fixture.write_config_with_host("ghe.internal.example", &server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let canonical_url = "https://ghe.internal.example/owner/repo/issues/42";
    let query = fixture.qgh(["query", canonical_url, "--repo", "owner/repo", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    let results = query_json["data"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert_eq!(result["entity_type"], "issue");
    assert_eq!(result["canonical_url"], canonical_url);
    assert_eq!(result["ranking"]["kind"], "exact");
    assert_eq!(result["get_args"]["profile_id"], "work");

    let source_id = result["get_args"]["source_id"].as_str().unwrap();
    let get = fixture.qgh(["get", source_id, "--profile-id", "work", "--json"]);
    assert_success(&get);
    assert_query_result_round_trips_to_get_result(result, &stdout_json(&get)["data"]["source"]);
}

#[test]
fn ghes_comment_url_resolves_as_exact_locator_with_parent_context() {
    let fixture = TestFixture::new("ghes-exact-comment-url");
    let server =
        FakeGitHub::start_with_comments(ghes_issue_payload(), ghes_issue_comments_payload());
    fixture.write_config_with_host("ghe.internal.example", &server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let canonical_url = "https://ghe.internal.example/owner/repo/issues/42#issuecomment-5001";
    let query = fixture.qgh(["query", canonical_url, "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    let results = query_json["data"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert_eq!(result["entity_type"], "issue_comment");
    assert_eq!(result["canonical_url"], canonical_url);
    assert_eq!(result["ranking"]["kind"], "exact");
    assert_eq!(
        result["parent_issue"]["canonical_url"],
        "https://ghe.internal.example/owner/repo/issues/42"
    );

    let source_id = result["get_args"]["source_id"].as_str().unwrap();
    let get = fixture.qgh(["get", source_id, "--profile-id", "work", "--json"]);
    assert_success(&get);
    assert_query_result_round_trips_to_get_result(result, &stdout_json(&get)["data"]["source"]);
}

#[test]
fn unknown_configured_ghes_issue_url_returns_exact_empty_results() {
    let fixture = TestFixture::new("ghes-exact-unknown-issue-url");
    let server =
        FakeGitHub::start_with_comments(ghes_issue_payload(), ghes_issue_comments_payload());
    fixture.write_config_with_host("ghe.internal.example", &server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let query = fixture.qgh([
        "query",
        "https://ghe.internal.example/owner/repo/issues/999",
        "--json",
    ]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    assert_eq!(query_json["ok"], true);
    assert_eq!(query_json["data"]["results"], json!([]));
}

#[test]
fn foreign_or_malformed_urls_are_not_treated_as_exact_locators() {
    let fixture = TestFixture::new("ghes-non-locator-urls");
    let server =
        FakeGitHub::start_with_comments(ghes_issue_payload(), ghes_issue_comments_payload());
    fixture.write_config_with_host("ghe.internal.example", &server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    for query_text in [
        "https://other.example/owner/repo/issues/42",
        "https://ghe.internal.example/owner/repo/pull/42",
        "https://ghe.internal.example/owner/repo/issues/not-a-number",
    ] {
        let query = fixture.qgh(["query", query_text, "--json"]);
        assert_eq!(query.status.code(), Some(2));
        assert_eq!(
            stdout_json(&query)["error"]["code"],
            "validation.invalid_query"
        );
    }
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
fn get_batch_returns_sources_in_input_order_without_changing_single_get_shape() {
    let fixture = TestFixture::new("get-batch-success");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let issue_id = "qgh://github.com/issue/I_kwDOISSUE1";
    let comment_id = "qgh://github.com/issue-comment/IC_kwDOCOMMENT1";
    let single = fixture.qgh(["get", issue_id, "--json"]);
    assert_success(&single);
    let single_json = stdout_json(&single);
    assert_eq!(single_json["data"]["source"]["source_id"], issue_id);
    assert!(single_json["data"].get("items").is_none());
    assert!(single_json["data"].get("summary").is_none());

    let batch = fixture.qgh(["get", issue_id, comment_id, "--json"]);
    assert_success(&batch);
    let batch_json = stdout_json(&batch);
    assert_eq!(batch_json["data"]["profile_id"], "work");
    assert_eq!(batch_json["data"]["summary"]["requested"], 2);
    assert_eq!(batch_json["data"]["summary"]["returned"], 2);
    assert_eq!(batch_json["data"]["summary"]["failed"], 0);
    assert_eq!(batch_json["data"]["summary"]["batch_size_cap"], 20);
    assert_eq!(
        batch_json["data"]["lifecycle_check_policy"]["mode"],
        "not_requested"
    );
    assert_eq!(
        batch_json["data"]["lifecycle_check_policy"]["max_in_flight_requests"],
        0
    );
    assert_eq!(
        batch_json["data"]["lifecycle_check_policy"]["verify_lifecycle"],
        false
    );
    let items = batch_json["data"]["items"].as_array().unwrap();
    assert_eq!(items[0]["input_index"], 0);
    assert_eq!(items[0]["source_id"], issue_id);
    assert_eq!(items[0]["ok"], true);
    assert!(items[0]["source"]["body"]
        .as_str()
        .unwrap()
        .contains("BM25 issue body tracer"));
    assert_eq!(items[1]["input_index"], 1);
    assert_eq!(items[1]["source_id"], comment_id);
    assert_eq!(items[1]["ok"], true);
    assert!(items[1]["source"]["body"]
        .as_str()
        .unwrap()
        .contains("comment-only mitigation"));
}

#[test]
fn get_batch_records_not_found_as_item_error_and_continues() {
    let fixture = TestFixture::new("get-batch-not-found");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let issue_id = "qgh://github.com/issue/I_kwDOISSUE1";
    let missing_id = "qgh://github.com/issue/MISSING";
    let comment_id = "qgh://github.com/issue-comment/IC_kwDOCOMMENT1";
    let batch = fixture.qgh(["get", issue_id, missing_id, comment_id, "--json"]);
    assert_success(&batch);
    let batch_json = stdout_json(&batch);
    assert_eq!(batch_json["data"]["summary"]["requested"], 3);
    assert_eq!(batch_json["data"]["summary"]["returned"], 2);
    assert_eq!(batch_json["data"]["summary"]["failed"], 1);
    let items = batch_json["data"]["items"].as_array().unwrap();
    assert_eq!(items[0]["ok"], true);
    assert_eq!(items[1]["source_id"], missing_id);
    assert_eq!(items[1]["ok"], false);
    assert_eq!(items[1]["error"]["code"], "source.not_found");
    assert_eq!(items[1]["error"]["details"]["source_id"], missing_id);
    assert_eq!(items[2]["ok"], true);
}

#[test]
fn get_batch_records_scope_violation_as_item_error() {
    let fixture = TestFixture::new("get-batch-scope-item-error");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");
    assert_success(&fixture.qgh_in(&nested_worktree_dir, ["sync", "--all", "--json"]));

    let owner_id = "qgh://github.com/issue/I_POLICY_OWNER";
    let other_id = "qgh://github.com/issue/I_POLICY_OTHER";
    let batch =
        fixture.qgh_without_profile_in(&nested_worktree_dir, ["get", owner_id, other_id, "--json"]);
    assert_success(&batch);
    let batch_json = stdout_json(&batch);
    assert_eq!(batch_json["meta"]["profile_source"], "single_match");
    assert_eq!(batch_json["meta"]["repo"], "owner/repo");
    assert_eq!(batch_json["data"]["summary"]["returned"], 1);
    assert_eq!(batch_json["data"]["summary"]["failed"], 1);
    let items = batch_json["data"]["items"].as_array().unwrap();
    assert_eq!(items[0]["source_id"], owner_id);
    assert_eq!(items[0]["ok"], true);
    assert_eq!(items[1]["source_id"], other_id);
    assert_eq!(items[1]["ok"], false);
    assert_eq!(items[1]["error"]["code"], "source.outside_effective_scope");
    assert_eq!(
        items[1]["error"]["details"]["effective_repo_scope"],
        "owner/repo"
    );
}

#[test]
fn get_batch_defaults_to_local_reads_and_only_tombstones_with_lifecycle_opt_in() {
    let fixture = TestFixture::new("get-batch-tombstone-item-error");
    let server = LifecycleFakeGitHub::start();
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    server.set_mode(LIFECYCLE_DELETED_COMMENT);
    let issue_id = "qgh://github.com/issue/I_kwDOISSUE1";
    let comment_id = "qgh://github.com/issue-comment/IC_kwDOCOMMENT1";
    let request_count_before_default_get = server.request_count();
    let batch = fixture.qgh(["get", issue_id, comment_id, "--json"]);
    assert_success(&batch);
    let batch_json = stdout_json(&batch);
    assert_eq!(batch_json["data"]["summary"]["returned"], 2);
    assert_eq!(batch_json["data"]["summary"]["failed"], 0);
    assert_eq!(
        batch_json["data"]["lifecycle_check_policy"]["verify_lifecycle"],
        false
    );
    assert_eq!(
        batch_json["data"]["lifecycle_check_policy"]["mode"],
        "not_requested"
    );
    assert_eq!(
        batch_json["data"]["items"][1]["source"]["lifecycle_check"]["reason"],
        "not_requested"
    );
    assert_eq!(
        server.request_count(),
        request_count_before_default_get,
        "default batch get must not run lifecycle network probes"
    );

    let batch = fixture.qgh(["get", issue_id, comment_id, "--verify-lifecycle", "--json"]);
    assert_success(&batch);
    let batch_json = stdout_json(&batch);
    assert_eq!(batch_json["data"]["summary"]["returned"], 1);
    assert_eq!(batch_json["data"]["summary"]["failed"], 1);
    assert_eq!(
        batch_json["data"]["lifecycle_check_policy"]["verify_lifecycle"],
        true
    );
    assert_eq!(
        batch_json["data"]["lifecycle_check_policy"]["mode"],
        "sequential"
    );
    let items = batch_json["data"]["items"].as_array().unwrap();
    assert_eq!(items[0]["source_id"], issue_id);
    assert_eq!(items[0]["ok"], true);
    assert_eq!(items[1]["source_id"], comment_id);
    assert_eq!(items[1]["ok"], false);
    assert_eq!(items[1]["error"]["code"], "source.tombstoned");
    assert_eq!(items[1]["error"]["details"]["reason"], "deleted");
}

#[test]
fn get_batch_size_cap_and_missing_source_ids_are_command_level_errors() {
    let fixture = TestFixture::new("get-batch-cap");
    fixture.write_config("http://127.0.0.1:1");

    let source_ids = (0..21)
        .map(|index| format!("qgh://github.com/issue/I_CAP_{index}"))
        .collect::<Vec<_>>();
    let mut cap_cmd = fixture.base_command();
    let cap = cap_cmd
        .args(["--profile", "work", "get"])
        .args(source_ids.iter().map(String::as_str))
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(cap.status.code(), Some(2));
    let cap_json = stdout_json(&cap);
    assert_eq!(cap_json["error"]["code"], "validation.batch_size");
    assert_eq!(cap_json["error"]["details"]["requested"], 21);
    assert_eq!(cap_json["error"]["details"]["batch_size_cap"], 20);

    let missing = fixture.qgh(["get", "--json"]);
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(stdout_json(&missing)["error"]["code"], "validation.cli");
}

#[test]
fn repo_policy_defaults_cli_query_to_current_worktree_repo_scope() {
    let fixture = TestFixture::new("repo-policy-default-scope");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    let nested_worktree_dir = fixture.init_git_worktree_with_repo_policy("owner/repo");

    assert_success(&fixture.qgh_in(&nested_worktree_dir, ["sync", "--all", "--json"]));

    let default_query = fixture.qgh_in(
        &nested_worktree_dir,
        ["query", "shared repo policy tracer", "--json"],
    );
    assert_success(&default_query);
    let default_query_json = stdout_json(&default_query);
    assert_eq!(default_query_json["meta"]["profile_id"], "work");
    assert_eq!(default_query_json["meta"]["profile_source"], "cli");
    assert_eq!(default_query_json["meta"]["repo"], "owner/repo");
    assert_eq!(default_query_json["meta"]["repo_source"], "repo_policy");
    assert!(default_query_json["meta"]["repo_policy_path"]
        .as_str()
        .unwrap()
        .ends_with(".qgh.toml"));
    let default_results = default_query_json["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["repo"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(default_results, ["owner/repo"]);

    let search_alias = fixture.qgh_in(
        &nested_worktree_dir,
        ["search", "shared repo policy tracer", "--json"],
    );
    assert_success(&search_alias);
    assert_eq!(
        stdout_json(&search_alias)["data"]["results"][0]["repo"],
        "owner/repo"
    );

    let override_query = fixture.qgh_in(
        &nested_worktree_dir,
        [
            "query",
            "shared repo policy tracer",
            "--repo",
            "other/repo",
            "--json",
        ],
    );
    assert_success(&override_query);
    let override_results = stdout_json(&override_query)["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["repo"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(override_results, ["other/repo"]);

    server.clear_requests();
    let explicit_sync = fixture.qgh(["sync", "--repo", "other/repo", "--json"]);
    assert_success(&explicit_sync);
    let explicit_sync_json = stdout_json(&explicit_sync);
    assert_eq!(explicit_sync_json["meta"]["repo"], "other/repo");
    assert_eq!(explicit_sync_json["meta"]["repo_source"], "cli");
    let requests = server.requests();
    assert!(requests
        .iter()
        .any(|request| request.starts_with("GET /repos/other/repo/issues?")));
    assert!(requests
        .iter()
        .all(|request| !request.contains("/repos/owner/repo/")));

    let hard_issue_filter = fixture.qgh_in(&nested_worktree_dir, ["query", "#7", "--json"]);
    assert_success(&hard_issue_filter);
    assert_eq!(
        stdout_json(&hard_issue_filter)["data"]["results"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "repo policy must keep exact issue lookups inside the effective repo scope"
    );
}

#[test]
fn git_origin_scope_without_repo_policy_drives_sync_query_status_and_get() {
    let fixture = TestFixture::new("git-origin-command-resolution");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let sync = fixture.qgh_without_profile_in(&nested_worktree_dir, ["sync", "--json"]);
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    assert_eq!(sync_json["meta"]["profile_id"], "work");
    assert_eq!(sync_json["meta"]["profile_source"], "single_match");
    assert_eq!(sync_json["meta"]["repo"], "owner/repo");
    assert_eq!(sync_json["meta"]["repo_source"], "git_remote");
    assert_eq!(sync_json["data"]["issues"]["upserted"], 1);

    let owner_query = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        ["query", "shared repo policy tracer", "--json"],
    );
    assert_success(&owner_query);
    let owner_query_json = stdout_json(&owner_query);
    assert_eq!(owner_query_json["meta"]["repo_source"], "git_remote");
    assert_eq!(owner_query_json["data"]["results"][0]["repo"], "owner/repo");

    let status = fixture.qgh_without_profile_in(&nested_worktree_dir, ["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(
        status_json["data"]["resolution"]["effective_repo_scope"],
        "owner/repo"
    );
    assert_eq!(
        status_json["data"]["resolution"]["repo_source"],
        "git_remote"
    );

    let other_get = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "get",
            "qgh://github.com/issue/I_POLICY_OTHER",
            "--profile-id",
            "work",
            "--json",
        ],
    );
    assert_eq!(other_get.status.code(), Some(4));
    assert_eq!(stdout_json(&other_get)["error"]["code"], "source.not_found");
}

#[test]
fn scoped_reconcile_does_not_probe_other_repos_without_all() {
    let fixture = TestFixture::new("scoped-reconcile");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    assert_success(&fixture.qgh_in(&nested_worktree_dir, ["sync", "--all", "--json"]));
    server.clear_requests();

    let sync = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        ["sync", "--reconcile", "full", "--json"],
    );
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    assert_eq!(sync_json["meta"]["repo"], "owner/repo");
    assert_eq!(sync_json["meta"]["repo_source"], "git_remote");
    assert_eq!(sync_json["data"]["reconciliation"]["checked_sources"], 1);

    let requests = server.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.starts_with("GET /repos/owner/repo/issues/42 ")),
        "scoped reconciliation should check the effective repo: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.contains("/repos/other/repo/")),
        "scoped reconciliation must not touch unrelated repos: {requests:?}"
    );
    let other_get = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "get",
            "qgh://github.com/issue/I_POLICY_OTHER",
            "--profile-id",
            "work",
            "--json",
        ],
    );
    assert_success(&other_get);
    assert_eq!(
        stdout_json(&other_get)["data"]["source"]["repo"],
        "other/repo"
    );
}

#[test]
fn sync_purges_repo_removed_from_profile_allowlist_and_preserves_other_repo() {
    let fixture = TestFixture::new("explicit-allowlist-removal-purge");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    assert_success(&fixture.qgh(["sync", "--json"]));
    server.clear_requests();

    fixture.write_config_with_repos(&server.base_url, &["owner/repo"]);
    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);

    let removed_id = "qgh://github.com/issue/I_POLICY_OTHER";
    let removed_get = fixture.qgh(["get", removed_id, "--json"]);
    assert_eq!(removed_get.status.code(), Some(4));
    assert_eq!(
        stdout_json(&removed_get)["error"]["details"]["reason"],
        "allowlist_removal"
    );

    let retained = fixture.qgh(["query", "shared repo policy tracer", "--json"]);
    assert_success(&retained);
    let retained_json = stdout_json(&retained);
    let results = retained_json["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["repo"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results, ["owner/repo"]);
    assert!(server
        .requests()
        .iter()
        .all(|request| !request.contains("/repos/other/repo/")));
}

#[test]
fn query_fails_closed_until_removed_profile_repository_is_reconciled() {
    let fixture = TestFixture::new("allowlist-removal-read-fence-query");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let profile_wide = fixture.qgh(["query", "shared repo policy tracer", "--json"]);
    assert_success(&profile_wide);
    let profile_wide_json = stdout_json(&profile_wide);
    let profile_wide_repos = profile_wide_json["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["repo"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        profile_wide_repos,
        BTreeSet::from(["other/repo", "owner/repo"])
    );

    let private_query = "PRIVATE_REMOVED_ALLOWLIST_QUERY_8d31";
    fixture.write_config_with_repos_and_embedding(
        &server.base_url,
        &["owner/repo"],
        r#"
[embedding]
provider = "local"
model_path = "/definitely/not/a/model"
file = "onnx/model.onnx"
pooling = "cls"
query_prefix = "query: "
quantization = "none"
"#,
    );
    server.clear_requests();
    let query = fixture.qgh(["query", private_query, "--json"]);

    let removed_id = "qgh://github.com/issue/I_POLICY_OTHER";
    let retained_id = "qgh://github.com/issue/I_POLICY_OWNER";
    let removed_get = fixture.qgh(["get", removed_id, "--verify-lifecycle", "--json"]);
    let retained_get = fixture.qgh(["get", retained_id, "--json"]);
    let batch_get = fixture.qgh(["get", retained_id, removed_id, "--json"]);
    for output in [&query, &removed_get, &retained_get, &batch_get] {
        assert_eq!(output.status.code(), Some(6));
        let json = stdout_json(output);
        assert_eq!(
            json["error"]["code"],
            "purge.allowlist_reconciliation_required"
        );
        let serialized = serde_json::to_string(&json).unwrap();
        for forbidden in [
            private_query,
            "other/repo",
            "owner/repo",
            "I_POLICY_OTHER",
            "I_POLICY_OWNER",
        ] {
            assert!(!serialized.contains(forbidden));
        }
    }
    assert!(
        server.requests().is_empty(),
        "allowlist fencing must run before lifecycle network access"
    );

    let mcp = fixture.mcp([
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "qgh-test", "version": "0" }
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
                "arguments": { "query": private_query }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "get",
                "arguments": { "source_id": removed_id }
            }
        }),
    ]);
    assert_success(&mcp);
    assert!(stderr_text(&mcp).is_empty());
    let messages = stdout_json_lines(&mcp);
    for response in [&messages[1], &messages[2]] {
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(
            response["result"]["structuredContent"]["error"]["code"],
            "purge.allowlist_reconciliation_required"
        );
        let serialized = serde_json::to_string(response).unwrap();
        for forbidden in [private_query, "other/repo", removed_id] {
            assert!(!serialized.contains(forbidden));
        }
    }

    let db_path = fixture.data_home.join("qgh/profiles/work/qgh.sqlite3");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let pending_purges: i64 = conn
        .query_row(
            "SELECT count(*) FROM purge_requests WHERE purge_pending = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pending_purges, 0,
        "read fencing must not mutate purge state"
    );
    let removed_active: i64 = conn
        .query_row(
            "SELECT count(*) FROM source_entities
             WHERE source_id = ?1 AND lifecycle_state = 'active'",
            rusqlite::params![removed_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(removed_active, 1);
    #[cfg(feature = "vector-search")]
    {
        let vector_migration: i64 = conn
            .query_row(
                "SELECT count(*) FROM schema_migrations WHERE version = 'qgh.vector.v1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            vector_migration, 0,
            "allowlist fencing must run before vector schema initialization"
        );
    }
    drop(conn);

    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);
    let removed_after_sync = fixture.qgh(["get", removed_id, "--json"]);
    assert_eq!(removed_after_sync.status.code(), Some(4));
    assert_eq!(
        stdout_json(&removed_after_sync)["error"]["details"]["reason"],
        "allowlist_removal"
    );
    let retained_after_sync = fixture.qgh(["query", "shared repo policy tracer", "--json"]);
    assert_success(&retained_after_sync);
    assert!(stdout_json(&retained_after_sync)["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["repo"] == "owner/repo"));
}

#[test]
fn casing_only_profile_allowlist_change_preserves_github_repository() {
    let fixture = TestFixture::new("allowlist-repo-casing-preserved");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo"]);
    assert_success(&fixture.qgh(["sync", "--json"]));

    fixture.write_config_with_repos(&server.base_url, &["OWNER/REPO"]);
    server.clear_requests();
    let retained_before_sync =
        fixture.qgh(["get", "qgh://github.com/issue/I_POLICY_OWNER", "--json"]);
    assert_success(&retained_before_sync);
    let query_before_sync = fixture.qgh(["query", "shared repo policy tracer", "--json"]);
    assert_success(&query_before_sync);
    assert!(server.requests().is_empty());

    let sync = fixture.qgh(["sync", "--if-stale", "--json"]);
    assert_success(&sync);
    assert_eq!(stdout_json(&sync)["data"]["sync_state"], "skipped_fresh");
    assert!(server.requests().is_empty());

    let retained = fixture.qgh(["get", "qgh://github.com/issue/I_POLICY_OWNER", "--json"]);
    assert_success(&retained);
    assert_eq!(
        stdout_json(&retained)["data"]["source"]["repo"],
        "owner/repo"
    );
    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(status["data"]["sources"]["tombstone_count"], 0);
}

#[test]
fn purge_successor_snapshot_does_not_advance_remote_sync_freshness() {
    let fixture = TestFixture::new("purge-successor-keeps-remote-freshness");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    assert_success(&fixture.qgh(["sync", "--json"]));
    let before = stdout_json(&fixture.qgh(["status", "--json"]));
    let remote_last_sync = before["data"]["sync"]["last_sync_at"].clone();
    assert!(remote_last_sync.as_str().is_some());

    fixture.write_config_with_repos(&server.base_url, &["owner/repo"]);
    server.clear_requests();
    let purge = fixture.qgh(["sync", "--if-stale", "--json"]);
    assert_success(&purge);
    assert_eq!(stdout_json(&purge)["data"]["sync_state"], "skipped_fresh");
    assert!(
        server.requests().is_empty(),
        "purge successor repair must not force or impersonate a remote sync"
    );

    let after = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(after["data"]["sync"]["last_sync_at"], remote_last_sync);
    assert_eq!(after["data"]["purge"]["successor_repair_required"], false);
    let removed = fixture.qgh(["get", "qgh://github.com/issue/I_POLICY_OTHER", "--json"]);
    assert_eq!(removed.status.code(), Some(4));
    assert_eq!(
        stdout_json(&removed)["error"]["details"]["reason"],
        "allowlist_removal"
    );
}

#[test]
fn purge_successor_remains_queryable_when_configured_embedding_refresh_fails() {
    let fixture = TestFixture::new("purge-successor-embedding-fallback");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    assert_success(&fixture.qgh(["sync", "--json"]));

    fixture.write_config_with_repos_and_embedding(
        &server.base_url,
        &["owner/repo"],
        r#"
[embedding]
provider = "local"
model_path = "/definitely/not/a/model"
file = "onnx/model.onnx"
pooling = "cls"
query_prefix = "query: "
quantization = "none"
"#,
    );
    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    assert!(warning_codes(&sync_json)
        .iter()
        .any(|code| code.starts_with("embedding.sync_") && code.ends_with("_failed")));
    let reported_generation = sync_json["data"]["index"]["active_generation"]
        .as_i64()
        .unwrap();
    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(
        status["data"]["index"]["active_generation"],
        reported_generation
    );
    assert_eq!(
        fixture.active_retrieval_publication_generation(),
        reported_generation
    );

    let query = fixture.qgh(["query", "shared repo policy tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    let results = query_json["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["repo"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results, ["owner/repo"]);
}

#[cfg(unix)]
#[test]
fn missing_successor_publication_stays_blocked_until_preflight_repair_succeeds() {
    use std::os::unix::fs::symlink;

    let fixture = TestFixture::new("purge-successor-repair-retry");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo"]);
    assert_success(&fixture.qgh(["sync", "--json"]));
    fixture.clear_retrieval_publication();
    fixture.mark_successor_repair_required();

    let index_root = fixture.data_home.join("qgh/profiles/work/tantivy");
    let saved_index_root = fixture
        .data_home
        .join("qgh/profiles/work/tantivy-before-repair");
    fs::rename(&index_root, &saved_index_root).unwrap();
    let invalid_index_root = fixture.root.join("invalid-repair-index-root");
    fs::create_dir_all(&invalid_index_root).unwrap();
    symlink(&invalid_index_root, &index_root).unwrap();
    server.clear_requests();
    let failed = fixture.qgh(["sync", "--json"]);

    fs::remove_file(&index_root).unwrap();
    fs::rename(&saved_index_root, &index_root).unwrap();

    assert_eq!(failed.status.code(), Some(6));
    assert!(server.requests().is_empty(), "repair must precede network");
    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(status["data"]["purge"]["pending_count"], 0);
    assert_eq!(status["data"]["purge"]["successor_repair_required"], true);
    assert_eq!(status["data"]["purge"]["retrieval_blocked"], true);
    let doctor = stdout_json(&fixture.qgh(["doctor", "--json"]));
    let purge_check = doctor["data"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "purge")
        .unwrap();
    assert_eq!(purge_check["ok"], false);

    let query = fixture.qgh(["query", "shared repo policy tracer", "--json"]);
    assert_eq!(query.status.code(), Some(6));
    assert_eq!(
        stdout_json(&query)["error"]["code"],
        "purge.successor_repair_required"
    );

    fixture.write_config_with_repos("http://127.0.0.1:1", &["owner/repo"]);
    let repaired_before_network_failure = fixture.qgh(["sync", "--json"]);
    assert_eq!(repaired_before_network_failure.status.code(), Some(3));
    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(status["data"]["purge"]["successor_repair_required"], false);
    assert_eq!(status["data"]["purge"]["retrieval_blocked"], false);
    assert_success(&fixture.qgh(["query", "shared repo policy tracer", "--json"]));
}

#[test]
fn never_synced_empty_store_does_not_require_synthetic_successor_snapshot() {
    let fixture = TestFixture::new("purge-successor-empty-never-synced");
    fixture.write_config_with_repos("http://127.0.0.1:1", &["owner/repo"]);

    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(status["data"]["purge"]["successor_repair_required"], false);
    assert_eq!(status["data"]["purge"]["retrieval_blocked"], false);
    let query = fixture.qgh(["query", "nothing indexed yet", "--json"]);
    assert_success(&query);
    assert!(stdout_json(&query)["data"]["results"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[cfg(unix)]
#[test]
fn pending_purge_is_retried_by_next_sync_without_touching_user_backup() {
    use std::os::unix::fs::symlink;

    let fixture = TestFixture::new("pending-purge-report-and-retry");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let profile_dir = fixture.data_home.join("qgh/profiles/work");
    let index_root = profile_dir.join("tantivy");
    let saved_index_root = profile_dir.join("tantivy-before-purge");
    fs::rename(&index_root, &saved_index_root).unwrap();
    let user_backup = fixture.root.join("user-created-index-backup");
    fs::create_dir_all(user_backup.join("generation-999")).unwrap();
    let backup_marker = user_backup.join("generation-999/private-backup-marker");
    fs::write(&backup_marker, "user-managed").unwrap();
    symlink(&user_backup, &index_root).unwrap();

    fixture.write_config_with_repos(&server.base_url, &["owner/repo"]);
    server.clear_requests();
    let failed = fixture.qgh(["sync", "--json"]);
    assert_eq!(failed.status.code(), Some(6));
    assert_eq!(stdout_json(&failed)["error"]["code"], "purge.retry_failed");
    assert!(
        server.requests().is_empty(),
        "purge preflight precedes network"
    );

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["purge"]["pending_count"], 1);
    assert_eq!(
        status_json["data"]["purge"]["successor_repair_required"],
        true
    );
    assert_eq!(status_json["data"]["purge"]["retrieval_blocked"], true);
    assert_eq!(
        status_json["data"]["purge"]["current_stages"],
        json!(["tantivy"])
    );

    let doctor = fixture.qgh(["doctor", "--json"]);
    assert_success(&doctor);
    let doctor_json = stdout_json(&doctor);
    assert_eq!(doctor_json["data"]["purge"]["pending_count"], 1);
    assert_eq!(
        doctor_json["data"]["purge"]["unmanaged_filesystem_backups"],
        "not_deleted_by_qgh"
    );
    let purge_check = doctor_json["data"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "purge")
        .unwrap();
    assert_eq!(purge_check["ok"], false);

    assert!(backup_marker.exists());

    fs::remove_file(&index_root).unwrap();
    fs::rename(&saved_index_root, &index_root).unwrap();
    let retry = fixture.qgh(["sync", "--json"]);
    assert_success(&retry);
    assert!(backup_marker.exists());
    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    assert_eq!(stdout_json(&status)["data"]["purge"]["pending_count"], 0);
    assert_eq!(
        stdout_json(&status)["data"]["purge"]["successor_repair_required"],
        false
    );
    assert_success(&fixture.qgh(["query", "shared repo policy tracer", "--json"]));
}

#[test]
fn get_without_profile_id_enforces_current_origin_scope_but_profile_id_round_trips() {
    let fixture = TestFixture::new("get-origin-scope");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    assert_success(&fixture.qgh_in(&nested_worktree_dir, ["sync", "--all", "--json"]));

    let scoped_get = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        ["get", "qgh://github.com/issue/I_POLICY_OTHER", "--json"],
    );
    assert_eq!(scoped_get.status.code(), Some(4));
    let scoped_json = stdout_json(&scoped_get);
    assert_eq!(
        scoped_json["error"]["code"],
        "source.outside_effective_scope"
    );
    assert_eq!(
        scoped_json["error"]["details"]["effective_repo_scope"],
        "owner/repo"
    );

    let round_trip_get = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "get",
            "qgh://github.com/issue/I_POLICY_OTHER",
            "--profile-id",
            "work",
            "--json",
        ],
    );
    assert_success(&round_trip_get);
    let round_trip_json = stdout_json(&round_trip_get);
    assert_eq!(round_trip_json["meta"]["profile_source"], "get_args");
    assert_eq!(
        round_trip_json["data"]["source"]["source_id"],
        "qgh://github.com/issue/I_POLICY_OTHER"
    );
}

#[test]
fn repo_policy_query_limit_sets_default_when_query_omits_limit() {
    let fixture = TestFixture::new("repo-policy-query-limit");
    let server = FakeGitHub::start(limit_policy_issue_payload());
    fixture.write_config(&server.base_url);
    let nested_worktree_dir = fixture.init_git_worktree_with_repo_policy("owner/repo");
    fixture.write_repo_policy_with_query_limit("owner/repo", 3);

    assert_success(&fixture.qgh_in(&nested_worktree_dir, ["sync", "--all", "--json"]));

    let default_limit = fixture.qgh_in(
        &nested_worktree_dir,
        ["query", "repo policy limit tracer", "--json"],
    );
    assert_success(&default_limit);
    assert_eq!(
        stdout_json(&default_limit)["data"]["results"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    let explicit_limit = fixture.qgh_in(
        &nested_worktree_dir,
        [
            "query",
            "repo policy limit tracer",
            "--limit",
            "5",
            "--json",
        ],
    );
    assert_success(&explicit_limit);
    assert_eq!(
        stdout_json(&explicit_limit)["data"]["results"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
}

#[test]
fn repo_policy_rejects_profile_credentials_and_storage_fields() {
    let fixture = TestFixture::new("repo-policy-forbidden-fields");
    fixture.write_config("http://127.0.0.1:1");
    let nested_worktree_dir = fixture.init_git_worktree_with_repo_policy("owner/repo");

    for forbidden_policy in [
        r#"
schema_version = "qgh.repo.v1"
profile_id = "work"

[repo]
github = "owner/repo"
"#,
        r#"
schema_version = "qgh.repo.v1"
token = "ghp_plaintext"

[repo]
github = "owner/repo"
"#,
        r#"
schema_version = "qgh.repo.v1"
db_path = "/Users/user/private/qgh.sqlite3"

[repo]
github = "owner/repo"
"#,
    ] {
        fs::write(fixture.root.join(".qgh.toml"), forbidden_policy).unwrap();
        let output = fixture.qgh_in(&nested_worktree_dir, ["query", "anything", "--json"]);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            stdout_json(&output)["error"]["code"],
            "config.invalid_repo_policy"
        );
    }
}

#[test]
fn repo_policy_and_cli_repo_override_cannot_widen_profile_allowlist() {
    let fixture = TestFixture::new("repo-policy-allowlist");
    fixture.write_config("http://127.0.0.1:1");
    let nested_worktree_dir = fixture.init_git_worktree_with_repo_policy("other/repo");

    let policy_outside_allowlist =
        fixture.qgh_in(&nested_worktree_dir, ["query", "anything", "--json"]);
    assert_eq!(policy_outside_allowlist.status.code(), Some(2));
    assert_eq!(
        stdout_json(&policy_outside_allowlist)["error"]["code"],
        "config.invalid_repo_policy"
    );

    let policy_outside_status = fixture.qgh_in(&nested_worktree_dir, ["status", "--json"]);
    assert_eq!(policy_outside_status.status.code(), Some(2));
    assert_eq!(
        stdout_json(&policy_outside_status)["error"]["code"],
        "config.invalid_repo_policy"
    );

    let policy_outside_doctor = fixture.qgh_in(&nested_worktree_dir, ["doctor", "--json"]);
    assert_eq!(policy_outside_doctor.status.code(), Some(2));
    assert_eq!(
        stdout_json(&policy_outside_doctor)["error"]["code"],
        "config.invalid_repo_policy"
    );

    let policy_outside_get = fixture.qgh_in(
        &nested_worktree_dir,
        ["get", "qgh://github.com/issue/I_POLICY_OTHER", "--json"],
    );
    assert_eq!(policy_outside_get.status.code(), Some(2));
    assert_eq!(
        stdout_json(&policy_outside_get)["error"]["code"],
        "config.invalid_repo_policy"
    );

    let mut env_status = fixture.base_command();
    let env_status = env_status
        .current_dir(&nested_worktree_dir)
        .env("QGH_PROFILE", "work")
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert_eq!(env_status.status.code(), Some(2));
    assert_eq!(
        stdout_json(&env_status)["error"]["code"],
        "config.invalid_repo_policy"
    );

    fixture.write_repo_policy("owner/repo");
    let override_outside_allowlist = fixture.qgh_in(
        &nested_worktree_dir,
        ["query", "anything", "--repo", "other/repo", "--json"],
    );
    assert_eq!(override_outside_allowlist.status.code(), Some(2));
    assert_eq!(
        stdout_json(&override_outside_allowlist)["error"]["code"],
        "validation.invalid_repo"
    );
}

#[test]
fn init_repo_writes_repo_policy_from_cli_repo_at_current_worktree_root() {
    let fixture = TestFixture::new("init-cli-repo");
    fixture.write_config_with_repos("http://127.0.0.1:1", &["owner/repo"]);
    let nested_worktree_dir = fixture.init_git_worktree();

    let init = fixture.qgh_in(
        &nested_worktree_dir,
        ["init", "repo", "--repo", "owner/repo", "--json"],
    );
    assert_success(&init);
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["repo"], "owner/repo");
    assert_eq!(init_json["data"]["repo_source"], "cli");
    assert_eq!(init_json["meta"]["repo_source"], "cli");
    assert_eq!(init_json["data"]["overwritten"], false);
    assert_eq!(
        init_json["data"]["profile_validation"]["status"],
        "validated"
    );
    assert_eq!(
        init_json["data"]["profile_validation"]["profile_id"],
        "work"
    );
    assert_eq!(init_json["warnings"].as_array().unwrap().len(), 0);
    assert!(init_json["data"]["path"]
        .as_str()
        .unwrap()
        .ends_with(".qgh.toml"));

    let policy_path = fixture.root.join(".qgh.toml");
    let policy = fs::read_to_string(&policy_path).unwrap();
    assert!(policy.contains(r#"schema_version = "qgh.repo.v1""#));
    assert!(policy.contains(r#"github = "owner/repo""#));
    assert!(policy.contains(r#"scope = "repo""#));
    assert!(policy.contains(r#"source_types = ["issue", "issue_comment"]"#));
    assert!(policy.contains("limit = 10"));
    for forbidden in ["token", "token_source", "profile_id", "db_path", "/Users/"] {
        assert!(
            !policy.contains(forbidden),
            "generated repo policy must not contain {forbidden}: {policy}"
        );
    }

    let status = fixture.qgh_in(&nested_worktree_dir, ["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(
        status_json["data"]["resolution"]["repo_source"],
        "repo_policy"
    );
    assert_eq!(
        status_json["data"]["resolution"]["effective_repo_scope"],
        "owner/repo"
    );
}

#[test]
fn init_repo_honors_parent_json_flag() {
    let fixture = TestFixture::new("init-repo-parent-json");
    fixture.write_config_with_repos("http://127.0.0.1:1", &["owner/repo"]);
    let nested_worktree_dir = fixture.init_git_worktree();

    let init = fixture.qgh_in(
        &nested_worktree_dir,
        ["init", "--json", "repo", "--repo", "owner/repo"],
    );
    assert_success(&init);
    assert!(stderr_text(&init).is_empty());
    let init_json = stdout_json(&init);
    assert_eq!(init_json["schema_version"], "qgh.v2");
    assert_eq!(init_json["data"]["repo"], "owner/repo");
    assert_eq!(init_json["data"]["repo_source"], "cli");
}

#[test]
fn init_yes_bootstraps_profile_config_and_repo_policy_without_secret_or_store_paths() {
    let fixture = TestFixture::new("init-yes-bootstrap");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "init",
            "--yes",
            "--profile",
            "work",
            "--repo",
            "owner/repo",
            "--host",
            "github.com",
            "--api-base-url",
            "https://api.github.com",
            "--web-base-url",
            "https://github.com",
            "--token-source",
            "env",
            "--token-env",
            "QGH_TEST_TOKEN",
            "--json",
        ],
    );
    assert_success(&init);
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["profile_id"], "work");
    assert_eq!(init_json["data"]["profile_action"], "created");
    assert_eq!(init_json["data"]["repo"], "owner/repo");
    assert_eq!(init_json["meta"]["repo_source"], "cli");
    assert_eq!(init_json["data"]["repo_allowlist_action"], "added");
    assert_eq!(init_json["data"]["repo_policy_action"], "created");
    assert_eq!(init_json["data"]["token_source"]["kind"], "env");
    let expected_next_steps = if cfg!(feature = "fastembed-provider") {
        json!([
            "qgh model install qwen3-embedding-0.6b",
            "qgh sync",
            "qgh query <terms>"
        ])
    } else {
        json!(["qgh sync", "qgh query <terms>"])
    };
    assert_eq!(init_json["data"]["next_steps"], expected_next_steps);

    let config_text = fs::read_to_string(fixture.config_home.join("qgh/config.toml")).unwrap();
    assert!(config_text.contains(r#"schema_version = "qgh.config.v1""#));
    assert!(config_text.contains("[profiles.work]"));
    assert!(config_text.contains(r#"host = "github.com""#));
    assert!(config_text.contains(r#"api_base_url = "https://api.github.com""#));
    assert!(config_text.contains(r#"web_base_url = "https://github.com""#));
    assert!(config_text.contains(r#"repos = ["owner/repo"]"#));
    assert!(config_text.contains(r#"type = "env""#));
    assert!(config_text.contains(r#"env = "QGH_TEST_TOKEN""#));
    for forbidden in [
        "fixture-token",
        "qgh.sqlite3",
        "profiles/work",
        fixture.data_home.to_str().unwrap(),
    ] {
        assert!(
            !config_text.contains(forbidden),
            "config must not contain {forbidden}: {config_text}"
        );
        assert!(
            !stdout_text(&init).contains(forbidden),
            "init output must not contain {forbidden}: {}",
            stdout_text(&init)
        );
    }

    let config_dir = fixture.config_home.join("qgh");
    let config_lock = config_dir.join("config.toml.lock");
    assert!(fs::symlink_metadata(&config_lock)
        .unwrap()
        .file_type()
        .is_file());
    assert!(fs::read(&config_lock).unwrap().is_empty());
    let staging_files = fs::read_dir(&config_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect::<Vec<_>>();
    assert!(staging_files.is_empty(), "staging files: {staging_files:?}");
    #[cfg(unix)]
    {
        assert_eq!(
            fs::metadata(config_dir.join("config.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&config_lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&config_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    let policy = fs::read_to_string(fixture.root.join(".qgh.toml")).unwrap();
    assert!(policy.contains(r#"github = "owner/repo""#));
    assert!(!policy.contains("QGH_TEST_TOKEN"));
    assert!(!policy.contains("profiles/work"));

    let status = fixture.qgh_without_profile_in(&nested_worktree_dir, ["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["meta"]["profile_source"], "single_match");
    assert_eq!(
        status_json["data"]["resolution"]["effective_repo_scope"],
        "owner/repo"
    );
}

#[cfg(unix)]
#[test]
fn init_rejects_symlinked_profile_config_without_mutating_its_target() {
    use std::os::unix::fs::symlink;

    let fixture = TestFixture::new("init-symlinked-profile-config");
    fixture.write_config("https://api.github.com");
    let config_path = fixture.config_home.join("qgh/config.toml");
    let target_path = fixture.root.join("managed-config.toml");
    fs::rename(&config_path, &target_path).unwrap();
    symlink(&target_path, &config_path).unwrap();
    let original_target = fs::read(&target_path).unwrap();
    let worktree = fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(
        &worktree,
        [
            "--profile",
            "fresh",
            "init",
            "--yes",
            "--repo",
            "other/repo",
            "--host",
            "github.com",
            "--api-base-url",
            "https://api.github.com",
            "--web-base-url",
            "https://github.com",
            "--token-source",
            "github_cli",
            "--json",
        ],
    );

    assert_eq!(init.status.code(), Some(6));
    assert_eq!(stdout_json(&init)["error"]["code"], "storage.failure");
    assert!(fs::symlink_metadata(&config_path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&target_path).unwrap(), original_target);
    assert!(!worktree.join(".qgh.toml").exists());
}

#[cfg(unix)]
#[test]
fn init_rejects_symlinked_repo_policy_without_mutating_its_target() {
    use std::os::unix::fs::symlink;

    let fixture = TestFixture::new("init-symlinked-repo-policy");
    let target_path = fixture.root.join("managed-policy.toml");
    fs::write(
        &target_path,
        "schema_version = \"qgh.repo.v1\"\n\n[repo]\ngithub = \"owner/original\"\n",
    )
    .unwrap();
    let original_target = fs::read(&target_path).unwrap();
    symlink(&target_path, fixture.root.join(".qgh.toml")).unwrap();
    let worktree = fixture.init_git_worktree_with_origin("https://github.com/owner/new.git");

    let init = fixture.qgh_without_profile_in(
        &worktree,
        ["--profile", "work", "init", "--yes", "--force", "--json"],
    );

    assert_eq!(init.status.code(), Some(6));
    assert_eq!(stdout_json(&init)["error"]["code"], "storage.failure");
    assert_eq!(fs::read(&target_path).unwrap(), original_target);
    assert!(fs::symlink_metadata(fixture.root.join(".qgh.toml"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!fixture.config_home.join("qgh/config.toml").exists());
}

#[cfg(unix)]
#[test]
fn init_repo_rejects_symlinked_policy_without_mutating_its_target() {
    use std::os::unix::fs::symlink;

    let fixture = TestFixture::new("init-repo-symlinked-policy");
    let target_path = fixture.root.join("managed-policy.toml");
    fs::write(
        &target_path,
        "schema_version = \"qgh.repo.v1\"\n\n[repo]\ngithub = \"owner/original\"\n",
    )
    .unwrap();
    let original_target = fs::read(&target_path).unwrap();
    symlink(&target_path, fixture.root.join(".qgh.toml")).unwrap();
    let worktree = fixture.init_git_worktree();

    let init = fixture.qgh_without_profile_in(
        &worktree,
        ["init", "repo", "--repo", "owner/new", "--force", "--json"],
    );

    assert_eq!(init.status.code(), Some(6));
    assert_eq!(stdout_json(&init)["error"]["code"], "storage.failure");
    assert_eq!(fs::read(&target_path).unwrap(), original_target);
    assert!(fs::symlink_metadata(fixture.root.join(".qgh.toml"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn init_validation_failure_preserves_existing_profile_config_bytes() {
    let fixture = TestFixture::new("init-invalid-existing-profile-preserved");
    let config_path = fixture.config_home.join("qgh/config.toml");
    fs::write(
        &config_path,
        r#"schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/repo"]
max_in_flight_requests = 17

[profiles.work.token_source]
type = "github_cli"
"#,
    )
    .unwrap();
    let original = fs::read(&config_path).unwrap();
    let worktree = fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(
        &worktree,
        [
            "--profile",
            "work",
            "init",
            "--yes",
            "--repo",
            "other/repo",
            "--host",
            "github.com",
            "--api-base-url",
            "https://api.github.com",
            "--web-base-url",
            "https://github.com",
            "--token-source",
            "github_cli",
            "--json",
        ],
    );

    assert_eq!(init.status.code(), Some(2));
    assert_eq!(stdout_json(&init)["error"]["code"], "config.invalid");
    assert_eq!(fs::read(&config_path).unwrap(), original);
    assert!(!worktree.join(".qgh.toml").exists());
}

#[test]
fn init_rejects_an_invalid_untouched_profile_before_publishing_config() {
    let fixture = TestFixture::new("init-invalid-untouched-profile-preserved");
    let config_path = fixture.config_home.join("qgh/config.toml");
    fs::write(
        &config_path,
        r#"schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "github_cli"

[profiles.broken]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/existing"]
max_in_flight_requests = 17

[profiles.broken.token_source]
type = "github_cli"
"#,
    )
    .unwrap();
    let original = fs::read(&config_path).unwrap();
    let worktree = fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(
        &worktree,
        [
            "--profile",
            "work",
            "init",
            "--yes",
            "--repo",
            "other/repo",
            "--host",
            "github.com",
            "--api-base-url",
            "https://api.github.com",
            "--web-base-url",
            "https://github.com",
            "--token-source",
            "github_cli",
            "--json",
        ],
    );

    assert_eq!(init.status.code(), Some(2));
    assert_eq!(stdout_json(&init)["error"]["code"], "config.invalid");
    assert_eq!(fs::read(&config_path).unwrap(), original);
    assert!(!worktree.join(".qgh.toml").exists());
}

#[test]
fn init_preset_preview_accepts_defaults_with_enter_before_writing() {
    let fixture = TestFixture::new("init-preset-accept");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init =
        fixture.qgh_without_profile_in_with_stdin(&nested_worktree_dir, ["init", "--json"], "\n");
    assert_success(&init);
    let stderr = stderr_text(&init);
    assert!(stderr.contains("Detected qgh init defaults"));
    assert!(stderr.contains("repo: owner/repo"));
    assert!(stderr.contains("host: github.com"));
    assert!(stderr.contains("profile id: github"));
    assert!(stderr.contains("token source: github_cli"));
    assert!(stderr.contains("config path:"));
    assert!(stderr.contains("repo policy path:"));
    assert!(stderr.contains("db path:"));
    assert!(stderr.contains("Use these defaults? [Y/n]"));

    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["profile_id"], "github");
    assert_eq!(init_json["data"]["repo"], "owner/repo");
    assert_eq!(init_json["data"]["token_source"]["kind"], "github_cli");
    assert_eq!(init_json["data"]["repo_policy_action"], "created");

    let config_text = fs::read_to_string(fixture.config_home.join("qgh/config.toml")).unwrap();
    assert!(config_text.contains(r#"type = "github_cli""#));
    assert!(!config_text.contains("qgh.sqlite3"));
    assert!(fixture.root.join(".qgh.toml").exists());
}

#[test]
fn init_preset_preview_no_enters_custom_flow_instead_of_canceling() {
    let fixture = TestFixture::new("init-preset-custom");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in_with_stdin(
        &nested_worktree_dir,
        ["init", "--json"],
        "n\ncustom\n\n\n\nenv\nQGH_TEST_TOKEN\nn\n",
    );
    assert_success(&init);
    let stderr = stderr_text(&init);
    assert!(stderr.contains("Use these defaults? [Y/n]"));
    assert!(stderr.contains("profile id"));
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["profile_id"], "custom");
    assert_eq!(init_json["data"]["token_source"]["kind"], "env");
    assert_eq!(init_json["data"]["repo_policy_action"], "skipped");
    assert!(!fixture.root.join(".qgh.toml").exists());
}

#[test]
fn init_yes_applies_inferred_preset_without_preview_or_required_flags() {
    let fixture = TestFixture::new("init-yes-preset");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(&nested_worktree_dir, ["init", "--yes", "--json"]);
    assert_success(&init);
    assert!(!stderr_text(&init).contains("Use these defaults?"));
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["profile_id"], "github");
    assert_eq!(init_json["data"]["repo"], "owner/repo");
    assert_eq!(init_json["data"]["token_source"]["kind"], "github_cli");
    assert_eq!(init_json["data"]["repo_policy_action"], "created");
}

#[test]
fn init_yes_auto_reuses_single_same_host_profile_and_reports_actual_id() {
    let fixture = TestFixture::new("init-yes-auto-host-match");
    fixture.write_config_with_host("github.com", "https://api.github.com");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/other.git");

    let init = fixture.qgh_without_profile_in(&nested_worktree_dir, ["init", "--yes", "--json"]);

    assert_success(&init);
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["profile_id"], "work");
    assert_eq!(init_json["meta"]["profile_id"], "work");
    assert_eq!(init_json["meta"]["profile_source"], "cli");
    assert_eq!(init_json["data"]["profile_action"], "updated");
    assert_eq!(init_json["data"]["repo_allowlist_action"], "added");
    assert_eq!(init_json["data"]["token_source"]["kind"], "env");
    let config = fs::read_to_string(fixture.config_home.join("qgh/config.toml")).unwrap();
    assert!(config.contains("[profiles.work]"));
    assert!(config.contains(r#""owner/repo""#));
    assert!(config.contains(r#""owner/other""#));
    assert!(!config.contains("[profiles.github]"));
}

#[test]
fn init_yes_normalizes_mixed_case_host_identity() {
    let fixture = TestFixture::new("init-yes-normalized-host");
    let nested_worktree_dir = fixture.init_git_worktree();

    let init = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "init",
            "--yes",
            "--repo",
            "owner/repo",
            "--host",
            "GitHub.com",
            "--json",
        ],
    );

    assert_success(&init);
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["profile_id"], "github");
    assert_eq!(init_json["meta"]["profile_source"], "cli");
    let config = fs::read_to_string(fixture.config_home.join("qgh/config.toml")).unwrap();
    assert!(config.contains(r#"host = "github.com""#));
    assert!(config.contains(r#"api_base_url = "https://api.github.com""#));
    assert!(!config.contains("GitHub.com"));
}

#[test]
fn init_yes_auto_rejects_ambiguous_repo_matches_without_writes() {
    let fixture = TestFixture::new("init-yes-auto-ambiguous-repo");
    let config_path = fixture.config_home.join("qgh/config.toml");
    fs::write(
        &config_path,
        r#"schema_version = "qgh.config.v1"

[profiles.github]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.github.token_source]
type = "github_cli"

[profiles.work]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#,
    )
    .unwrap();
    let original = fs::read(&config_path).unwrap();
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(&nested_worktree_dir, ["init", "--yes", "--json"]);

    assert_eq!(init.status.code(), Some(2));
    let error = stdout_json(&init);
    assert_eq!(error["error"]["code"], "config.ambiguous_profile");
    assert_eq!(error["error"]["details"]["match_basis"], "repo");
    assert_eq!(
        error["error"]["details"]["matching_profile_ids"],
        json!(["github", "work"])
    );
    assert_eq!(fs::read(config_path).unwrap(), original);
    assert!(!fixture.root.join(".qgh.toml").exists());
}

#[test]
fn init_yes_auto_rejects_ambiguous_host_matches_without_writes() {
    let fixture = TestFixture::new("init-yes-auto-ambiguous-host");
    let config_path = fixture.config_home.join("qgh/config.toml");
    fs::write(
        &config_path,
        r#"schema_version = "qgh.config.v1"

[profiles.github]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/one"]

[profiles.github.token_source]
type = "github_cli"

[profiles.work]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/two"]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#,
    )
    .unwrap();
    let original = fs::read(&config_path).unwrap();
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(&nested_worktree_dir, ["init", "--yes", "--json"]);

    assert_eq!(init.status.code(), Some(2));
    let error = stdout_json(&init);
    assert_eq!(error["error"]["code"], "config.ambiguous_profile");
    assert_eq!(error["error"]["details"]["match_basis"], "host");
    assert_eq!(fs::read(config_path).unwrap(), original);
    assert!(!fixture.root.join(".qgh.toml").exists());
}

#[test]
fn init_yes_auto_validates_snapshot_before_reporting_ambiguity() {
    let fixture = TestFixture::new("init-yes-auto-invalid-before-ambiguity");
    let config_path = fixture.config_home.join("qgh/config.toml");
    fs::write(
        &config_path,
        r#"schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/one"]

[profiles.work.token_source]
type = "github_cli"

[profiles."PRIVATE_INVALID!"]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/two"]

[profiles."PRIVATE_INVALID!".token_source]
type = "github_cli"
"#,
    )
    .unwrap();
    let original = fs::read(&config_path).unwrap();
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(&nested_worktree_dir, ["init", "--yes", "--json"]);

    assert_eq!(init.status.code(), Some(2));
    assert_eq!(stdout_json(&init)["error"]["code"], "config.invalid");
    assert!(!stdout_text(&init).contains("PRIVATE_INVALID"));
    assert_eq!(fs::read(config_path).unwrap(), original);
    assert!(!fixture.root.join(".qgh.toml").exists());
}

#[test]
fn interactive_init_validates_snapshot_before_previewing_profile_paths() {
    let fixture = TestFixture::new("init-interactive-invalid-before-preview");
    let config_path = fixture.config_home.join("qgh/config.toml");
    fs::write(
        &config_path,
        r#"schema_version = "qgh.config.v1"

[profiles."PRIVATE_INVALID!"]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles."PRIVATE_INVALID!".token_source]
type = "github_cli"
"#,
    )
    .unwrap();
    let original = fs::read(&config_path).unwrap();
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init =
        fixture.qgh_without_profile_in_with_stdin(&nested_worktree_dir, ["init", "--json"], "");

    assert_eq!(init.status.code(), Some(2));
    assert_eq!(stdout_json(&init)["error"]["code"], "config.invalid");
    assert!(!stderr_text(&init).contains("PRIVATE_INVALID"));
    assert!(!stdout_text(&init).contains("PRIVATE_INVALID"));
    assert_eq!(fs::read(config_path).unwrap(), original);
    assert!(!fixture.root.join(".qgh.toml").exists());
}

#[test]
fn init_yes_auto_reuses_existing_inferred_endpoints_but_rejects_explicit_conflict() {
    let fixture = TestFixture::new("init-yes-auto-existing-endpoints");
    fixture.write_config("http://127.0.0.1:1");
    let config_path = fixture.config_home.join("qgh/config.toml");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/other.git");

    let inferred =
        fixture.qgh_without_profile_in(&nested_worktree_dir, ["init", "--yes", "--json"]);
    assert_success(&inferred);
    let inferred_json = stdout_json(&inferred);
    assert_eq!(inferred_json["data"]["profile_id"], "work");
    assert_eq!(inferred_json["data"]["profile_action"], "updated");
    let after_inferred = fs::read(&config_path).unwrap();
    assert!(String::from_utf8_lossy(&after_inferred).contains("http://127.0.0.1:1"));

    let explicit = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "init",
            "--yes",
            "--repo",
            "owner/third",
            "--api-base-url",
            "https://api.github.com",
            "--force",
            "--json",
        ],
    );
    assert_eq!(explicit.status.code(), Some(2));
    assert_eq!(stdout_json(&explicit)["error"]["code"], "config.invalid");
    assert_eq!(fs::read(config_path).unwrap(), after_inferred);
}

#[test]
fn interactive_customize_preserves_existing_endpoint_defaults() {
    let fixture = TestFixture::new("init-custom-existing-endpoints");
    fixture.write_config("http://127.0.0.1:1");
    let config_path = fixture.config_home.join("qgh/config.toml");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/other.git");

    let init = fixture.qgh_without_profile_in_with_stdin(
        &nested_worktree_dir,
        ["--profile", "work", "init", "--json"],
        "n\n\n\n\n\n\n",
    );

    assert_success(&init);
    assert_eq!(stdout_json(&init)["data"]["profile_id"], "work");
    assert!(stderr_text(&init).contains("token source: env"));
    assert!(!stderr_text(&init).contains("token source (github_cli/env)"));
    let config = fs::read_to_string(config_path).unwrap();
    assert!(config.contains(r#"api_base_url = "http://127.0.0.1:1""#));
    assert!(config.contains(r#""owner/other""#));
}

#[test]
fn init_uses_defaults_for_an_explicit_host_instead_of_mismatched_origin_endpoints() {
    let fixture = TestFixture::new("init-explicit-host-endpoints");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        ["init", "--yes", "--host", "ghe.example", "--json"],
    );

    assert_success(&init);
    let config = fs::read_to_string(fixture.config_home.join("qgh/config.toml")).unwrap();
    assert!(config.contains(r#"host = "ghe.example""#));
    assert!(config.contains(r#"api_base_url = "https://ghe.example/api/v3""#));
    assert!(config.contains(r#"web_base_url = "https://ghe.example""#));
    assert!(!config.contains("api.github.com"));
}

#[test]
fn init_normalizes_mixed_case_github_origin_and_reports_git_remote_provenance() {
    let fixture = TestFixture::new("init-mixed-case-origin");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://GitHub.COM/owner/repo.git");

    let init = fixture.qgh_without_profile_in(&nested_worktree_dir, ["init", "--yes", "--json"]);

    assert_success(&init);
    let json = stdout_json(&init);
    assert_eq!(json["data"]["profile_id"], "github");
    assert_eq!(json["meta"]["repo_source"], "git_remote");
    assert!(json["data"]["repo_policy_path"].is_string());
    assert_eq!(json["meta"]["repo_policy_path"], Value::Null);
    let config = fs::read_to_string(fixture.config_home.join("qgh/config.toml")).unwrap();
    assert!(config.contains(r#"host = "github.com""#));
    assert!(config.contains(r#"api_base_url = "https://api.github.com""#));
}

#[test]
fn top_level_init_honors_env_profile_and_cli_precedence() {
    let fixture = TestFixture::new("init-profile-env-precedence");
    fs::write(
        fixture.config_home.join("qgh/config.toml"),
        r#"schema_version = "qgh.config.v1"

[profiles.env]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/one"]

[profiles.env.token_source]
type = "github_cli"

[profiles.cli]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/two"]

[profiles.cli.token_source]
type = "github_cli"
"#,
    )
    .unwrap();
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/env-target.git");

    let mut env_command = fixture.base_command();
    let env_init = env_command
        .env("QGH_PROFILE", "env")
        .current_dir(&nested_worktree_dir)
        .args(["init", "--yes", "--json"])
        .output()
        .unwrap();
    assert_success(&env_init);
    let env_json = stdout_json(&env_init);
    assert_eq!(env_json["data"]["profile_id"], "env");
    assert_eq!(env_json["meta"]["profile_source"], "env");

    let mut cli_command = fixture.base_command();
    let cli_init = cli_command
        .env("QGH_PROFILE", "env")
        .current_dir(&nested_worktree_dir)
        .args([
            "--profile",
            "cli",
            "init",
            "--yes",
            "--repo",
            "owner/cli-target",
            "--force",
            "--json",
        ])
        .output()
        .unwrap();
    assert_success(&cli_init);
    let cli_json = stdout_json(&cli_init);
    assert_eq!(cli_json["data"]["profile_id"], "cli");
    assert_eq!(cli_json["meta"]["profile_source"], "cli");
}

#[test]
fn interactive_customize_keeps_explicit_profile_without_reprompting() {
    let fixture = TestFixture::new("init-interactive-fixed-profile");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in_with_stdin(
        &nested_worktree_dir,
        ["--profile", "fixed", "init", "--json"],
        "n\n\n\n\n\nn\n",
    );

    assert_success(&init);
    assert_eq!(stdout_json(&init)["data"]["profile_id"], "fixed");
    assert_eq!(stderr_text(&init).matches("profile id").count(), 1);
}

#[test]
fn init_yes_rejects_token_env_without_token_source() {
    let fixture = TestFixture::new("init-yes-token-env-without-source");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        ["init", "--yes", "--token-env", "QGH_TEST_TOKEN", "--json"],
    );
    assert_eq!(init.status.code(), Some(2));
    let init_json = stdout_json(&init);
    assert_eq!(init_json["error"]["code"], "validation.missing_init_value");
    assert!(init_json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("--token-source"));
    assert!(!fixture.config_home.join("qgh/config.toml").exists());
    assert!(!fixture.root.join(".qgh.toml").exists());
}

#[test]
fn init_interactive_token_source_env_prompts_for_token_env() {
    let fixture = TestFixture::new("init-token-source-env-prompt");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in_with_stdin(
        &nested_worktree_dir,
        ["init", "--token-source", "env", "--json"],
        "QGH_TEST_TOKEN\n\n",
    );
    assert_success(&init);
    let stderr = stderr_text(&init);
    assert!(stderr.contains("token env var [GITHUB_TOKEN]"));
    assert!(stderr.contains("token source: env (QGH_TEST_TOKEN)"));
    assert!(stderr.contains("Use these defaults? [Y/n]"));
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["token_source"]["kind"], "env");

    let config_text = fs::read_to_string(fixture.config_home.join("qgh/config.toml")).unwrap();
    assert!(config_text.contains(r#"type = "env""#));
    assert!(config_text.contains(r#"env = "QGH_TEST_TOKEN""#));
    assert!(!config_text.contains(r#"type = "github_cli""#));
}

#[test]
fn init_without_json_prints_human_summary_for_profile_and_repo_policy_paths() {
    let fixture = TestFixture::new("init-human-summary");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(&nested_worktree_dir, ["init", "--yes"]);
    assert_success(&init);
    assert!(stderr_text(&init).is_empty());
    let stdout = stdout_text(&init);
    assert!(!stdout.starts_with('{'));
    assert!(stdout.contains("qgh init complete"));
    assert!(stdout.contains("profile: github (created)"));
    assert!(stdout.contains("repo: owner/repo (allowlist added)"));
    assert!(stdout.contains("token source: github_cli"));
    assert!(stdout.contains("config:"));
    assert!(stdout.contains("repo policy: created at"));
    assert_eq!(
        stdout.contains("next: qgh model install qwen3-embedding-0.6b"),
        cfg!(feature = "fastembed-provider")
    );
    assert!(stdout.contains("next: qgh sync"));
    assert!(stdout.contains("next: qgh query <terms>"));

    let repo_fixture = TestFixture::new("init-repo-human-summary");
    repo_fixture.write_config("http://127.0.0.1:1");
    let repo_worktree = repo_fixture.init_git_worktree();
    let init_repo = repo_fixture.qgh_in(&repo_worktree, ["init", "repo", "--repo", "owner/repo"]);
    assert_success(&init_repo);
    let repo_stdout = stdout_text(&init_repo);
    assert!(repo_stdout.contains("qgh init repo complete"));
    assert!(repo_stdout.contains("repo: owner/repo (cli)"));
    assert!(repo_stdout.contains("repo policy:"));
    assert!(repo_stdout.contains("profile check: validated"));
}

#[test]
fn init_human_warnings_use_stderr_without_polluting_summary_stdout() {
    let fixture = TestFixture::new("init-human-warning-stream");
    let worktree = fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let output = fixture.qgh_without_profile_in(&worktree, ["init", "repo"]);
    assert_success(&output);
    let stdout = stdout_text(&output);
    let stderr = stderr_text(&output);
    assert!(stdout.contains("qgh init repo complete"));
    assert!(stdout.contains("profile check: not_checked"));
    assert!(!stdout.contains("config.profile_not_checked"));
    assert!(stderr.contains("warning [config.profile_not_checked]"));
}

#[test]
fn init_repo_inputs_with_secret_suffixes_fail_content_free() {
    let fixture = TestFixture::new("init-secret-repo-inputs");
    let worktree = fixture.init_git_worktree();
    let marker = "owner/repo?token=PRIVATE_INIT_REPO_MARKER";

    let repo_only =
        fixture.qgh_without_profile_in(&worktree, ["init", "repo", "--repo", marker, "--json"]);
    assert_eq!(repo_only.status.code(), Some(2));
    assert_eq!(
        stdout_json(&repo_only)["error"]["code"],
        "validation.invalid_repo"
    );
    assert!(
        !format!("{}{}", stdout_text(&repo_only), stderr_text(&repo_only))
            .contains("PRIVATE_INIT_REPO_MARKER")
    );

    let preset = fixture.qgh_without_profile_in(
        &worktree,
        [
            "init",
            "--yes",
            "--repo",
            marker,
            "--host",
            "github.com",
            "--api-base-url",
            "https://api.github.com",
            "--web-base-url",
            "https://github.com",
            "--token-source",
            "env",
            "--token-env",
            "QGH_TEST_TOKEN",
            "--json",
        ],
    );
    assert_eq!(preset.status.code(), Some(2));
    assert_eq!(
        stdout_json(&preset)["error"]["code"],
        "validation.invalid_repo"
    );
    assert!(!format!("{}{}", stdout_text(&preset), stderr_text(&preset))
        .contains("PRIVATE_INIT_REPO_MARKER"));
}

#[test]
fn init_short_yes_alias_applies_inferred_preset() {
    let fixture = TestFixture::new("init-short-yes");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("git@ghe.internal.example:team/repo.git");

    let init = fixture.qgh_without_profile_in(&nested_worktree_dir, ["init", "-y", "--json"]);
    assert_success(&init);
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["profile_id"], "ghe");
    assert_eq!(init_json["data"]["repo"], "team/repo");
    assert_eq!(init_json["data"]["token_source"]["kind"], "github_cli");

    let config_text = fs::read_to_string(fixture.config_home.join("qgh/config.toml")).unwrap();
    assert!(config_text.contains(r#"host = "ghe.internal.example""#));
    assert!(config_text.contains(r#"api_base_url = "https://ghe.internal.example/api/v3""#));
}

#[test]
fn init_default_profile_reuses_existing_profile_for_repo_and_host() {
    let fixture = TestFixture::new("init-default-profile-reuse");
    fixture.write_config_with_host("github.com", "https://api.github.com");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(&nested_worktree_dir, ["init", "--yes", "--json"]);
    assert_success(&init);
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["profile_id"], "work");
    assert_eq!(init_json["data"]["profile_action"], "updated");
    assert_eq!(
        init_json["data"]["repo_allowlist_action"],
        "already_present"
    );
    assert_eq!(init_json["warnings"], json!([]));
}

#[test]
fn init_warns_when_repo_already_allowlisted_in_other_profile() {
    let fixture = TestFixture::new("init-duplicate-allowlist-warning");
    fixture.write_config_with_host("github.com", "https://api.github.com");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        ["--profile", "fresh", "init", "--yes", "--json"],
    );
    assert_success(&init);
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["profile_id"], "fresh");
    assert_eq!(init_json["data"]["profile_action"], "created");
    assert_eq!(
        init_json["warnings"][0]["code"],
        "config.duplicate_repo_allowlist"
    );
    assert_eq!(
        json_object_keys(&init_json["warnings"][0]),
        BTreeSet::from([
            "code".to_string(),
            "message".to_string(),
            "severity".to_string(),
        ])
    );
    assert!(init_json["warnings"][0]["message"]
        .as_str()
        .unwrap()
        .contains("work"));

    let human_init = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        ["--profile", "fresh", "init", "--yes", "--force"],
    );
    assert_success(&human_init);
    assert!(!stdout_text(&human_init).contains("config.duplicate_repo_allowlist"));
    assert!(stderr_text(&human_init).contains("warning [config.duplicate_repo_allowlist]"));
}

#[test]
fn repo_policy_scope_uses_git_remote_host_to_disambiguate_profiles() {
    let fixture = TestFixture::new("policy-scope-remote-host");
    fs::write(
        fixture.config_home.join("qgh/config.toml"),
        r#"
schema_version = "qgh.config.v1"

[profiles.ghe]
host = "ghe.example.com"
api_base_url = "https://ghe.example.com/api/v3"
web_base_url = "https://ghe.example.com"
repos = ["owner/repo"]

[profiles.ghe.token_source]
type = "env"
env = "QGH_TEST_TOKEN"

[profiles.hub]
host = "github.com"
api_base_url = "http://127.0.0.1:1"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.hub.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#,
    )
    .unwrap();
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");
    fixture.write_repo_policy("owner/repo");

    let status = fixture.qgh_without_profile_in(&nested_worktree_dir, ["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["meta"]["profile_id"], "hub");
    assert_eq!(status_json["meta"]["profile_source"], "single_match");
}

#[test]
fn init_preset_eof_cancels_without_writing_files() {
    let fixture = TestFixture::new("init-preset-eof");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init =
        fixture.qgh_without_profile_in_with_stdin(&nested_worktree_dir, ["init", "--json"], "");
    assert_eq!(init.status.code(), Some(2));
    let init_json = stdout_json(&init);
    assert_eq!(init_json["error"]["code"], "validation.init_cancelled");
    let stderr = stderr_text(&init);
    assert!(stderr.contains("qgh init canceled"));
    assert!(stderr.contains("no files changed"));
    assert!(!fixture.config_home.join("qgh/config.toml").exists());
    assert!(!fixture.root.join(".qgh.toml").exists());
}

#[test]
fn credential_store_token_source_fails_config_validation_before_auth_resolution() {
    let fixture = TestFixture::new("credential-store-token-source");
    fixture.write_config_with_credential_store("http://127.0.0.1:1");

    let sync = fixture.qgh(["sync", "--json"]);
    assert_eq!(sync.status.code(), Some(2));
    let sync_json = stdout_json(&sync);
    assert_eq!(
        sync_json["error"]["code"],
        "validation.invalid_token_source"
    );
    assert!(sync_json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("github_cli"));
    assert!(!stdout_text(&sync).contains("auth.token_unavailable"));
    assert!(!stdout_text(&sync).contains("credential_store token resolution"));

    let status = fixture.qgh(["status", "--json"]);
    assert_eq!(status.status.code(), Some(2));
    assert_eq!(
        stdout_json(&status)["error"]["code"],
        "validation.invalid_token_source"
    );
}

#[test]
fn init_yes_with_explicit_values_does_not_require_origin_remote() {
    let fixture = TestFixture::new("init-yes-no-origin");
    let nested_worktree_dir = fixture.init_git_worktree();

    let init = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "init",
            "--yes",
            "--profile",
            "work",
            "--repo",
            "owner/repo",
            "--host",
            "github.com",
            "--api-base-url",
            "https://api.github.com",
            "--web-base-url",
            "https://github.com",
            "--token-source",
            "env",
            "--token-env",
            "QGH_TEST_TOKEN",
            "--json",
        ],
    );
    assert_success(&init);
    assert!(fixture.config_home.join("qgh/config.toml").exists());
    assert!(fixture.root.join(".qgh.toml").exists());
}

#[test]
fn init_interactive_wizard_reads_stdin_defaults_and_bootstraps_profile() {
    let fixture = TestFixture::new("init-interactive");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in_with_stdin(
        &nested_worktree_dir,
        ["init", "--json"],
        "n\nwork\n\n\n\nenv\nQGH_TEST_TOKEN\ny\n",
    );
    assert_success(&init);
    assert!(stdout_text(&init).starts_with('{'));
    assert!(stderr_text(&init).contains("profile id"));
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["profile_id"], "work");
    assert_eq!(init_json["data"]["profile_action"], "created");
    assert_eq!(init_json["data"]["repo"], "owner/repo");
    assert_eq!(init_json["data"]["token_source"]["kind"], "env");
    assert_eq!(init_json["data"]["repo_policy_action"], "created");

    let config_text = fs::read_to_string(fixture.config_home.join("qgh/config.toml")).unwrap();
    assert!(config_text.contains(r#"host = "github.com""#));
    assert!(config_text.contains(r#"api_base_url = "https://api.github.com""#));
    assert!(config_text.contains(r#"web_base_url = "https://github.com""#));
    assert!(config_text.contains(r#"env = "QGH_TEST_TOKEN""#));
    assert!(!config_text.contains("fixture-token"));
}

#[test]
fn init_yes_infers_ghes_defaults_from_origin_remote() {
    let fixture = TestFixture::new("init-ghes-defaults");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("git@ghe.internal.example:team/repo.git");

    let init = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "init",
            "--yes",
            "--profile",
            "enterprise",
            "--token-source",
            "github_cli",
            "--json",
        ],
    );
    assert_success(&init);
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["repo"], "team/repo");
    assert_eq!(init_json["data"]["token_source"]["kind"], "github_cli");

    let config_text = fs::read_to_string(fixture.config_home.join("qgh/config.toml")).unwrap();
    assert!(config_text.contains(r#"host = "ghe.internal.example""#));
    assert!(config_text.contains(r#"api_base_url = "https://ghe.internal.example/api/v3""#));
    assert!(config_text.contains(r#"web_base_url = "https://ghe.internal.example""#));
    assert!(config_text.contains(r#"repos = ["team/repo"]"#));
    assert!(config_text.contains(r#"type = "github_cli""#));
}

#[test]
fn init_yes_strips_userinfo_from_origin_defaults_before_writing_config() {
    let fixture = TestFixture::new("init-origin-userinfo");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://alice:ghp_secret@ghe.example/team/repo.git");

    let init = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "init",
            "--yes",
            "--profile",
            "enterprise",
            "--token-source",
            "github_cli",
            "--json",
        ],
    );
    assert_success(&init);
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["repo"], "team/repo");

    let config_text = fs::read_to_string(fixture.config_home.join("qgh/config.toml")).unwrap();
    assert!(config_text.contains(r#"host = "ghe.example""#));
    assert!(config_text.contains(r#"api_base_url = "https://ghe.example/api/v3""#));
    assert!(config_text.contains(r#"web_base_url = "https://ghe.example""#));
    for forbidden in ["alice", "ghp_secret", "alice:ghp_secret@ghe.example"] {
        assert!(
            !config_text.contains(forbidden),
            "config must not persist origin credentials: {config_text}"
        );
        assert!(
            !stdout_text(&init).contains(forbidden),
            "init output must not expose origin credentials: {}",
            stdout_text(&init)
        );
    }
}

#[test]
fn init_yes_rejects_explicit_host_userinfo_without_writing_config() {
    let fixture = TestFixture::new("init-explicit-host-userinfo");
    let nested_worktree_dir = fixture.init_git_worktree();

    let init = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "init",
            "--yes",
            "--profile",
            "work",
            "--repo",
            "owner/repo",
            "--host",
            "alice:ghp_secret@github.com",
            "--api-base-url",
            "https://api.github.com",
            "--web-base-url",
            "https://github.com",
            "--token-source",
            "github_cli",
            "--json",
        ],
    );
    assert_eq!(init.status.code(), Some(2));
    let init_json = stdout_json(&init);
    assert_eq!(init_json["error"]["code"], "validation.invalid_host");
    assert!(!stdout_text(&init).contains("ghp_secret"));
    assert!(!fixture.config_home.join("qgh/config.toml").exists());
    assert!(!fixture.root.join(".qgh.toml").exists());
}

#[test]
fn init_yes_rejects_conflicting_token_source_for_existing_profile() {
    let fixture = TestFixture::new("init-existing-token-source-conflict");
    fixture.write_config("https://api.github.com");
    let config_path = fixture.config_home.join("qgh/config.toml");
    let original = fs::read(&config_path).unwrap();
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "init",
            "--yes",
            "--profile",
            "work",
            "--repo",
            "owner/repo",
            "--host",
            "github.com",
            "--api-base-url",
            "https://api.github.com",
            "--web-base-url",
            "https://github.com",
            "--token-source",
            "github_cli",
            "--json",
        ],
    );
    assert_eq!(init.status.code(), Some(2));
    assert_eq!(stdout_json(&init)["error"]["code"], "config.invalid");
    assert_eq!(fs::read(config_path).unwrap(), original);
    assert!(!fixture.root.join(".qgh.toml").exists());
}

#[test]
fn init_yes_auto_rejects_conflicting_token_source_after_locked_selection() {
    let fixture = TestFixture::new("init-auto-token-source-conflict");
    fixture.write_config("https://api.github.com");
    let config_path = fixture.config_home.join("qgh/config.toml");
    let original = fs::read(&config_path).unwrap();
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/other.git");

    let init = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        ["init", "--yes", "--token-source", "github_cli", "--json"],
    );

    assert_eq!(init.status.code(), Some(2));
    assert_eq!(stdout_json(&init)["error"]["code"], "config.invalid");
    assert_eq!(fs::read(config_path).unwrap(), original);
    assert!(!fixture.root.join(".qgh.toml").exists());
}

#[test]
fn interactive_preview_reports_existing_profile_token_source() {
    let fixture = TestFixture::new("init-existing-token-source-preview");
    fixture.write_config("https://api.github.com");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let init =
        fixture.qgh_without_profile_in_with_stdin(&nested_worktree_dir, ["init", "--json"], "\n");

    assert_success(&init);
    assert!(stderr_text(&init).contains("token source: env (QGH_TEST_TOKEN)"));
    assert!(!stderr_text(&init).contains("token source: github_cli"));
    assert_eq!(stdout_json(&init)["data"]["token_source"]["kind"], "env");
}

#[test]
fn init_yes_rejects_existing_policy_for_different_repo_without_config_mutation() {
    let fixture = TestFixture::new("init-policy-mismatch");
    fixture.write_repo_policy("owner/repo");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/other/repo.git");

    let init = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "init",
            "--yes",
            "--profile",
            "work",
            "--token-source",
            "github_cli",
            "--json",
        ],
    );
    assert_eq!(init.status.code(), Some(2));
    let init_json = stdout_json(&init);
    assert_eq!(init_json["error"]["code"], "config.repo_policy_exists");
    assert_eq!(init_json["error"]["details"]["existing_repo"], "owner/repo");
    assert_eq!(
        init_json["error"]["details"]["requested_repo"],
        "other/repo"
    );
    assert!(!fixture.config_home.join("qgh/config.toml").exists());

    let policy = fs::read_to_string(fixture.root.join(".qgh.toml")).unwrap();
    assert!(policy.contains(r#"github = "owner/repo""#));
    assert!(!policy.contains(r#"github = "other/repo""#));
}

#[test]
fn init_yes_missing_required_values_fails_without_prompting() {
    let fixture = TestFixture::new("init-yes-missing");
    let nested_worktree_dir = fixture.init_git_worktree();

    let init = fixture.qgh_without_profile_in(&nested_worktree_dir, ["init", "--yes", "--json"]);
    assert_eq!(init.status.code(), Some(2));
    let init_json = stdout_json(&init);
    assert_eq!(init_json["error"]["code"], "config.git_remote_unavailable");
    assert!(stderr_text(&init).is_empty());
    assert!(!fixture.config_home.join("qgh/config.toml").exists());
    assert!(!fixture.root.join(".qgh.toml").exists());
}

#[test]
fn init_infers_repo_from_github_origin_remote_without_profile_resolution() {
    for (case, remote) in [
        ("https-dot-git", "https://github.com/owner/repo.git"),
        ("https-no-dot-git", "https://github.com/owner/repo"),
        ("ssh-dot-git", "git@github.com:owner/repo.git"),
    ] {
        let fixture = TestFixture::new(&format!("init-origin-{case}"));
        let nested_worktree_dir = fixture.init_git_worktree_with_origin(remote);

        let init = fixture.qgh_without_profile_in(&nested_worktree_dir, ["init", "repo", "--json"]);
        assert_success(&init);
        let init_json = stdout_json(&init);
        assert_eq!(init_json["data"]["repo"], "owner/repo");
        assert_eq!(init_json["data"]["repo_source"], "git_remote");
        assert_eq!(init_json["meta"]["repo_source"], "git_remote");
        assert_eq!(
            init_json["data"]["profile_validation"]["status"],
            "not_checked"
        );
        assert_eq!(
            init_json["warnings"][0]["code"],
            "config.profile_not_checked"
        );
        assert_eq!(init_json["warnings"][0]["severity"], "warn");
        assert!(fixture.root.join(".qgh.toml").exists());
    }
}

#[test]
fn init_fails_outside_git_worktree_without_writing_policy() {
    let fixture = TestFixture::new("init-no-worktree");

    let init = fixture.qgh_without_profile(["init", "repo", "--repo", "owner/repo", "--json"]);
    assert_eq!(init.status.code(), Some(2));
    let init_json = stdout_json(&init);
    assert_eq!(init_json["error"]["code"], "config.no_git_worktree");
    assert!(!fixture.root.join(".qgh.toml").exists());
}

#[test]
fn init_fails_for_missing_malformed_or_non_github_origin() {
    let missing = TestFixture::new("init-missing-origin");
    let missing_nested = missing.init_git_worktree();
    let missing_output =
        missing.qgh_without_profile_in(&missing_nested, ["init", "repo", "--json"]);
    assert_eq!(missing_output.status.code(), Some(2));
    assert_eq!(
        stdout_json(&missing_output)["error"]["code"],
        "config.git_remote_unavailable"
    );
    assert!(!missing.root.join(".qgh.toml").exists());

    let malformed = TestFixture::new("init-bad-origin-malformed");
    let malformed_nested = malformed.init_git_worktree_with_origin("not a github remote");
    let malformed_output =
        malformed.qgh_without_profile_in(&malformed_nested, ["init", "repo", "--json"]);
    assert_eq!(malformed_output.status.code(), Some(2));
    assert_eq!(
        stdout_json(&malformed_output)["error"]["code"],
        "config.unsupported_git_remote"
    );
    assert!(!malformed.root.join(".qgh.toml").exists());

    let secret = TestFixture::new("init-bad-origin-secret");
    let secret_nested = secret.init_git_worktree_with_origin(
        "https://github.com/owner/repo.git?access_token=PRIVATE_REMOTE_MARKER",
    );
    let secret_output = secret.qgh_without_profile_in(&secret_nested, ["init", "repo", "--json"]);
    assert_eq!(secret_output.status.code(), Some(2));
    let secret_json = stdout_json(&secret_output);
    assert_eq!(
        secret_json["error"]["code"],
        "config.unsupported_git_remote"
    );
    assert_eq!(
        secret_json["error"]["details"]["remote"],
        "<redacted-remote>"
    );
    let output = format!(
        "{}{}",
        stdout_text(&secret_output),
        stderr_text(&secret_output)
    );
    assert!(!output.contains("PRIVATE_REMOTE_MARKER"));
    assert!(!secret.root.join(".qgh.toml").exists());
}

#[test]
fn init_refuses_existing_policy_unless_force_overwrites() {
    let fixture = TestFixture::new("init-force");
    fixture.write_config_with_repos("http://127.0.0.1:1", &["owner/repo", "other/repo"]);
    let nested = fixture.init_git_worktree();

    assert_success(&fixture.qgh_in(&nested, ["init", "repo", "--repo", "owner/repo", "--json"]));
    let existing = fixture.qgh_in(&nested, ["init", "repo", "--repo", "other/repo", "--json"]);
    assert_eq!(existing.status.code(), Some(2));
    assert_eq!(
        stdout_json(&existing)["error"]["code"],
        "config.repo_policy_exists"
    );
    assert!(fs::read_to_string(fixture.root.join(".qgh.toml"))
        .unwrap()
        .contains(r#"github = "owner/repo""#));

    let forced = fixture.qgh_in(
        &nested,
        ["init", "repo", "--repo", "other/repo", "--force", "--json"],
    );
    assert_success(&forced);
    assert_eq!(stdout_json(&forced)["data"]["overwritten"], true);
    assert!(fs::read_to_string(fixture.root.join(".qgh.toml"))
        .unwrap()
        .contains(r#"github = "other/repo""#));
}

#[test]
fn init_repo_force_atomically_replaces_invalid_regular_policy() {
    let fixture = TestFixture::new("init-force-invalid-policy");
    let nested = fixture.init_git_worktree();
    fs::write(fixture.root.join(".qgh.toml"), "not valid toml = [").unwrap();

    let forced = fixture.qgh_without_profile_in(
        &nested,
        ["init", "repo", "--repo", "owner/repo", "--force", "--json"],
    );

    assert_success(&forced);
    assert_eq!(stdout_json(&forced)["data"]["overwritten"], true);
    assert!(fs::read_to_string(fixture.root.join(".qgh.toml"))
        .unwrap()
        .contains(r#"github = "owner/repo""#));
}

#[test]
fn init_validates_profile_allowlist_before_writing_policy() {
    let fixture = TestFixture::new("init-profile-allowlist");
    fixture.write_config("http://127.0.0.1:1");
    let nested = fixture.init_git_worktree();

    let output = fixture.qgh_in(&nested, ["init", "repo", "--repo", "other/repo", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stdout_json(&output)["error"]["code"],
        "validation.invalid_repo"
    );
    assert!(!fixture.root.join(".qgh.toml").exists());
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
    assert_eq!(deleted_get_json["error"]["details"]["reason"], "deleted");

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
    let request_count_before_default_get = server.request_count();
    let default_get = fixture.qgh(["get", issue_source_id, "--json"]);
    assert_success(&default_get);
    let default_json = stdout_json(&default_get);
    assert_eq!(
        default_json["data"]["source"]["lifecycle_check"]["reason"],
        "not_requested"
    );
    assert_eq!(
        server.request_count(),
        request_count_before_default_get,
        "default get must not discover remote unavailability"
    );

    let get = fixture.qgh(["get", issue_source_id, "--verify-lifecycle", "--json"]);
    assert_eq!(get.status.code(), Some(4));
    let get_json = stdout_json(&get);
    assert_eq!(get_json["error"]["code"], "source.tombstoned");
    assert_eq!(get_json["error"]["details"]["source_id"], issue_source_id);
    assert_eq!(get_json["error"]["details"]["reason"], "deleted");
    assert!(get_json["error"]["details"]["observed_at"]
        .as_str()
        .is_some());

    let query = fixture.qgh(["query", "BM25 issue body tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    assert_eq!(query_json["data"]["results"].as_array().unwrap().len(), 0);
    assert_eq!(
        query_json["data"]["result_filtering"]["unresolvable_hits"], 0,
        "purge publishes a lexical successor without the removed source"
    );

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    assert_eq!(
        stdout_json(&status)["data"]["sources"]["tombstone_count"],
        2,
        "confirmed issue deletion cascades to its comments"
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
    let get = fixture.qgh(["get", issue_source_id, "--verify-lifecycle", "--json"]);
    assert_eq!(get.status.code(), Some(4));
    let get_json = stdout_json(&get);
    assert_eq!(get_json["error"]["code"], "source.tombstoned");
    assert_eq!(get_json["error"]["details"]["source_id"], issue_source_id);
    assert_eq!(get_json["error"]["details"]["reason"], "transferred");
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
fn query_and_status_warn_or_fail_from_local_snapshot_age_without_network() {
    let fixture = TestFixture::new("snapshot-age-freshness");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_freshness(&server.base_url, Some("1s"), Some("warn"), None);

    assert_success(&fixture.qgh(["sync", "--json"]));
    fixture.set_last_sync_age_seconds(3_600);
    let requests_before_local_reads = server.request_count();

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    assert_eq!(
        server.request_count(),
        requests_before_local_reads,
        "query freshness must not probe GitHub"
    );
    let query_json = stdout_json(&query);
    assert_eq!(query_json["data"]["freshness"]["decision"], "stale_warn");
    assert_eq!(query_json["data"]["freshness"]["remote_checked"], false);
    assert!(query_json["data"]["freshness"]["snapshot_age_seconds"]
        .as_i64()
        .is_some_and(|age| age >= 3_600));
    assert_eq!(query_json["data"]["freshness"]["max_age_seconds"], 1);
    assert_eq!(
        query_json["warnings"][0]["code"],
        "freshness.query_snapshot_stale"
    );
    assert_eq!(query_json["warnings"][0]["severity"], "warn");

    let human_query = fixture.qgh(["query", "BM25 tracer"]);
    assert_success(&human_query);
    assert!(stdout_text(&human_query).contains("freshness: stale_warn"));
    assert!(stderr_text(&human_query).contains("freshness.query_snapshot_stale"));

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    assert_eq!(
        server.request_count(),
        requests_before_local_reads,
        "status freshness must remain local-only"
    );
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["freshness"]["decision"], "stale_warn");
    assert_eq!(
        status_json["warnings"][0]["code"],
        "freshness.query_snapshot_stale"
    );

    let require_fresh = fixture.qgh(["query", "BM25 tracer", "--require-fresh", "--json"]);
    assert_eq!(require_fresh.status.code(), Some(2));
    let require_fresh_json = stdout_json(&require_fresh);
    assert_eq!(require_fresh_json["error"]["code"], "freshness.stale");
    assert_eq!(
        require_fresh_json["error"]["details"]["freshness"]["decision"],
        "stale_fail"
    );
    assert_eq!(
        require_fresh_json["error"]["details"]["warnings"][0]["severity"],
        "fail"
    );

    let relaxed = fixture.qgh(["query", "BM25 tracer", "--max-age", "12mo", "--json"]);
    assert_success(&relaxed);
    let relaxed_json = stdout_json(&relaxed);
    assert_eq!(relaxed_json["data"]["freshness"]["decision"], "fresh");
    assert_eq!(
        relaxed_json["data"]["freshness"]["max_age_seconds"],
        31_104_000
    );
    assert_eq!(relaxed_json["warnings"], json!([]));
}

#[test]
fn status_reports_never_synced_and_validates_duration_config() {
    let fixture = TestFixture::new("never-synced-freshness");
    fixture.write_config("http://127.0.0.1:1");

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["freshness"]["decision"], "never_synced");
    assert_eq!(status_json["data"]["freshness"]["remote_checked"], false);
    assert_eq!(
        status_json["data"]["freshness"]["snapshot_age_seconds"],
        Value::Null
    );
    assert_eq!(status_json["warnings"][0]["code"], "freshness.never_synced");

    let require_fresh = fixture.qgh(["status", "--require-fresh", "--json"]);
    assert_eq!(require_fresh.status.code(), Some(2));
    let require_fresh_json = stdout_json(&require_fresh);
    assert_eq!(require_fresh_json["error"]["code"], "freshness.stale");
    assert_eq!(
        require_fresh_json["error"]["details"]["freshness"]["decision"],
        "never_synced"
    );

    let valid_duration = TestFixture::new("valid-duration-config");
    valid_duration.write_config_with_reconcile_after_duration("http://127.0.0.1:1", "30m");
    assert_success(&valid_duration.qgh(["status", "--json"]));

    let invalid_duration = TestFixture::new("invalid-duration-config");
    invalid_duration.write_config_with_freshness(
        "http://127.0.0.1:1",
        Some("0d"),
        Some("warn"),
        None,
    );
    let invalid = invalid_duration.qgh(["status", "--json"]);
    assert_eq!(invalid.status.code(), Some(2));
    assert_eq!(stdout_json(&invalid)["error"]["code"], "config.invalid");
}

#[test]
fn status_shape_is_unchanged_without_embedding_config() {
    let fixture = TestFixture::new("status-bm25-shape");
    fixture.write_config("http://127.0.0.1:1");

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(
        json_object_keys(&status_json["data"]),
        BTreeSet::from([
            "coverage".to_string(),
            "database".to_string(),
            "freshness".to_string(),
            "github".to_string(),
            "index".to_string(),
            "paths".to_string(),
            "privacy".to_string(),
            "profile_id".to_string(),
            "purge".to_string(),
            "reconciliation".to_string(),
            "resolution".to_string(),
            "sources".to_string(),
            "sync".to_string(),
        ])
    );
    assert!(
        status_json["data"].get("embedding").is_none(),
        "BM25-only status must not expose embedding shape: {status_json}"
    );
}

#[test]
fn embedding_status_uses_only_config_and_local_store_snapshot() {
    let fixture = TestFixture::new("embedding-local-status");
    fixture.write_config_with_embedding(
        "http://127.0.0.1:1",
        r#"
provider = "local"
model_path = "/definitely/not/a/model"
file = "onnx/model.onnx"
pooling = "cls"
query_prefix = "query: "
quantization = "none"
"#,
    );

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["freshness"]["decision"], "never_synced");
    assert_eq!(status_json["data"]["embedding"]["state"], "missing");
    assert_eq!(
        status_json["data"]["embedding"]["configured_model"]["model_path"],
        "/definitely/not/a/model"
    );
    assert_eq!(
        status_json["data"]["embedding"]["coverage"]["total_chunks"],
        0
    );
    assert_eq!(
        status_json["data"]["embedding"]["coverage"]["completed_chunks"],
        0
    );
    assert_eq!(
        status_json["data"]["embedding"]["coverage"]["missing_chunks"],
        0
    );
    assert_eq!(status_json["data"]["embedding"]["fingerprint"], Value::Null);
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn status_does_not_checksum_or_initialize_explicit_model_artifacts() {
    let fixture = TestFixture::new("embedding-status-body-free");
    let server = FakeGitHub::start(issue_payload_with_pr());
    let (manifest_path, _) = fixture.write_prepared_embedding_manifest();
    fixture.write_config_with_embedding(
        &server.base_url,
        &format!(
            "provider = \"local\"\nmanifest_path = \"{}\"",
            manifest_path.display()
        ),
    );
    let requests_before = server.request_count();

    let valid_but_not_onnx = fixture.qgh(["status", "--json"]);
    assert_success(&valid_but_not_onnx);
    assert_eq!(
        stdout_json(&valid_but_not_onnx)["data"]["embedding"]["state"],
        "missing"
    );
    fs::write(
        manifest_path.parent().unwrap().join("onnx/model.onnx"),
        b"corrupt!",
    )
    .unwrap();

    let same_size_corrupt = fixture.qgh(["status", "--json"]);
    assert_success(&same_size_corrupt);
    let status_json = stdout_json(&same_size_corrupt);
    assert_eq!(status_json["data"]["embedding"]["state"], "missing");
    assert_eq!(server.request_count(), requests_before);
    assert!(!String::from_utf8_lossy(&same_size_corrupt.stdout).contains("corrupt!"));
    assert!(same_size_corrupt.stderr.is_empty());
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn status_degrades_structurally_corrupt_prepared_alias() {
    let fixture = TestFixture::new("embedding-status-corrupt-alias");
    let (manifest_path, _) = fixture.write_prepared_embedding_manifest();
    fixture.write_config_with_embedding(
        "http://127.0.0.1:1",
        &format!(
            "provider = \"local\"\nmanifest_path = \"{}\"",
            manifest_path.display()
        ),
    );
    let embed = fixture.qgh(["embed", "--force", "--json"]);
    assert!(!embed.status.success());
    fs::write(fixture.single_prepared_request_alias(), b"{").unwrap();

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["embedding"]["state"], "corrupt");
    assert!(status.stderr.is_empty());
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn status_preserves_corrupt_prepared_state_without_explicit_manifest() {
    let fixture = TestFixture::new("embedding-status-corrupt-preset-alias");
    fixture.write_default_embedding_config("http://127.0.0.1:1");
    fixture.write_corrupt_prepared_request_alias(&FastembedProviderOptions {
        manifest_path: None,
        model: Some(format!("hf:{DEFAULT_HF_MODEL_ID}")),
        model_path: None,
        file: Some("onnx/model_quantized.onnx".to_string()),
        pooling: Some(PoolingKind::Cls),
        query_prefix: Some(DEFAULT_QUERY_PREFIX.to_string()),
        quantization: None,
        token_source_env: None,
        cache_dir: None,
    });

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    assert_eq!(
        stdout_json(&status)["data"]["embedding"]["state"],
        "corrupt"
    );
}

#[cfg(all(feature = "fastembed-provider", unix))]
#[test]
fn status_rejects_symlink_explicit_manifest_contract() {
    use std::os::unix::fs::symlink;

    let fixture = TestFixture::new("embedding-status-symlink-manifest");
    let (manifest_path, _) = fixture.write_prepared_embedding_manifest();
    let linked_manifest = manifest_path.parent().unwrap().join("linked-manifest.json");
    symlink(&manifest_path, &linked_manifest).unwrap();
    fixture.write_config_with_embedding(
        "http://127.0.0.1:1",
        &format!(
            "provider = \"local\"\nmanifest_path = \"{}\"",
            linked_manifest.display()
        ),
    );

    let status = fixture.qgh(["status", "--json"]);
    assert!(!status.status.success());
    assert_eq!(
        stdout_json(&status)["error"]["code"],
        "embedding.prepared_manifest_invalid"
    );
}

#[cfg(all(feature = "vector-search", feature = "fastembed-provider"))]
#[test]
fn status_embedding_coverage_counts_completed_and_missing_chunks() {
    let fixture = TestFixture::new("embedding-coverage-counts");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    let (manifest_path, manifest_hash) = fixture.write_prepared_embedding_manifest();
    fixture.write_config_with_embedding(
        &server.base_url,
        &format!(
            "provider = \"local\"\nmanifest_path = \"{}\"",
            manifest_path.display()
        ),
    );
    assert!(!fixture.qgh(["embed", "--force", "--json"]).status.success());
    fixture.initialize_embedding_schema_for_test();
    let issue_chunk = fixture.insert_chunk_for_source(
        "qgh://github.com/issue/I_kwDOISSUE1",
        "issue embedding chunk",
    );
    let comment_chunk = fixture.insert_chunk_for_source(
        "qgh://github.com/issue-comment/IC_kwDOCOMMENT1",
        "comment embedding chunk",
    );
    let requests_before_status = server.request_count();

    let missing = fixture.qgh(["status", "--json"]);
    assert_success(&missing);
    assert_eq!(
        server.request_count(),
        requests_before_status,
        "status embedding coverage must not probe GitHub"
    );
    let missing_json = stdout_json(&missing);
    assert_eq!(missing_json["data"]["embedding"]["state"], "missing");
    assert_eq!(
        missing_json["data"]["embedding"]["coverage"]["total_chunks"],
        2
    );
    assert_eq!(
        missing_json["data"]["embedding"]["coverage"]["completed_chunks"],
        0
    );
    assert_eq!(
        missing_json["data"]["embedding"]["coverage"]["missing_chunks"],
        2
    );

    fixture
        .insert_active_embedding_fingerprint_with_revision("local:offline-fixture", &manifest_hash);
    fixture.insert_embedding_for_chunk(issue_chunk);
    let partial = fixture.qgh(["status", "--json"]);
    assert_success(&partial);
    let partial_json = stdout_json(&partial);
    assert_eq!(partial_json["data"]["embedding"]["state"], "partial");
    assert_eq!(
        partial_json["data"]["embedding"]["coverage"]["completed_chunks"],
        1
    );
    assert_eq!(
        partial_json["data"]["embedding"]["coverage"]["missing_chunks"],
        1
    );
    assert_eq!(
        partial_json["data"]["embedding"]["fingerprint"]["matches_config"],
        true
    );

    fixture.insert_embedding_for_chunk(comment_chunk);
    let complete = fixture.qgh(["status", "--json"]);
    assert_success(&complete);
    let complete_json = stdout_json(&complete);
    assert_eq!(complete_json["data"]["embedding"]["state"], "complete");
    assert_eq!(
        complete_json["data"]["embedding"]["coverage"]["completed_chunks"],
        2
    );
    assert_eq!(
        complete_json["data"]["embedding"]["coverage"]["missing_chunks"],
        0
    );
}

#[cfg(all(feature = "vector-search", feature = "fastembed-provider"))]
#[test]
fn status_embedding_coverage_reports_fingerprint_mismatch() {
    let fixture = TestFixture::new("embedding-coverage-mismatch");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    let (manifest_path, _) = fixture.write_prepared_embedding_manifest();
    fixture.write_config_with_embedding(
        &server.base_url,
        &format!(
            "provider = \"local\"\nmanifest_path = \"{}\"",
            manifest_path.display()
        ),
    );
    assert!(!fixture.qgh(["embed", "--force", "--json"]).status.success());
    fixture.initialize_embedding_schema_for_test();
    let chunk_id = fixture.insert_chunk_for_source(
        "qgh://github.com/issue/I_kwDOISSUE1",
        "stale embedding chunk",
    );
    fixture.insert_active_embedding_fingerprint_with_revision("Other/model", "fixture-sha");
    fixture.insert_embedding_for_chunk(chunk_id);

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(
        status_json["warnings"][0]["code"],
        "embedding.fingerprint_mismatch"
    );
    assert_eq!(
        status_json["data"]["embedding"]["state"],
        "fingerprint_mismatch"
    );
    assert_eq!(
        status_json["data"]["embedding"]["coverage"]["completed_chunks"],
        0
    );
    assert_eq!(
        status_json["data"]["embedding"]["coverage"]["missing_chunks"],
        1
    );
    assert_eq!(
        status_json["data"]["embedding"]["coverage"]["mismatched_chunks"],
        1
    );
    assert_eq!(
        status_json["data"]["embedding"]["fingerprint"]["matches_config"],
        false
    );
}

#[cfg(feature = "vector-search")]
#[test]
fn corrupt_embedding_fingerprint_degrades_status_and_query_without_breaking_get() {
    let fixture = TestFixture::new("embedding-corrupt-fingerprint");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let source_id = "qgh://github.com/issue/I_kwDOISSUE1";
    fixture.write_default_embedding_config(&server.base_url);
    fixture.initialize_embedding_schema_for_test();
    let chunk_id = fixture.insert_chunk_for_source(source_id, "corrupt fingerprint chunk");
    fixture.insert_matching_active_embedding_fingerprint();
    fixture.insert_embedding_metadata_for_chunk(chunk_id);
    fixture.corrupt_active_embedding_fingerprint_json();

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["embedding"]["state"], "corrupt");
    assert_eq!(
        warning_codes(&status_json),
        vec!["embedding.artifact_corrupt"]
    );
    assert_eq!(
        json_object_keys(&status_json["warnings"][0]),
        BTreeSet::from([
            "action".to_string(),
            "code".to_string(),
            "message".to_string(),
            "severity".to_string(),
        ])
    );
    assert_eq!(
        status_json["warnings"][0]["action"],
        json!({
            "reason": "embedding_rebuild_required",
            "command": "qgh embed --force --profile work",
            "json_command": "qgh embed --force --profile work --json"
        })
    );

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    assert_eq!(query_json["data"]["results"][0]["ranking"]["kind"], "bm25");
    assert_eq!(
        warning_codes(&query_json),
        vec!["embedding.artifact_corrupt"]
    );
    assert_eq!(
        json_object_keys(&query_json["warnings"][0]),
        BTreeSet::from([
            "action".to_string(),
            "code".to_string(),
            "message".to_string(),
            "severity".to_string(),
        ])
    );
    assert_eq!(
        query_json["warnings"][0]["action"],
        json!({
            "reason": "embedding_rebuild_required",
            "command": "qgh embed --force --profile work",
            "json_command": "qgh embed --force --profile work --json"
        })
    );

    let get = fixture.qgh(["get", source_id, "--json"]);
    assert_success(&get);
    assert_eq!(stdout_json(&get)["data"]["source"]["source_id"], source_id);
}

#[cfg(feature = "vector-search")]
#[test]
fn unreadable_vector_table_degrades_status_and_query_without_breaking_get() {
    for (scenario, create_mismatched_table) in [("missing", false), ("mismatched", true)] {
        let fixture = TestFixture::new(&format!("embedding-vector-table-{scenario}"));
        let server = FakeGitHub::start(issue_payload_with_pr());
        fixture.write_config(&server.base_url);
        assert_success(&fixture.qgh(["sync", "--json"]));

        let source_id = "qgh://github.com/issue/I_kwDOISSUE1";
        fixture.write_default_embedding_config(&server.base_url);
        fixture.initialize_embedding_schema_for_test();
        let chunk_id = fixture.insert_chunk_for_source(source_id, "vector readiness chunk");
        fixture.insert_matching_active_embedding_fingerprint();
        fixture.insert_embedding_metadata_for_chunk(chunk_id);
        if create_mismatched_table {
            fixture.create_mismatched_vector_table();
        }

        let status = fixture.qgh(["status", "--json"]);
        assert_success(&status);
        let status_json = stdout_json(&status);
        assert_eq!(
            status_json["data"]["embedding"]["state"], "corrupt",
            "scenario={scenario}: {status_json}"
        );
        assert_eq!(
            warning_codes(&status_json),
            vec!["embedding.artifact_corrupt"],
            "scenario={scenario}: {status_json}"
        );
        assert_eq!(
            json_object_keys(&status_json["warnings"][0]),
            BTreeSet::from([
                "action".to_string(),
                "code".to_string(),
                "message".to_string(),
                "severity".to_string(),
            ])
        );
        assert_eq!(
            status_json["warnings"][0]["action"],
            json!({
                "reason": "embedding_rebuild_required",
                "command": "qgh embed --force --profile work",
                "json_command": "qgh embed --force --profile work --json"
            }),
            "scenario={scenario}: {status_json}"
        );

        let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
        assert_success(&query);
        let query_json = stdout_json(&query);
        assert_eq!(
            query_json["data"]["results"][0]["ranking"]["kind"], "bm25",
            "scenario={scenario}: {query_json}"
        );
        assert_eq!(
            warning_codes(&query_json),
            vec!["embedding.artifact_corrupt"],
            "scenario={scenario}: {query_json}"
        );
        assert_eq!(
            query_json["warnings"][0]["action"],
            json!({
                "reason": "embedding_rebuild_required",
                "command": "qgh embed --force --profile work",
                "json_command": "qgh embed --force --profile work --json"
            }),
            "scenario={scenario}: {query_json}"
        );

        let get = fixture.qgh(["get", source_id, "--json"]);
        assert_success(&get);
        assert_eq!(stdout_json(&get)["data"]["source"]["source_id"], source_id);
    }
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn embedding_enabled_sync_persists_tokenizer_backed_chunks() {
    let fixture = TestFixture::new("embedding-sync-chunks");
    let server = FakeGitHub::start(issue_payload_with_pr());
    let model_dir = fixture.write_local_embedding_tokenizer_model();
    fixture.write_config_with_embedding(
        &server.base_url,
        &format!(
            r#"
provider = "local"
model_path = "{}"
file = "onnx/model.onnx"
pooling = "cls"
query_prefix = "query: "
quantization = "none"
"#,
            model_dir.display()
        ),
    );

    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);

    let chunk_count = fixture.sqlite_chunk_count();
    assert!(
        chunk_count > 0,
        "embedding-enabled sync must persist tokenizer-backed chunks"
    );
}

#[cfg(all(feature = "vector-search", feature = "fastembed-provider"))]
#[test]
#[ignore = "requires explicitly installed pinned Qwen embedding snapshot"]
fn installed_qwen_normal_sync_publishes_hybrid_generation() {
    let fixture = TestFixture::new("qwen-normal-sync-generation");
    let server = FakeGitHub::start(issue_payload_with_pr());
    let prepared_models = PathBuf::from(
        std::env::var("QGH_QWEN_PREPARED_MODELS")
            .expect("QGH_QWEN_PREPARED_MODELS must point to the prepared store"),
    );
    let cache_home = prepared_models
        .parent()
        .and_then(Path::parent)
        .expect("prepared store must be <cache>/qgh/prepared-qwen-models");
    fixture.write_config_with_embedding(
        &server.base_url,
        r#"
provider = "local"
model = "qwen3-embedding-0.6b"
device = "auto"
"#,
    );

    let mut sync_command = fixture.base_command();
    let sync = sync_command
        .env("XDG_CACHE_HOME", cache_home)
        .args(["--profile", "work", "sync", "--json"])
        .output()
        .unwrap();
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    assert!(
        warning_codes(&sync_json)
            .iter()
            .all(|code| !code.starts_with("embedding.sync_")),
        "normal Qwen sync must not fall back after tokenizer/runtime preparation: {sync_json}"
    );

    let mut status_command = fixture.base_command();
    let status = status_command
        .env("XDG_CACHE_HOME", cache_home)
        .args(["--profile", "work", "status", "--json"])
        .output()
        .unwrap();
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["embedding"]["state"], "complete");
    assert_eq!(
        status_json["data"]["embedding"]["coverage"]["missing_chunks"],
        0
    );
    assert_eq!(
        status_json["data"]["embedding"]["configured_model"]["model_id"],
        "Qwen/Qwen3-Embedding-0.6B"
    );

    let mut query_command = fixture.base_command();
    let query = query_command
        .env("XDG_CACHE_HOME", cache_home)
        .args(["--profile", "work", "query", "BM25 tracer", "--json"])
        .output()
        .unwrap();
    assert_success(&query);
    let query_json = stdout_json(&query);
    assert!(warning_codes(&query_json).is_empty());
    assert_eq!(
        query_json["data"]["results"][0]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );
    assert_eq!(
        query_json["data"]["results"][0]["ranking"]["kind"],
        "hybrid"
    );
    let source_id = query_json["data"]["results"][0]["get_args"]["source_id"]
        .as_str()
        .unwrap();
    let mut get_command = fixture.base_command();
    let get = get_command
        .env("XDG_CACHE_HOME", cache_home)
        .args(["--profile", "work", "get", source_id, "--json"])
        .output()
        .unwrap();
    assert_success(&get);
    assert_eq!(stdout_json(&get)["data"]["source"]["source_id"], source_id);

    let mut second_sync_command = fixture.base_command();
    let second_sync = second_sync_command
        .env("XDG_CACHE_HOME", cache_home)
        .args(["--profile", "work", "sync"])
        .output()
        .unwrap();
    assert_success(&second_sync);
    let second_stderr = stderr_text(&second_sync);
    assert!(second_stderr.contains("reused="));
    assert!(second_stderr.contains("missing=0"));
    assert!(second_stderr.contains("embedded=0"));

    let mut second_query_command = fixture.base_command();
    let second_query = second_query_command
        .env("XDG_CACHE_HOME", cache_home)
        .args(["--profile", "work", "query", "BM25 tracer", "--json"])
        .output()
        .unwrap();
    assert_success(&second_query);
    let second_query_json = stdout_json(&second_query);
    assert!(warning_codes(&second_query_json).is_empty());
    assert_eq!(
        second_query_json["data"]["results"][0]["ranking"]["kind"],
        "hybrid"
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn embedding_if_stale_fresh_skip_does_not_backfill_local_artifacts() {
    let fixture = TestFixture::new("embedding-sync-local-backfill");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    fixture.assert_sqlite_chunks_empty();
    let request_count_after_seed = server.request_count();

    let model_dir = fixture.write_local_embedding_tokenizer_model();
    fixture.write_config_with_embedding(
        &server.base_url,
        &format!(
            r#"
provider = "local"
model_path = "{}"
file = "onnx/model.onnx"
pooling = "cls"
query_prefix = "query: "
quantization = "none"
"#,
            model_dir.display()
        ),
    );

    let local_backfill = fixture.qgh(["sync", "--if-stale", "--max-age", "30m", "--json"]);
    assert_success(&local_backfill);
    let local_backfill_json = stdout_json(&local_backfill);
    assert_eq!(local_backfill_json["data"]["sync_state"], "skipped_fresh");
    assert!(warning_codes(&local_backfill_json).is_empty());
    assert_eq!(
        server.request_count(),
        request_count_after_seed,
        "fresh skip must not contact GitHub"
    );
    fixture.assert_sqlite_chunks_empty();
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn embedding_sync_preserves_refresh_error_code_without_content() {
    let fixture = TestFixture::new("embedding-sync-refresh-error-code");
    let server = FakeGitHub::start(issue_payload_with_pr());
    let model_dir = fixture.write_local_embedding_tokenizer_model();
    fixture.write_config_with_embedding(
        &server.base_url,
        &format!(
            r#"
provider = "local"
model_path = "{}"
file = "onnx/model.onnx"
pooling = "cls"
query_prefix = "query: "
quantization = "none"
"#,
            model_dir.display()
        ),
    );
    let private_marker = "PRIVATE_TEST_DOCUMENT_90";
    let output = fixture.qgh_with_document_vectors(
        ["sync", "--json"],
        &json!({ private_marker: [0.1, 0.2, 0.3] }),
    );

    assert_success(&output);
    let output_json = stdout_json(&output);
    let warning = embedding_sync_warning(&output_json);
    assert_eq!(warning["code"], "embedding.generation_invalid_spec");
    assert_eq!(
        json_object_keys(warning),
        BTreeSet::from([
            "code".to_string(),
            "message".to_string(),
            "severity".to_string(),
        ])
    );
    assert!(!stdout_text(&output).contains(private_marker));
    assert!(!stderr_text(&output).contains(private_marker));

    let human =
        fixture.qgh_with_document_vectors(["sync"], &json!({ private_marker: [0.1, 0.2, 0.3] }));
    assert_success(&human);
    let human_stdout = stdout_text(&human);
    assert!(human_stdout.contains("qgh sync complete — search ready with limitations"));
    assert!(human_stdout.contains("search: BM25 ready; semantic unavailable"));
    assert!(human_stdout.contains("next: qgh sync --backfill --all --profile work"));
    assert!(!human_stdout.contains("repair: qgh embed --force --profile work"));
    let human_stderr = stderr_text(&human);
    assert!(human_stderr.contains("warning [embedding.generation_invalid_spec]"));
    assert!(human_stderr.contains("BM25 index refresh remains available"));
    assert!(!human_stdout.contains(private_marker));
    assert!(!human_stderr.contains(private_marker));

    let mut quiet_warning_command = fixture.base_command();
    let quiet_warning = quiet_warning_command
        .env(
            "QGH_TEST_EMBEDDING_DOCUMENT_VECTORS",
            json!({ private_marker: [0.1, 0.2, 0.3] }).to_string(),
        )
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .args(["--profile", "work", "sync", "--quiet"])
        .output()
        .unwrap();
    assert_success(&quiet_warning);
    let quiet_warning_stderr = stderr_text(&quiet_warning);
    assert!(quiet_warning_stderr.contains("warning [embedding.generation_invalid_spec]"));
    assert!(!quiet_warning_stderr.contains('\u{1b}'));
    assert!(!stdout_text(&quiet_warning).contains('\u{1b}'));

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    let results = query_json["data"]["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["ranking"]["kind"], "bm25");
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn embedding_sync_skips_when_body_hash_matches_despite_new_github_timestamp() {
    let fixture = TestFixture::new("embedding-sync-body-hash-skip");
    let server = EditingFakeGitHub::start();
    let model_dir = fixture.write_local_embedding_tokenizer_model();
    fixture.write_config_with_embedding(
        &server.base_url,
        &format!(
            r#"
provider = "local"
model_path = "{}"
file = "onnx/model.onnx"
pooling = "cls"
query_prefix = "query: "
quantization = "none"
"#,
            model_dir.display()
        ),
    );

    let first_sync = fixture.qgh(["sync", "--json"]);
    assert_success(&first_sync);
    assert_embedding_sync_warning(&stdout_json(&first_sync));
    let issue_source_id = "qgh://github.com/issue/I_kwDOISSUE1";
    let first_issue = stdout_json(&fixture.qgh(["get", issue_source_id, "--json"]));
    let first_source_version = first_issue["data"]["source"]["source_version"].clone();
    let first_chunk_ids = fixture.sqlite_chunk_ids_for_source(issue_source_id);
    assert!(
        !first_chunk_ids.is_empty(),
        "embedding-enabled seed sync must chunk the issue source"
    );
    fixture.assert_source_version_count(issue_source_id, 1);

    server.set_mode(EDITING_SAME_BODY_NEW_TIMESTAMP);
    let second_sync = fixture.qgh(["sync", "--json"]);
    assert_success(&second_sync);
    assert_embedding_sync_warning(&stdout_json(&second_sync));

    let second_issue = stdout_json(&fixture.qgh(["get", issue_source_id, "--json"]));
    let second_source_version = &second_issue["data"]["source"]["source_version"];
    assert_eq!(
        second_source_version["body_hash"], first_source_version["body_hash"],
        "timestamp-only GitHub edits must keep the same content identity"
    );
    assert_eq!(
        second_source_version["github_updated_at"],
        "2026-01-05T00:00:00Z"
    );
    assert_ne!(
        second_source_version["github_updated_at"],
        first_source_version["github_updated_at"]
    );
    assert_eq!(
        fixture.sqlite_chunk_ids_for_source(issue_source_id),
        first_chunk_ids,
        "body_hash-unchanged sync must reuse existing chunks instead of re-chunking"
    );
    fixture.assert_source_version_count(issue_source_id, 1);
}

#[cfg(feature = "vector-search")]
#[test]
fn embedding_config_without_vector_artifacts_keeps_bm25_query_available() {
    let fixture = TestFixture::new("embedding-no-vector-artifacts-query");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    let request_count_after_sync = server.request_count();

    fixture.write_default_embedding_config(&server.base_url);
    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);

    assert_eq!(
        query_json["data"]["results"][0]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );
    assert_eq!(query_json["data"]["results"][0]["ranking"]["kind"], "bm25");
    assert_eq!(
        warning_codes(&query_json),
        vec!["embedding.coverage_missing"]
    );
    assert_eq!(
        server.request_count(),
        request_count_after_sync,
        "local query must not make GitHub or model acquisition requests"
    );
}

#[cfg(all(feature = "vector-search", feature = "fastembed-provider"))]
#[test]
fn missing_qwen_embedding_model_guides_install_across_sync_and_status() {
    let fixture = TestFixture::new("qwen-model-install-action");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_embedding(
        &server.base_url,
        r#"
provider = "local"
model = "qwen3-embedding-0.6b"
device = "auto"
"#,
    );

    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    let warning = sync_json["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|warning| warning["code"] == "embedding.model_not_installed")
        .unwrap_or_else(|| panic!("missing Qwen model warning: {sync_json}"));
    assert_eq!(
        warning["action"],
        json!({
            "reason": "embedding_model_not_installed",
            "command": "qgh model install qwen3-embedding-0.6b",
            "json_command": "qgh model install qwen3-embedding-0.6b --json"
        })
    );

    let human_sync = fixture.qgh(["sync"]);
    assert_success(&human_sync);
    assert!(stderr_text(&human_sync).contains("qgh model install qwen3-embedding-0.6b"));
    assert!(!stdout_text(&human_sync).contains("repair: qgh embed --force"));

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(
        status_json["data"]["embedding"]["repair_action"],
        warning["action"]
    );
    assert_eq!(
        status_json["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|warning| warning["code"] == "embedding.coverage_missing")
            .unwrap()["action"],
        warning["action"]
    );

    let human_status = fixture.qgh(["status"]);
    assert_success(&human_status);
    assert!(stderr_text(&human_status).contains("qgh model install qwen3-embedding-0.6b"));
}

#[cfg(all(feature = "vector-search", feature = "fastembed-provider"))]
#[test]
fn corrupt_qwen_embedding_model_guides_reinstall_across_sync_and_status() {
    let fixture = TestFixture::new("qwen-model-reinstall-action");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_embedding(
        &server.base_url,
        r#"
provider = "local"
model = "qwen3-embedding-0.6b"
device = "auto"
"#,
    );
    let private_marker = "PRIVATE_CORRUPT_QWEN_SNAPSHOT_91";
    let snapshot = fixture
        .cache_home
        .join("qgh/prepared-qwen-models/qwen3-embedding-0.6b");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("manifest.json"), private_marker).unwrap();

    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    let warning = sync_json["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|warning| warning["code"] == "embedding.qwen_snapshot_invalid")
        .unwrap_or_else(|| panic!("missing corrupt Qwen warning: {sync_json}"));
    let expected_action = json!({
        "reason": "embedding_model_invalid",
        "command": "qgh model install qwen3-embedding-0.6b",
        "json_command": "qgh model install qwen3-embedding-0.6b --json"
    });
    assert_eq!(warning["action"], expected_action);

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["embedding"]["state"], "corrupt");
    assert_eq!(
        status_json["data"]["embedding"]["repair_action"],
        expected_action
    );
    assert_eq!(status_json["warnings"][0]["action"], expected_action);
    assert!(!status_json.to_string().contains(private_marker));

    let embed = fixture.qgh(["embed", "--force", "--json"]);
    assert_eq!(embed.status.code(), Some(2));
    let embed_json = stdout_json(&embed);
    assert_eq!(
        embed_json["error"]["code"],
        "embedding.qwen_snapshot_invalid"
    );
    assert_eq!(
        embed_json["error"]["details"]["repair_action"],
        expected_action
    );
    assert!(embed_json["error"]["hint"]
        .as_str()
        .unwrap()
        .contains("qgh model install qwen3-embedding-0.6b"));
    assert!(!embed_json.to_string().contains(private_marker));
}

#[test]
fn requested_rerank_without_configuration_preserves_bm25_results() {
    let fixture = TestFixture::new("reranker-not-configured-fallback");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let baseline = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&baseline);
    let baseline_results = stdout_json(&baseline)["data"]["results"].clone();

    let reranked = fixture.qgh(["query", "BM25 tracer", "--rerank", "--json"]);
    assert_success(&reranked);
    let reranked_json = stdout_json(&reranked);

    assert_eq!(reranked_json["data"]["results"], baseline_results);
    assert_eq!(
        reranked_json["data"]["rerank"],
        json!({
            "requested": true,
            "applied": false,
            "reason": "not_configured"
        })
    );
    assert_eq!(
        warning_codes(&reranked_json),
        vec!["reranker.not_configured"]
    );
    assert_eq!(
        json_object_keys(&reranked_json["warnings"][0]),
        BTreeSet::from([
            "code".to_string(),
            "message".to_string(),
            "severity".to_string(),
        ])
    );
}

#[test]
fn requested_rerank_bypasses_exact_locator_without_warning() {
    let fixture = TestFixture::new("reranker-exact-locator-bypass");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let exact = fixture.qgh([
        "query",
        "https://github.com/owner/repo/issues/42",
        "--rerank",
        "--json",
    ]);
    assert_success(&exact);
    let exact_json = stdout_json(&exact);

    assert_eq!(
        exact_json["data"]["rerank"],
        json!({
            "requested": true,
            "applied": false,
            "reason": "exact_bypass"
        })
    );
    assert!(warning_codes(&exact_json).is_empty());
    assert_eq!(exact_json["data"]["results"][0]["ranking"]["kind"], "exact");

    for output in [
        fixture.qgh(["query", "#42", "--repo", "owner/repo", "--rerank", "--json"]),
        fixture.qgh([
            "query",
            "https://github.com/owner/repo/issues/42#issuecomment-5001",
            "--rerank",
            "--json",
        ]),
    ] {
        assert_success(&output);
        let output = stdout_json(&output);
        assert_eq!(output["data"]["rerank"]["reason"], "exact_bypass");
        assert_eq!(output["data"]["results"][0]["ranking"]["kind"], "exact");
        assert!(output["data"]["results"][0]["ranking"]
            .get("rerank_score")
            .is_none());
    }
}

#[test]
fn configured_but_uninstalled_reranker_preserves_retrieval_order() {
    let fixture = TestFixture::new("reranker-model-not-installed-fallback");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_reranker(
        &server.base_url,
        r#"
provider = "local"
model = "qwen3-reranker-0.6b"
device = "auto"
"#,
    );
    assert_success(&fixture.qgh(["sync", "--json"]));
    let request_count_after_sync = server.request_count();

    let baseline = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&baseline);
    let baseline_results = stdout_json(&baseline)["data"]["results"].clone();

    let reranked = fixture.qgh(["query", "BM25 tracer", "--rerank", "--json"]);
    assert_success(&reranked);
    let reranked_json = stdout_json(&reranked);

    assert_eq!(reranked_json["data"]["results"], baseline_results);
    assert_eq!(reranked_json["data"]["rerank"]["requested"], true);
    assert_eq!(reranked_json["data"]["rerank"]["applied"], false);
    assert_eq!(
        reranked_json["data"]["rerank"]["reason"],
        "model_not_installed"
    );
    assert_eq!(
        warning_codes(&reranked_json),
        vec!["reranker.model_not_installed"]
    );
    let expected_action = json!({
        "reason": "reranker_model_not_installed",
        "command": "qgh model install qwen3-reranker-0.6b",
        "json_command": "qgh model install qwen3-reranker-0.6b --json"
    });
    assert_eq!(
        reranked_json["data"]["rerank"]["repair_action"],
        expected_action
    );
    assert_eq!(reranked_json["warnings"][0]["action"], expected_action);
    let human = fixture.qgh(["query", "BM25 tracer", "--rerank"]);
    assert_success(&human);
    assert!(stderr_text(&human).contains("qgh model install qwen3-reranker-0.6b"));
    assert_eq!(
        server.request_count(),
        request_count_after_sync,
        "local query must not download a model"
    );
}

#[test]
fn corrupt_reranker_snapshot_preserves_retrieval_order_without_content_leakage() {
    let fixture = TestFixture::new("reranker-corrupt-snapshot-fallback");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_reranker(
        &server.base_url,
        r#"
provider = "local"
model = "qwen3-reranker-0.6b"
device = "auto"
"#,
    );
    assert_success(&fixture.qgh(["sync", "--json"]));
    let snapshot = fixture
        .cache_home
        .join("qgh/prepared-qwen-models/qwen3-reranker-0.6b");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("manifest.json"), b"private malformed payload").unwrap();
    let baseline =
        stdout_json(&fixture.qgh(["query", "BM25 tracer", "--json"]))["data"]["results"].clone();

    let output = fixture.qgh(["query", "BM25 tracer", "--rerank", "--json"]);

    assert_success(&output);
    let stderr = stderr_text(&output);
    let output = stdout_json(&output);
    assert_eq!(output["data"]["results"], baseline);
    assert_eq!(output["data"]["rerank"]["reason"], "model_corrupt");
    assert_eq!(warning_codes(&output), vec!["reranker.model_corrupt"]);
    let expected_action = json!({
        "reason": "reranker_model_invalid",
        "command": "qgh model install qwen3-reranker-0.6b",
        "json_command": "qgh model install qwen3-reranker-0.6b --json"
    });
    assert_eq!(output["data"]["rerank"]["repair_action"], expected_action);
    assert_eq!(output["warnings"][0]["action"], expected_action);
    assert!(!serde_json::to_string(&output)
        .unwrap()
        .contains("private malformed payload"));
    assert!(!stderr.contains("private malformed payload"));
}

#[test]
fn configured_reranker_reorders_only_resolved_candidates_and_exposes_scores() {
    let fixture = TestFixture::new("reranker-applied-contract");
    let server = FakeGitHub::start(limit_policy_issue_payload());
    fixture.write_config_with_reranker(
        &server.base_url,
        r#"
provider = "local"
model = "qwen3-reranker-0.6b"
device = "auto"
"#,
    );
    assert_success(&fixture.qgh(["sync", "--json"]));
    let baseline = fixture.qgh([
        "query",
        "repo policy limit tracer",
        "--limit",
        "5",
        "--json",
    ]);
    assert_success(&baseline);
    let baseline_json = stdout_json(&baseline);
    let baseline_ids = result_source_ids(&baseline_json);
    let scores = baseline_ids
        .iter()
        .enumerate()
        .map(|(index, source_id)| (source_id.clone(), json!(index as f32)))
        .collect::<serde_json::Map<_, _>>();

    let reranked = fixture.qgh_with_rerank_scores(
        [
            "query",
            "repo policy limit tracer",
            "--limit",
            "5",
            "--rerank",
            "--json",
        ],
        &Value::Object(scores),
    );

    assert_success(&reranked);
    let reranked_json = stdout_json(&reranked);
    let mut expected = baseline_ids.clone();
    expected.reverse();
    assert_eq!(result_source_ids(&reranked_json), expected);
    assert_eq!(
        reranked_json["data"]["rerank"],
        json!({
            "requested": true,
            "applied": true,
            "model": "qwen3-reranker-0.6b",
            "runtime_profile": "cpu_f32",
            "candidate_count": 5,
            "max_candidates": 10,
            "max_tokens": 384
        })
    );
    assert!(warning_codes(&reranked_json).is_empty());
    for result in reranked_json["data"]["results"].as_array().unwrap() {
        assert!(result["ranking"]["rerank_score"].is_number());
        assert!(result["ranking"]["pre_rerank_rank"].is_number());
        let source_id = result["get_args"]["source_id"].as_str().unwrap();
        let get = fixture.qgh(["get", source_id, "--json"]);
        assert_success(&get);
        assert_eq!(stdout_json(&get)["data"]["source"]["source_id"], source_id);
    }
}

#[test]
fn reranker_never_touches_candidates_beyond_the_fixed_top_ten() {
    let fixture = TestFixture::new("reranker-fixed-depth");
    let server = FakeGitHub::start(rerank_depth_issue_payload());
    fixture.write_config_with_reranker(
        &server.base_url,
        r#"
provider = "local"
model = "qwen3-reranker-0.6b"
device = "auto"
"#,
    );
    assert_success(&fixture.qgh(["sync", "--json"]));
    let baseline = stdout_json(&fixture.qgh([
        "query",
        "fixed rerank depth tracer",
        "--limit",
        "12",
        "--json",
    ]));
    let baseline_ids = result_source_ids(&baseline);
    assert_eq!(baseline_ids.len(), 12);
    let scores = baseline_ids
        .iter()
        .take(10)
        .enumerate()
        .map(|(index, source_id)| (source_id.clone(), json!(index as f32)))
        .collect::<serde_json::Map<_, _>>();

    let output = fixture.qgh_with_rerank_scores(
        [
            "query",
            "fixed rerank depth tracer",
            "--limit",
            "12",
            "--rerank",
            "--json",
        ],
        &Value::Object(scores),
    );

    assert_success(&output);
    let output = stdout_json(&output);
    let mut expected_head = baseline_ids[..10].to_vec();
    expected_head.reverse();
    expected_head.extend_from_slice(&baseline_ids[10..]);
    assert_eq!(result_source_ids(&output), expected_head);
    assert_eq!(output["data"]["rerank"]["candidate_count"], 10);
    let results = output["data"]["results"].as_array().unwrap();
    assert!(results[..10]
        .iter()
        .all(|result| result["ranking"].get("rerank_score").is_some()));
    assert!(results[10..]
        .iter()
        .all(|result| result["ranking"].get("rerank_score").is_none()));
}

#[test]
fn human_query_reports_rerank_application_and_fallback_on_stdout() {
    let fixture = TestFixture::new("reranker-human-output");
    let server = FakeGitHub::start(limit_policy_issue_payload());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let fallback = fixture.qgh(["query", "repo policy limit tracer", "--rerank"]);
    assert_success(&fallback);
    assert!(stdout_text(&fallback).contains("rerank: not applied (not_configured)"));
    assert!(stderr_text(&fallback).contains("reranker.not_configured"));

    fixture.write_config_with_reranker(
        &server.base_url,
        r#"
provider = "local"
model = "qwen3-reranker-0.6b"
device = "auto"
"#,
    );
    let baseline = stdout_json(&fixture.qgh([
        "query",
        "repo policy limit tracer",
        "--limit",
        "5",
        "--json",
    ]));
    let scores = result_source_ids(&baseline)
        .into_iter()
        .enumerate()
        .map(|(index, source_id)| (source_id, json!(index as f32)))
        .collect::<serde_json::Map<_, _>>();
    let applied = fixture.qgh_with_rerank_scores(
        [
            "query",
            "repo policy limit tracer",
            "--limit",
            "5",
            "--rerank",
        ],
        &Value::Object(scores),
    );
    assert_success(&applied);
    assert!(stdout_text(&applied)
        .contains("rerank: applied qwen3-reranker-0.6b to 5 candidates (cpu_f32)"));
    assert!(stderr_text(&applied).is_empty());
}

#[test]
fn mcp_query_accepts_boolean_rerank_without_adding_a_write_tool() {
    let fixture = TestFixture::new("reranker-mcp-query");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let output = fixture.mcp([
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
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {"query": "BM25 tracer", "rerank": true}
            }
        }),
    ]);

    assert_success(&output);
    assert!(stderr_text(&output).is_empty());
    let messages = stdout_json_lines(&output);
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[1]["result"]["structuredContent"]["data"]["rerank"],
        json!({"requested": true, "applied": false, "reason": "not_configured"})
    );
    assert_eq!(
        messages[1]["result"]["structuredContent"]["warnings"][0]["code"],
        "reranker.not_configured"
    );
}

#[test]
fn partial_reranker_failure_discards_all_scores_and_preserves_original_order() {
    let fixture = TestFixture::new("reranker-partial-failure");
    let server = FakeGitHub::start(limit_policy_issue_payload());
    fixture.write_config_with_reranker(
        &server.base_url,
        r#"
provider = "local"
model = "qwen3-reranker-0.6b"
device = "auto"
"#,
    );
    assert_success(&fixture.qgh(["sync", "--json"]));
    let baseline = fixture.qgh([
        "query",
        "repo policy limit tracer",
        "--limit",
        "5",
        "--json",
    ]);
    assert_success(&baseline);
    let baseline_json = stdout_json(&baseline);
    let baseline_results = baseline_json["data"]["results"].clone();
    let first_id = baseline_results[0]["source_id"].as_str().unwrap();

    let reranked = fixture.qgh_with_rerank_scores(
        [
            "query",
            "repo policy limit tracer",
            "--limit",
            "5",
            "--rerank",
            "--json",
        ],
        &json!({first_id: 1.0}),
    );

    assert_success(&reranked);
    let reranked_json = stdout_json(&reranked);
    assert_eq!(reranked_json["data"]["results"], baseline_results);
    assert_eq!(
        reranked_json["data"]["rerank"],
        json!({
            "requested": true,
            "applied": false,
            "reason": "inference_failed"
        })
    );
    assert_eq!(
        warning_codes(&reranked_json),
        vec!["reranker.inference_failed"]
    );
    assert!(reranked_json["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|result| result["ranking"].get("rerank_score").is_none()));
}

#[test]
fn reranker_config_rejects_unknown_keys_remote_providers_models_and_devices() {
    for (fixture_name, reranker, expected_message_fragment) in [
        (
            "reranker-unknown-key",
            r#"
provider = "local"
model = "qwen3-reranker-0.6b"
depth = 20
"#,
            "unknown field",
        ),
        (
            "reranker-remote-provider",
            r#"
provider = "remote"
model = "qwen3-reranker-0.6b"
"#,
            "unknown variant",
        ),
        (
            "reranker-unsupported-model",
            r#"
provider = "local"
model = "unapproved-reranker"
"#,
            "qwen3-reranker-0.6b",
        ),
        (
            "reranker-invalid-device",
            r#"
provider = "local"
model = "qwen3-reranker-0.6b"
device = "cuda"
"#,
            "unknown variant",
        ),
    ] {
        let fixture = TestFixture::new(fixture_name);
        fixture.write_config_with_reranker("http://127.0.0.1:1", reranker);

        let status = fixture.qgh(["status", "--json"]);
        assert_eq!(status.status.code(), Some(2));
        let status_json = stdout_json(&status);
        assert_eq!(status_json["error"]["code"], "config.invalid");
        assert!(
            status_json["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected_message_fragment),
            "unexpected reranker config error: {status_json}"
        );
    }
}

#[cfg(not(feature = "vector-search"))]
#[test]
fn bm25_build_with_embedding_config_warns_and_keeps_query_available() {
    let fixture = TestFixture::new("bm25-build-embedding-config-query");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    let request_count_after_sync = server.request_count();
    fixture.write_default_embedding_config(&server.base_url);

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    assert_eq!(query_json["data"]["results"][0]["ranking"]["kind"], "bm25");
    assert_eq!(
        warning_codes(&query_json),
        vec!["embedding.vector_init_failed"]
    );
    assert_eq!(
        json_object_keys(&query_json["warnings"][0]),
        BTreeSet::from([
            "code".to_string(),
            "message".to_string(),
            "severity".to_string(),
        ])
    );
    assert_eq!(server.request_count(), request_count_after_sync);
}

#[cfg(feature = "vector-search")]
#[test]
fn vector_init_failure_returns_closed_warning_and_bm25_results() {
    let fixture = TestFixture::new("embedding-vector-init-failure");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    fixture.write_default_embedding_config(&server.base_url);

    let mut command = fixture.base_command();
    let query = command
        .env("QGH_TEST_VECTOR_INIT_FAILURE", "1")
        .args(["--profile", "work", "query", "BM25 tracer", "--json"])
        .output()
        .unwrap();
    assert_success(&query);
    let query_json = stdout_json(&query);

    assert_eq!(query_json["data"]["results"][0]["ranking"]["kind"], "bm25");
    let warning = &query_json["warnings"][0];
    assert_eq!(warning["code"], "embedding.vector_init_failed");
    assert_eq!(warning["severity"], "warn");
    assert_eq!(
        json_object_keys(warning),
        BTreeSet::from([
            "code".to_string(),
            "message".to_string(),
            "severity".to_string(),
        ])
    );
}

#[cfg(feature = "vector-search")]
#[test]
fn embedding_fingerprint_mismatch_warns_and_keeps_bm25_results() {
    let fixture = TestFixture::new("embedding-fingerprint-mismatch");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    fixture.write_config_with_embedding(
        &server.base_url,
        r#"
provider = "local"
model = "hf:Snowflake/snowflake-arctic-embed-l-v2.0"
file = "onnx/model_quantized.onnx"
pooling = "cls"
query_prefix = "query: "
"#,
    );
    assert_success(&fixture.qgh(["status", "--json"]));
    fixture.initialize_embedding_schema_for_test();
    fixture.insert_active_embedding_fingerprint("Example/old-model");

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    assert_eq!(
        query_json["data"]["results"][0]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );
    assert_eq!(query_json["data"]["results"][0]["ranking"]["kind"], "bm25");
    let warning = &query_json["warnings"][0];
    assert_eq!(warning["code"], "embedding.fingerprint_mismatch");
    assert_eq!(
        json_object_keys(warning),
        BTreeSet::from([
            "code".to_string(),
            "message".to_string(),
            "severity".to_string(),
        ])
    );
    let warning_text = warning.to_string();
    assert!(!warning_text.contains("Example/old-model"));
    assert!(!warning_text.contains("BM25 issue body tracer"));
}

#[cfg(feature = "vector-search")]
#[test]
fn embedding_fingerprint_revision_mismatch_warns_and_keeps_bm25_results() {
    let fixture = TestFixture::new("embedding-fingerprint-revision-mismatch");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    fixture.write_config_with_embedding(
        &server.base_url,
        r#"
provider = "local"
model = "hf:Snowflake/snowflake-arctic-embed-l-v2.0"
file = "onnx/model_quantized.onnx"
pooling = "cls"
query_prefix = "query: "
"#,
    );
    fixture.initialize_embedding_schema_for_test();
    fixture.insert_active_embedding_fingerprint_with_revision(DEFAULT_HF_MODEL_ID, "old-main-sha");

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    assert_eq!(
        query_json["data"]["results"][0]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );
    assert_eq!(query_json["data"]["results"][0]["ranking"]["kind"], "bm25");
    assert_eq!(
        query_json["warnings"][0]["code"],
        "embedding.fingerprint_mismatch"
    );
}

#[cfg(feature = "vector-search")]
#[test]
fn partial_embedding_coverage_warns_and_falls_back_to_bm25_results() {
    let fixture = TestFixture::new("embedding-coverage-partial-query");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let bm25_query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&bm25_query);
    let bm25_json = stdout_json(&bm25_query);
    let bm25_results = bm25_json["data"]["results"].clone();
    assert_eq!(bm25_json["warnings"], json!([]));

    fixture.write_default_embedding_config(&server.base_url);
    fixture.initialize_embedding_schema_for_test();
    let issue_chunk = fixture.insert_chunk_for_source(
        "qgh://github.com/issue/I_kwDOISSUE1",
        "issue embedding chunk",
    );
    fixture.insert_chunk_for_source(
        "qgh://github.com/issue-comment/IC_kwDOCOMMENT1",
        "comment embedding chunk",
    );
    fixture.insert_matching_active_embedding_fingerprint();
    fixture.insert_embedding_for_chunk(issue_chunk);

    let fallback = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&fallback);
    let fallback_json = stdout_json(&fallback);

    assert_eq!(
        warning_codes(&fallback_json),
        vec!["embedding.coverage_partial"]
    );
    assert_eq!(
        fallback_json["data"]["results"], bm25_results,
        "partial embedding coverage must disable hybrid and preserve BM25 result schema/content"
    );
    assert_eq!(
        fallback_json["data"]["results"][0]["ranking"]["kind"],
        "bm25"
    );
    assert!(fallback_json["data"]["results"][0]["ranking"]
        .get("rrf_rank_score")
        .is_none());
}

#[cfg(feature = "vector-search")]
#[test]
fn missing_embedding_runtime_warns_and_does_not_break_local_commands() {
    let fixture = TestFixture::new("embedding-runtime-missing-fallback");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let source_id = "qgh://github.com/issue/I_kwDOISSUE1";
    let bm25_query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&bm25_query);
    let bm25_results = stdout_json(&bm25_query)["data"]["results"].clone();
    let request_count_after_seed = server.request_count();

    let model_path = "/definitely/not/a/model";
    fixture.write_config_with_embedding(
        &server.base_url,
        &format!(
            r#"
provider = "local"
model_path = "{model_path}"
file = "onnx/model_quantized.onnx"
pooling = "cls"
query_prefix = "query: "
quantization = "none"
"#
        ),
    );
    fixture.initialize_embedding_schema_for_test();
    let chunk_id = fixture.insert_chunk_for_source(source_id, "issue embedding chunk");
    fixture.insert_active_embedding_fingerprint_with_revision(
        &format!("model_path:{model_path}"),
        LOCAL_MODEL_REVISION,
    );
    fixture.insert_embedding_for_chunk(chunk_id);

    let sync = fixture.qgh(["sync", "--if-stale", "--max-age", "30m", "--json"]);
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    assert_eq!(sync_json["data"]["sync_state"], "skipped_fresh");
    assert!(warning_codes(&sync_json).is_empty());
    assert_eq!(
        server.request_count(),
        request_count_after_seed,
        "fresh sync fallback must stay local"
    );

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    assert_eq!(
        warning_codes(&query_json),
        vec!["embedding.runtime_unavailable"]
    );
    assert_eq!(
        json_object_keys(&query_json["warnings"][0]),
        BTreeSet::from([
            "code".to_string(),
            "message".to_string(),
            "severity".to_string(),
        ])
    );
    assert_eq!(
        query_json["data"]["results"], bm25_results,
        "runtime fallback must preserve BM25 result schema/content"
    );

    let get = fixture.qgh(["get", source_id, "--json"]);
    assert_success(&get);
    assert_eq!(stdout_json(&get)["data"]["source"]["source_id"], source_id);

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    assert_eq!(
        stdout_json(&status)["data"]["embedding"]["state"],
        "missing"
    );
}

#[cfg(feature = "vector-search")]
#[test]
fn force_embed_uses_local_snapshot_without_advancing_remote_freshness() {
    let fixture = TestFixture::new("embed-local-rebuild-freshness");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    fixture.write_default_embedding_config(&server.base_url);
    let backdated = "2020-01-01T00:00:00Z";
    let db_path = fixture.data_home.join("qgh/profiles/work/qgh.sqlite3");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE sync_runs SET completed_at = ?1
         WHERE snapshot_kind = 'remote_sync'",
        [backdated],
    )
    .unwrap();
    conn.execute(
        "UPDATE repository_sync_state SET last_successful_sync_at = ?1",
        [backdated],
    )
    .unwrap();
    drop(conn);
    let document_vectors = json!({
        "Repository: github.com/owner/repo\nIssue #42: Cache sync bug\n\nThe BM25 issue body tracer must round-trip through get before citation.": [0.1, 0.2, 0.3],
        "Repository: github.com/owner/repo\nComment on issue #42: Cache sync bug\n\nThe answer lives in this comment-only mitigation note.": [0.3, 0.2, 0.1]
    });

    let first_embed =
        fixture.qgh_with_document_vectors(["embed", "--force", "--json"], &document_vectors);
    assert_success(&first_embed);
    assert_eq!(
        stdout_json(&first_embed)["data"]["embedding_state"],
        "refreshed"
    );
    let issue_source_id = "qgh://github.com/issue/I_kwDOISSUE1";
    let raw_body_before = stdout_json(&fixture.qgh(["get", issue_source_id, "--json"]))["data"]
        ["source"]["body"]
        .clone();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE chunks SET chunker_fingerprint = 'legacy-force-embed-fingerprint'",
        [],
    )
    .unwrap();
    drop(conn);

    let repaired_embed =
        fixture.qgh_with_document_vectors(["embed", "--force", "--json"], &document_vectors);
    assert_success(&repaired_embed);
    assert_eq!(
        stdout_json(&repaired_embed)["data"]["embedding_state"],
        "refreshed"
    );
    let repaired_chunk_ids = fixture.sqlite_chunk_ids_for_source(issue_source_id);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let stale_fingerprint_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM chunks
             WHERE chunker_fingerprint IS NOT ?1",
            [qgh::chunking::CHUNKER_FINGERPRINT],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stale_fingerprint_count, 0);
    drop(conn);
    assert_eq!(
        stdout_json(&fixture.qgh(["get", issue_source_id, "--json"]))["data"]["source"]["body"],
        raw_body_before
    );

    let idempotent_embed =
        fixture.qgh_with_document_vectors(["embed", "--force"], &document_vectors);
    assert_success(&idempotent_embed);
    let embed_stdout = stdout_text(&idempotent_embed);
    assert!(embed_stdout.contains("text chunks rebuilt: 0"));
    assert!(embed_stdout.contains("vectors generated: 2"));
    assert!(!embed_stdout.contains("chunks: refreshed"));
    let embed_stderr = stderr_text(&idempotent_embed);
    assert!(embed_stderr.contains("qgh embed: generating vectors total=2"));
    assert!(embed_stderr.contains("qgh embed: generated vectors=2/2"));
    assert!(!embed_stderr.contains("BM25 issue body tracer"));
    assert_eq!(
        fixture.sqlite_chunk_ids_for_source(issue_source_id),
        repaired_chunk_ids
    );

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let remote_completed_at: String = conn
        .query_row(
            "SELECT completed_at FROM sync_runs
             WHERE snapshot_kind = 'remote_sync' ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let repository_sync_at: String = conn
        .query_row(
            "SELECT last_successful_sync_at FROM repository_sync_state
             WHERE repo = 'owner/repo'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remote_completed_at, backdated);
    assert_eq!(repository_sync_at, backdated);
    let local_snapshot_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM sync_runs
             WHERE snapshot_kind = 'local_rebuild' AND completed_successfully = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(local_snapshot_count, 3);
    drop(conn);
    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(status["data"]["sync"]["last_sync_at"], backdated);

    let request_count_before = server.request_count();
    let sync = fixture.qgh(["sync", "--if-stale", "--max-age", "30m", "--json"]);
    assert_success(&sync);
    assert!(
        server.request_count() > request_count_before,
        "force embed must not make a stale remote snapshot look fresh"
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn cached_prepared_manifest_query_is_offline_and_falls_back_to_bm25() {
    let fixture = TestFixture::new("prepared-manifest-offline-query");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let bm25 = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&bm25);
    let bm25_results = stdout_json(&bm25)["data"]["results"].clone();
    let request_count = server.request_count();
    let (manifest_path, manifest_hash) = fixture.write_prepared_embedding_manifest();
    fixture.write_config_with_embedding(
        &server.base_url,
        &format!(
            "provider = \"local\"\nmanifest_path = \"{}\"",
            manifest_path.display()
        ),
    );

    let acquisition = fixture.qgh(["sync", "--if-stale", "--max-age", "30m", "--json"]);
    assert_success(&acquisition);
    assert_eq!(
        server.request_count(),
        request_count,
        "prepared model acquisition during a fresh sync must not contact GitHub"
    );

    fixture.initialize_embedding_schema_for_test();
    let chunk_id = fixture.insert_chunk_for_source(
        "qgh://github.com/issue/I_kwDOISSUE1",
        "prepared manifest runtime fixture",
    );
    fixture
        .insert_active_embedding_fingerprint_with_revision("local:offline-fixture", &manifest_hash);
    fixture.insert_embedding_for_chunk(chunk_id);

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    assert_eq!(
        warning_codes(&query_json),
        vec!["embedding.runtime_unavailable"]
    );
    assert_eq!(query_json["data"]["results"], bm25_results);
    assert_eq!(
        server.request_count(),
        request_count,
        "query must not contact GitHub or a model host"
    );
}

#[test]
fn embed_without_embedding_config_reports_not_configured_before_force() {
    let fixture = TestFixture::new("embed-missing-embedding-config");
    fixture.write_config("http://127.0.0.1:1");

    let embed = fixture.qgh(["embed", "--json"]);

    assert_eq!(embed.status.code(), Some(2));
    let embed_json = stdout_json(&embed);
    assert_eq!(embed_json["error"]["code"], "embedding.not_configured");
    assert!(embed_json["error"]["hint"]
        .as_str()
        .unwrap()
        .contains("qgh embed --force"));
}

#[test]
fn embed_requires_force_for_full_refresh() {
    let fixture = TestFixture::new("embed-requires-force");
    fixture.write_default_embedding_config("http://127.0.0.1:1");

    let embed = fixture.qgh(["embed", "--json"]);

    assert_eq!(embed.status.code(), Some(2));
    let embed_json = stdout_json(&embed);
    assert_eq!(embed_json["error"]["code"], "embedding.force_required");
    assert!(embed_json["error"]["hint"]
        .as_str()
        .unwrap()
        .contains("qgh embed --force"));
}

#[test]
fn embedding_config_rejects_unknown_keys_and_non_local_provider() {
    for (fixture_name, embedding, expected_message_fragment) in [
        (
            "embedding-unknown-key",
            r#"
provider = "local"
providre = "local"
"#,
            "unknown field",
        ),
        (
            "embedding-openai-compatible",
            r#"provider = "openai-compatible""#,
            "unknown variant",
        ),
        (
            "embedding-invalid-provider",
            r#"provider = "remote""#,
            "unknown variant",
        ),
        (
            "embedding-literal-token",
            r#"
provider = "local"
token = "hf_literal_token_forbidden"
"#,
            "unknown field",
        ),
        (
            "embedding-invalid-query-prefix",
            r#"
provider = "local"
query_prefix = "Query: "
"#,
            "lowercase `query: ` prefix",
        ),
        (
            "embedding-model-and-path",
            r#"
provider = "local"
model = "hf:Snowflake/snowflake-arctic-embed-l-v2.0"
model_path = "/tmp/model.onnx"
"#,
            "only one of `model` or `model_path`",
        ),
        (
            "qwen-embedding-contract-override",
            r#"
provider = "local"
model = "qwen3-embedding-0.6b"
pooling = "mean"
"#,
            "cannot be combined",
        ),
        (
            "qwen-embedding-invalid-device",
            r#"
provider = "local"
model = "qwen3-embedding-0.6b"
device = "cuda"
"#,
            "unknown variant",
        ),
        (
            "onnx-embedding-device-forbidden",
            r#"
provider = "local"
model = "arctic-m-v2-fp32"
device = "cpu"
"#,
            "only valid with the Qwen embedding preset",
        ),
    ] {
        let fixture = TestFixture::new(fixture_name);
        fixture.write_config_with_embedding("http://127.0.0.1:1", embedding);

        let status = fixture.qgh(["status", "--json"]);
        assert_eq!(status.status.code(), Some(2));
        let status_json = stdout_json(&status);
        assert_eq!(status_json["ok"], false);
        assert_eq!(status_json["error"]["code"], "config.invalid");
        assert!(
            status_json["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected_message_fragment),
            "unexpected embedding config error: {status_json}"
        );
    }
}

#[test]
fn qwen_embedding_preset_is_strict_and_status_stays_local_only() {
    let fixture = TestFixture::new("qwen-embedding-config-status");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_embedding(
        &server.base_url,
        r#"
provider = "local"
model = "qwen3-embedding-0.6b"
device = "auto"
"#,
    );

    let status = fixture.qgh(["status", "--json"]);

    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(
        status_json["data"]["embedding"]["configured_model"]["model"],
        "qwen3-embedding-0.6b"
    );
    assert_eq!(
        status_json["data"]["embedding"]["configured_model"]["model_id"],
        "Qwen/Qwen3-Embedding-0.6B"
    );
    assert_eq!(
        server.request_count(),
        0,
        "status must not download a model"
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn qwen_status_checks_snapshot_layout_while_doctor_keeps_full_validation_explicit() {
    let fixture = TestFixture::new("qwen-status-no-artifact-validation");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    fixture.write_config_with_embedding(
        &server.base_url,
        r#"
provider = "local"
model = "qwen3-embedding-0.6b"
device = "auto"
"#,
    );
    let snapshot = fixture
        .cache_home
        .join("qgh/prepared-qwen-models/qwen3-embedding-0.6b");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(
        snapshot.join("manifest.json"),
        b"private malformed model marker",
    )
    .unwrap();

    let requests_before_local_reads = server.request_count();
    let status = fixture.qgh(["status", "--json"]);

    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["embedding"]["state"], "corrupt");
    let status_output = format!("{}{}", stdout_text(&status), stderr_text(&status));
    assert!(!status_output.contains("private malformed model marker"));
    assert_eq!(server.request_count(), requests_before_local_reads);

    let exact = fixture.qgh(["query", "https://github.com/owner/repo/issues/42", "--json"]);
    assert_success(&exact);
    let exact_json = stdout_json(&exact);
    assert_eq!(exact_json["data"]["results"][0]["ranking"]["kind"], "exact");
    let exact_output = format!("{}{}", stdout_text(&exact), stderr_text(&exact));
    assert!(!exact_output.contains("private malformed model marker"));
    assert_eq!(server.request_count(), requests_before_local_reads);

    let doctor = fixture.qgh(["doctor", "--json"]);
    assert_success(&doctor);
    let doctor_json = stdout_json(&doctor);
    let checks = doctor_json["data"]["checks"].as_array().unwrap();
    assert_eq!(doctor_check_ok(checks, "embedding_artifacts"), Some(false));
    assert_eq!(doctor_check_ok(checks, "embedding_runtime"), Some(false));
    let doctor_output = format!("{}{}", stdout_text(&doctor), stderr_text(&doctor));
    assert!(!doctor_output.contains("private malformed model marker"));
}

#[test]
fn model_install_cli_rejects_unknown_presets_before_profile_resolution() {
    let fixture = TestFixture::new("model-install-strict-preset");

    let output = fixture.qgh(["model", "install", "unknown-model", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    let output_json = stdout_json(&output);
    assert_eq!(output_json["error"]["code"], "validation.cli");
    let message = output_json["error"]["message"].as_str().unwrap();
    assert!(message.contains("qwen3-embedding-0.6b"));
    assert!(message.contains("qwen3-reranker-0.6b"));
}

#[cfg(not(feature = "fastembed-provider"))]
#[test]
fn bm25_binary_reports_unavailable_model_installer_without_resolving_a_profile() {
    let fixture = TestFixture::new("model-install-bm25-binary");

    let output =
        fixture.qgh_without_profile(["model", "install", "qwen3-embedding-0.6b", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    let output_json = stdout_json(&output);
    assert_eq!(output_json["error"]["code"], "model.provider_unavailable");
    assert!(output_json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("without local Qwen model installation support"));
}

#[test]
fn active_issue_max_age_tightens_query_freshness_and_lists_all_triggers() {
    let fixture = TestFixture::new("active-issue-freshness");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_freshness(&server.base_url, Some("30m"), Some("warn"), Some("1s"));

    assert_success(&fixture.qgh(["sync", "--json"]));
    fixture.set_last_sync_age_seconds(3_600);

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    assert_eq!(query_json["data"]["freshness"]["decision"], "stale_warn");
    assert_eq!(query_json["data"]["freshness"]["max_age_seconds"], 1);
    let warning_codes = query_json["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|warning| warning["code"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        warning_codes,
        [
            "freshness.query_snapshot_stale",
            "freshness.active_issue_snapshot_stale"
        ]
    );
    assert_eq!(query_json["warnings"][1]["severity"], "warn_strong");
}

#[test]
fn coverage_metadata_surfaces_partial_and_warns_on_no_result() {
    let fixture = TestFixture::new("coverage-metadata");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));

    // status exposes the coverage component block. A completed full-profile sync
    // covers open issues, so open_backfill_complete is set, but historical
    // backfill is still pending so the corpus stays partial.
    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["coverage"]["mode"], "partial");
    assert_eq!(
        status_json["data"]["coverage"]["open_backfill_complete"],
        true
    );
    assert_eq!(
        status_json["data"]["coverage"]["historical_backfill_complete"],
        false
    );
    assert_eq!(
        status_json["data"]["coverage"]["history_cursor"],
        Value::Null
    );
    assert!(status_json["data"]["coverage"]["recent_bootstrap_floor"].is_string());

    let requests_before_human_status = server.request_count();
    let human_status = fixture.qgh(["status"]);
    assert_success(&human_status);
    assert_eq!(server.request_count(), requests_before_human_status);
    let human_status = stdout_text(&human_status);
    assert!(human_status.contains("qgh status — search ready"));
    assert!(human_status.contains("search: BM25 ready; semantic not configured"));
    assert!(human_status.contains("coverage: partial; historical coverage incomplete"));
    assert!(human_status.contains("next: qgh sync --backfill --all --profile work"));
    assert!(!human_status.contains("next: qgh sync --all"));

    // a no-result query on a partial corpus gets a strong coverage warning.
    let empty = fixture.qgh(["query", "zzznomatchqgh", "--json"]);
    assert_success(&empty);
    let empty_json = stdout_json(&empty);
    assert_eq!(empty_json["data"]["results"].as_array().unwrap().len(), 0);
    assert_eq!(empty_json["data"]["coverage"]["mode"], "partial");
    let coverage_warning = empty_json["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|warning| warning["code"] == "coverage.partial_no_result")
        .expect("partial coverage no-result warning");
    assert_eq!(coverage_warning["severity"], "warn_strong");

    let empty_human_output = fixture.qgh(["query", "zzznomatchqgh"]);
    assert_success(&empty_human_output);
    let empty_human = stdout_text(&empty_human_output);
    assert!(empty_human.contains("qgh query — 0 source candidates"));
    assert!(empty_human.contains("coverage: partial; historical coverage incomplete"));
    assert!(empty_human.contains("No matches in the current partial corpus."));
    assert!(empty_human.contains("next: qgh sync --backfill --all --profile work"));
    assert!(
        stderr_text(&empty_human_output).contains("strong warning [coverage.partial_no_result]")
    );

    // an exact-locator no-result on the same partial corpus must NOT fire the
    // FTS coverage backfill warning: the locator was filtered/unresolved, not a
    // coverage gap.
    let locator = fixture.qgh(["query", "999999", "--json"]);
    assert_success(&locator);
    let locator_json = stdout_json(&locator);
    assert_eq!(locator_json["data"]["results"].as_array().unwrap().len(), 0);
    assert_eq!(locator_json["data"]["coverage"]["mode"], "partial");
    assert!(locator_json["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .all(|warning| warning["code"] != "coverage.partial_no_result"));

    // mode is derived: once both backfills complete, mode flips to complete and
    // the no-result coverage warning disappears.
    let db_path = fixture.data_home.join("qgh/profiles/work/qgh.sqlite3");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO coverage_state (id, open_backfill_complete, historical_backfill_complete)
         VALUES (1, 1, 1)
         ON CONFLICT(id) DO UPDATE SET
            open_backfill_complete = 1,
            historical_backfill_complete = 1,
            historical_scope_fingerprint = open_scope_fingerprint",
        [],
    )
    .unwrap();
    drop(conn);

    let complete = fixture.qgh(["query", "zzznomatchqgh", "--json"]);
    assert_success(&complete);
    let complete_json = stdout_json(&complete);
    assert_eq!(complete_json["data"]["coverage"]["mode"], "complete");
    assert!(complete_json["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .all(|warning| warning["code"] != "coverage.partial_no_result"));
}

#[test]
fn full_sync_seeds_coverage_and_fixes_bootstrap_floor() {
    let fixture = TestFixture::new("coverage-bootstrap-floor");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(status["data"]["coverage"]["open_backfill_complete"], true);
    assert!(status["data"]["coverage"]["oldest_synced_updated_at"].is_string());
    assert!(status["data"]["coverage"]["recent_bootstrap_floor"].is_string());
    assert!(status["data"]["coverage"]["open_cursor"].is_string());

    // The bootstrap floor is fixed at first seed and must not be re-derived from
    // `now` on later syncs. Set a distinctive past floor, re-sync, confirm it
    // survives unchanged.
    let db_path = fixture.data_home.join("qgh/profiles/work/qgh.sqlite3");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE coverage_state SET recent_bootstrap_floor = '2020-01-01T00:00:00Z'",
        [],
    )
    .unwrap();
    drop(conn);

    assert_success(&fixture.qgh(["sync", "--json"]));
    let status_after = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(
        status_after["data"]["coverage"]["recent_bootstrap_floor"],
        "2020-01-01T00:00:00Z"
    );
}

#[test]
fn sync_if_stale_skips_when_fresh_and_runs_when_stale() {
    let fixture = TestFixture::new("sync-if-stale");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));

    // Fresh snapshot: --if-stale is a no-op that does not call GitHub.
    let skipped = fixture.qgh(["sync", "--if-stale", "--max-age", "30m", "--json"]);
    assert_success(&skipped);
    let skipped_json = stdout_json(&skipped);
    assert_eq!(skipped_json["data"]["sync_state"], "skipped_fresh");
    assert_eq!(skipped_json["data"]["sync"]["max_age_seconds"], 1800);

    let requests_before_human_skip = server.request_count();
    let human_skip = fixture.qgh(["sync", "--if-stale", "--max-age", "30m"]);
    assert_success(&human_skip);
    assert_eq!(server.request_count(), requests_before_human_skip);
    let human_stdout = stdout_text(&human_skip);
    assert!(human_stdout.contains("qgh sync skipped — local snapshot is fresh"));
    assert!(human_stdout.contains("repo scope: all profile repos"));
    assert!(human_stdout.contains("network: skipped; no GitHub request was needed"));
    assert!(human_stdout.contains("snapshot age: "));
    assert!(human_stdout.contains("max age: 1800 seconds"));
    assert!(human_stdout.contains("next: qgh sync --backfill --all --profile work"));
    assert!(!human_stdout.contains("n/a"));

    // Age the snapshot past max-age: --if-stale now runs a real sync.
    fixture.set_last_sync_age_seconds(3_600);
    let ran = fixture.qgh(["sync", "--if-stale", "--max-age", "30m", "--json"]);
    assert_success(&ran);
    assert_eq!(stdout_json(&ran)["data"]["sync_state"], "ok");
}

#[test]
fn sync_if_stale_repairs_detached_pre_epoch_publication_even_when_remote_sync_is_fresh() {
    let fixture = TestFixture::new("sync-if-stale-pre-epoch-publication");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let db_path = fixture.data_home.join("qgh/profiles/work/qgh.sqlite3");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let original_publication_id: i64 = conn
        .query_row(
            "SELECT publication_id FROM retrieval_publication_pointer WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "UPDATE retrieval_publications SET source_snapshot_epoch = NULL WHERE active = 1",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE index_generations
         SET source_snapshot_epoch = NULL, source_inventory_hash = NULL
         WHERE active = 1",
        [],
    )
    .unwrap();
    drop(conn);

    let requests_before_repair = server.request_count();
    let repaired = fixture.qgh(["sync", "--if-stale", "--max-age", "30m", "--json"]);
    assert_success(&repaired);
    assert_eq!(stdout_json(&repaired)["data"]["sync_state"], "ok");
    assert!(server.request_count() > requests_before_repair);

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let successor_publication_id: i64 = conn
        .query_row(
            "SELECT publication_id FROM retrieval_publication_pointer WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(successor_publication_id, original_publication_id);
    drop(conn);

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    assert!(!stdout_json(&query)["data"]["results"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn sync_if_stale_repairs_missing_active_tantivy_even_when_remote_sync_is_fresh() {
    let fixture = TestFixture::new("sync-if-stale-missing-active-tantivy");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    let original_publication_id = fixture.active_retrieval_publication_id();
    fixture.remove_active_tantivy_generation();
    let requests_before_repair = server.request_count();

    let repaired = fixture.qgh(["sync", "--if-stale", "--max-age", "30m", "--json"]);
    assert_success(&repaired);
    assert_eq!(stdout_json(&repaired)["data"]["sync_state"], "ok");
    assert!(server.request_count() > requests_before_repair);
    assert_ne!(
        fixture.active_retrieval_publication_id(),
        original_publication_id
    );
    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    assert!(!stdout_json(&query)["data"]["results"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn reconcile_recent_window_tombstones_with_unified_reason_code() {
    let fixture = TestFixture::new("recent-reconciliation");
    let server = LifecycleFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    let comment_source_id = "qgh://github.com/issue-comment/IC_kwDOCOMMENT1";

    server.set_mode(LIFECYCLE_DELETED_COMMENT);
    let reconcile = fixture.qgh([
        "sync",
        "--reconcile",
        "recent",
        "--window",
        "120mo",
        "--json",
    ]);
    assert_success(&reconcile);
    let reconcile_json = stdout_json(&reconcile);
    assert_eq!(reconcile_json["data"]["reconciliation"]["mode"], "recent");
    assert_eq!(
        reconcile_json["data"]["reconciliation"]["tombstoned_sources"],
        1
    );

    // Reason code is unified with full reconcile and targeted refresh.
    let deleted_get = fixture.qgh(["get", comment_source_id, "--json"]);
    assert_eq!(deleted_get.status.code(), Some(4));
    let deleted_get_json = stdout_json(&deleted_get);
    assert_eq!(deleted_get_json["error"]["code"], "source.tombstoned");
    assert_eq!(deleted_get_json["error"]["details"]["reason"], "deleted");
}

#[test]
fn reconcile_recent_window_excludes_old_sources_and_validates_flag() {
    let fixture = TestFixture::new("recent-window-exclusion");
    let server = LifecycleFakeGitHub::start();
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    let comment_source_id = "qgh://github.com/issue-comment/IC_kwDOCOMMENT1";

    server.set_mode(LIFECYCLE_DELETED_COMMENT);
    // Synced sources carry past GitHub updated_at; a 1-second window excludes
    // them, so the deleted comment is never rechecked or tombstoned by recent.
    let narrow = fixture.qgh(["sync", "--reconcile", "recent", "--window", "1s", "--json"]);
    assert_success(&narrow);
    let narrow_json = stdout_json(&narrow);
    assert_eq!(narrow_json["data"]["reconciliation"]["mode"], "recent");
    assert_eq!(
        narrow_json["data"]["reconciliation"]["tombstoned_sources"],
        0
    );
    // The comment is still an active local source (excluded from the recheck).
    assert_success(&fixture.qgh(["get", comment_source_id, "--json"]));

    // --window is only valid with --reconcile recent.
    let rejected = fixture.qgh(["sync", "--window", "7d", "--json"]);
    assert_eq!(rejected.status.code(), Some(2));
    assert_eq!(
        stdout_json(&rejected)["error"]["code"],
        "validation.window_requires_recent"
    );
}

#[test]
fn repo_listing_comments_upsert_issue_comments_and_skip_pull_request_comments() {
    let fixture = TestFixture::new("repo-listing-comments");
    let server = RepoCommentListingFakeGitHub::start();
    fixture.write_config_repo_listing_comments(&server.base_url);

    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    assert_eq!(sync_json["data"]["comment_listing"]["mode"], "repo_listing");
    assert_eq!(
        sync_json["data"]["comment_listing"]["skipped_pr_comments"],
        1
    );
    // The comment whose parent issue is not in the local corpus is deferred
    // (held for retry), not silently dropped or guessed.
    assert_eq!(sync_json["data"]["comment_listing"]["deferred_comments"], 1);
    assert_eq!(sync_json["data"]["comments"]["upserted"], 1);

    // The issue-parent comment is indexed; the PR-parent comment was skipped.
    let query = fixture.qgh(["query", "repo level comment tracer", "--json"]);
    assert_success(&query);
    let comment_hits = stdout_json(&query)["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|result| result["entity_type"] == "issue_comment")
        .count();
    assert_eq!(comment_hits, 1);

    // The repo-level listing endpoint was used instead of per-issue comments.
    let requests = server.requests();
    assert!(requests
        .iter()
        .any(|line| line.starts_with("GET /repos/owner/repo/issues/comments?")));
    assert!(requests
        .iter()
        .all(|line| !line.starts_with("GET /repos/owner/repo/issues/1/comments?")));
}

#[test]
fn repo_listing_permission_evidence_is_pending_before_comment_upsert_failure() {
    let fixture = TestFixture::new("repo-listing-permission-before-upsert");
    let server = RepoCommentListingFakeGitHub::start();
    fixture.write_config_repo_listing_comments(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    fixture.install_repo_comment_upsert_failure_trigger();
    server.set_mode(REPO_COMMENT_LISTING_PERMISSION_AFTER_PAGE);

    let sync = fixture.qgh(["sync", "--json"]);
    assert_eq!(sync.status.code(), Some(6));
    assert_eq!(stdout_json(&sync)["error"]["code"], "storage.failure");
    let output = format!("{}{}", stdout_text(&sync), stderr_text(&sync));
    assert!(!output.contains("repo listing pending evidence tracer"));
    assert!(!output.contains("fixture-token"));

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["purge"]["pending_count"], 1);
    assert_eq!(
        status_json["data"]["purge"]["triggers"],
        json!(["permission_loss"])
    );
    assert_eq!(status_json["data"]["purge"]["retrieval_blocked"], true);
}

#[test]
fn repo_listing_permission_purge_recaptures_rows_processed_after_queue() {
    let fixture = TestFixture::new("repo-listing-permission-recaptures-row");
    let server = RepoCommentListingFakeGitHub::start();
    fixture.write_config_repo_listing_comments(&server.base_url);
    server.set_mode(REPO_COMMENT_LISTING_PERMISSION_AFTER_PAGE);

    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    assert_eq!(sync_json["data"]["comments"]["upserted"], 1);

    for source_id in [
        "qgh://github.com/issue/I_REPO_LISTING_1",
        "qgh://github.com/issue-comment/IC_REPO_LISTING_PENDING",
    ] {
        let get = fixture.qgh(["get", source_id, "--json"]);
        assert_eq!(get.status.code(), Some(4));
        assert_eq!(
            stdout_json(&get)["error"]["details"]["reason"],
            "permission_loss"
        );
    }
    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(status["data"]["purge"]["pending_count"], 0);
    assert_eq!(status["data"]["purge"]["retrieval_blocked"], false);
}

#[test]
fn repo_listing_permission_purge_canonicalizes_mixed_case_comment_repo() {
    let fixture = TestFixture::new("repo-listing-permission-mixed-case");
    let server = RepoCommentListingFakeGitHub::start();
    fixture.write_config_repo_listing_comments(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    fixture.set_issue_metadata_repo_casing("qgh://github.com/issue/I_REPO_LISTING_1", "OWNER/REPO");

    fixture.write_config_repo_listing_comments_with_repo(&server.base_url, "OWNER/REPO");
    server.set_mode(REPO_COMMENT_LISTING_PERMISSION_AFTER_PAGE);
    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);

    let mixed_case_comment = fixture.qgh([
        "get",
        "qgh://github.com/issue-comment/IC_REPO_LISTING_PENDING",
        "--json",
    ]);
    assert_eq!(mixed_case_comment.status.code(), Some(4));
    assert_eq!(
        stdout_json(&mixed_case_comment)["error"]["details"]["reason"],
        "permission_loss"
    );
    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(status["data"]["purge"]["pending_count"], 0);
    assert_eq!(status["data"]["purge"]["retrieval_blocked"], false);
}

#[test]
fn backfill_walks_history_and_completes_coverage() {
    let fixture = TestFixture::new("historical-backfill");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);

    // Live sync first: records open coverage + the fixed bootstrap floor.
    assert_success(&fixture.qgh(["sync", "--json"]));
    let after_live = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(
        after_live["data"]["coverage"]["open_backfill_complete"],
        true
    );
    assert_eq!(
        after_live["data"]["coverage"]["historical_backfill_complete"],
        false
    );
    assert_eq!(after_live["data"]["coverage"]["mode"], "partial");

    // Backfill walks history to the end and completes historical coverage.
    let backfill = fixture.qgh(["sync", "--backfill", "--max-requests", "50"]);
    assert_success(&backfill);
    let backfill_stdout = stdout_text(&backfill);
    assert!(backfill_stdout.contains("qgh historical backfill pass complete"));
    assert!(backfill_stdout.contains("fetched: issues 1, comments 1, skipped PRs 1"));
    assert!(backfill_stdout.contains("coverage: complete"));
    assert!(backfill_stdout.contains("history cursor:"));
    assert!(backfill_stdout.contains("next: qgh query <terms> --profile work"));
    assert!(!backfill_stdout.contains("fetched -"));

    // Coverage now derives to complete (open + historical both done).
    let after_backfill = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(
        after_backfill["data"]["coverage"]["historical_backfill_complete"],
        true
    );
    assert_eq!(after_backfill["data"]["coverage"]["mode"], "complete");
    assert!(after_backfill["data"]["coverage"]["history_cursor"].is_string());
}

#[test]
fn single_repo_worktree_sync_and_backfill_reach_complete_coverage() {
    let fixture = TestFixture::new("single-repo-worktree-coverage");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    let worktree = fixture.init_git_worktree_with_repo_policy("owner/repo");

    assert_success(&fixture.qgh_in(&worktree, ["sync", "--json"]));
    let after_live = stdout_json(&fixture.qgh_in(&worktree, ["status", "--json"]));
    assert_eq!(
        after_live["data"]["coverage"]["open_backfill_complete"],
        true
    );
    assert_eq!(
        after_live["data"]["coverage"]["next_action"]["command"],
        "qgh sync --backfill --all --profile work"
    );

    let backfill = fixture.qgh_in(&worktree, ["sync", "--backfill", "--all"]);
    assert_success(&backfill);
    let backfill_stdout = stdout_text(&backfill);
    assert!(backfill_stdout.contains("coverage: complete"));
    assert!(backfill_stdout.contains("open coverage complete: true"));
    assert!(backfill_stdout.contains("historical coverage complete: true"));
    assert!(backfill_stdout.contains("next: qgh query <terms> --profile work"));

    let complete = stdout_json(&fixture.qgh_in(&worktree, ["status", "--json"]));
    assert_eq!(complete["data"]["coverage"]["mode"], "complete");
    assert_eq!(complete["data"]["coverage"]["next_action"], Value::Null);
}

#[test]
fn repo_scoped_live_sync_guides_full_profile_coverage_before_backfill() {
    let fixture = TestFixture::new("repo-scoped-open-coverage-action");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    let worktree = fixture.init_git_worktree_with_repo_policy("owner/repo");

    assert_success(&fixture.qgh_in(&worktree, ["sync", "--json"]));
    let status = fixture.qgh_in(&worktree, ["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(
        status_json["data"]["coverage"]["open_backfill_complete"],
        false
    );
    assert_eq!(
        status_json["data"]["coverage"]["next_action"]["command"],
        "qgh sync --all --profile work"
    );
    assert_eq!(
        status_json["data"]["coverage"]["next_action"]["json_command"],
        "qgh sync --all --profile work --json"
    );
    assert_eq!(
        status_json["data"]["coverage"]["next_action"]["reason"],
        "open_coverage_incomplete"
    );

    let human_status = fixture.qgh_in(&worktree, ["status"]);
    assert_success(&human_status);
    assert!(stdout_text(&human_status)
        .contains("coverage: partial; open and historical coverage incomplete"));
    assert!(stdout_text(&human_status).contains("next: qgh sync --all --profile work"));

    let no_result = fixture.qgh_in(&worktree, ["query", "zzznomatchqgh", "--json"]);
    assert_success(&no_result);
    let no_result_json = stdout_json(&no_result);
    assert_eq!(
        no_result_json["data"]["coverage"]["next_action"]["command"],
        "qgh sync --all --profile work"
    );
    assert!(no_result_json["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning["code"] == "coverage.partial_no_result"
            && warning["message"]
                .as_str()
                .unwrap()
                .contains("coverage.next_action")));

    let scoped_backfill = fixture.qgh_in(&worktree, ["sync", "--backfill"]);
    assert_success(&scoped_backfill);
    let scoped_stdout = stdout_text(&scoped_backfill);
    assert!(scoped_stdout.contains("coverage: partial"));
    assert!(scoped_stdout.contains("open coverage complete: false"));
    assert!(scoped_stdout.contains("historical coverage complete: false"));
    assert!(scoped_stdout.contains("next: qgh sync --all --profile work"));
}

#[test]
fn profile_membership_change_invalidates_completed_historical_coverage() {
    let fixture = TestFixture::new("coverage-profile-membership-change");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo"]);

    assert_success(&fixture.qgh(["sync", "--json"]));
    assert_success(&fixture.qgh(["sync", "--backfill", "--all", "--json"]));
    let initially_complete = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(initially_complete["data"]["coverage"]["mode"], "complete");

    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    assert_success(&fixture.qgh(["sync", "--all", "--json"]));
    let expanded = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(expanded["data"]["coverage"]["open_backfill_complete"], true);
    assert_eq!(
        expanded["data"]["coverage"]["historical_backfill_complete"],
        false
    );
    assert_eq!(expanded["data"]["coverage"]["mode"], "partial");
    assert_eq!(
        expanded["data"]["coverage"]["next_action"]["command"],
        "qgh sync --backfill --all --profile work"
    );

    assert_success(&fixture.qgh(["sync", "--backfill", "--all", "--json"]));
    let completed_again = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(completed_again["data"]["coverage"]["mode"], "complete");
    assert_eq!(
        completed_again["data"]["coverage"]["next_action"],
        Value::Null
    );
}

#[test]
fn repo_scoped_backfill_does_not_claim_profile_wide_completion() {
    let fixture = TestFixture::new("repo-scoped-backfill-coverage");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    let worktree = fixture.init_git_worktree_with_repo_policy("owner/repo");

    assert_success(&fixture.qgh_in(&worktree, ["sync", "--all", "--json"]));

    let limited_all = fixture.qgh_in(
        &worktree,
        ["sync", "--backfill", "--all", "--max-requests", "1"],
    );
    assert_success(&limited_all);
    assert!(stdout_text(&limited_all).contains("next: qgh sync --backfill --all --profile work"));

    let scoped = fixture.qgh_in(&worktree, ["sync", "--backfill"]);
    assert_success(&scoped);
    let scoped_stdout = stdout_text(&scoped);
    assert!(scoped_stdout.contains("repo scope: owner/repo"));
    assert!(scoped_stdout.contains("repo scope history end reached: true"));
    assert!(scoped_stdout.contains("coverage: partial"));
    assert!(scoped_stdout.contains("next: qgh sync --backfill --all --profile work"));

    let partial_status = fixture.qgh_in(&worktree, ["status", "--json"]);
    assert_success(&partial_status);
    assert_eq!(
        stdout_json(&partial_status)["data"]["coverage"]["historical_backfill_complete"],
        false
    );
    let human_status = fixture.qgh_in(&worktree, ["status"]);
    assert_success(&human_status);
    assert!(stdout_text(&human_status).contains("next: qgh sync --backfill --all --profile work"));

    let full = fixture.qgh_in(&worktree, ["sync", "--backfill", "--all", "--json"]);
    assert_success(&full);
    assert_eq!(
        stdout_json(&full)["data"]["backfill"]["historical_backfill_complete"],
        true
    );
}

#[test]
fn backfill_flag_conflicts_are_rejected() {
    let fixture = TestFixture::new("backfill-flag-conflicts");
    fixture.write_config("http://127.0.0.1:1");

    // --backfill excludes live-sync modifiers (validated before any network use).
    let with_reconcile = fixture.qgh(["sync", "--backfill", "--reconcile", "full", "--json"]);
    assert_eq!(with_reconcile.status.code(), Some(2));
    assert_eq!(
        stdout_json(&with_reconcile)["error"]["code"],
        "validation.backfill_conflicts"
    );

    let with_if_stale = fixture.qgh(["sync", "--backfill", "--if-stale", "--json"]);
    assert_eq!(with_if_stale.status.code(), Some(2));
    assert_eq!(
        stdout_json(&with_if_stale)["error"]["code"],
        "validation.backfill_conflicts"
    );

    // Budget flags require --backfill rather than being silently ignored.
    let orphan_budget = fixture.qgh(["sync", "--max-requests", "5", "--json"]);
    assert_eq!(orphan_budget.status.code(), Some(2));
    assert_eq!(
        stdout_json(&orphan_budget)["error"]["code"],
        "validation.requires_backfill"
    );

    // Freshness options and targeted-sync parent options are never ignored.
    let orphan_max_age = fixture.qgh(["sync", "--max-age", "30m", "--json"]);
    assert_eq!(orphan_max_age.status.code(), Some(2));
    assert_eq!(
        stdout_json(&orphan_max_age)["error"]["code"],
        "validation.max_age_requires_if_stale"
    );

    let targeted_with_backfill = fixture.qgh(["sync", "--backfill", "issue", "42", "--json"]);
    assert_eq!(targeted_with_backfill.status.code(), Some(2));
    assert_eq!(
        stdout_json(&targeted_with_backfill)["error"]["code"],
        "validation.cli"
    );
    assert!(stdout_json(&targeted_with_backfill)["error"]["hint"]
        .as_str()
        .unwrap()
        .contains("sync issue <number>"));

    let all_with_repo = fixture.qgh(["sync", "--all", "--repo", "owner/repo", "--json"]);
    assert_eq!(all_with_repo.status.code(), Some(2));
    assert_eq!(
        stdout_json(&all_with_repo)["error"]["code"],
        "validation.cli"
    );
}

#[test]
fn freshness_precedence_is_flag_then_repo_policy_then_profile_config() {
    let fixture = TestFixture::new("freshness-precedence");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_freshness(&server.base_url, Some("1s"), Some("warn"), None);
    let nested_worktree_dir = fixture.init_git_worktree_with_repo_policy("owner/repo");
    fixture.write_repo_policy_with_freshness("owner/repo", Some("12mo"), None, None);

    assert_success(&fixture.qgh_in(&nested_worktree_dir, ["sync", "--json"]));
    fixture.set_last_sync_age_seconds(3_600);

    let policy_fresh = fixture.qgh_in(&nested_worktree_dir, ["query", "BM25 tracer", "--json"]);
    assert_success(&policy_fresh);
    let policy_json = stdout_json(&policy_fresh);
    assert_eq!(policy_json["data"]["freshness"]["decision"], "fresh");
    assert_eq!(
        policy_json["data"]["freshness"]["max_age_seconds"],
        31_104_000
    );
    assert_eq!(policy_json["warnings"], json!([]));

    let flag_stale = fixture.qgh_in(
        &nested_worktree_dir,
        ["query", "BM25 tracer", "--max-age", "1s", "--json"],
    );
    assert_success(&flag_stale);
    let flag_json = stdout_json(&flag_stale);
    assert_eq!(flag_json["data"]["freshness"]["decision"], "stale_warn");
    assert_eq!(flag_json["data"]["freshness"]["max_age_seconds"], 1);
    assert_eq!(
        flag_json["warnings"][0]["code"],
        "freshness.query_snapshot_stale"
    );
}

#[test]
fn query_freshness_uses_effective_repo_sync_age_not_profile_latest_sync_run() {
    let fixture = TestFixture::new("repo-scoped-query-freshness");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_freshness_and_repos(
        &server.base_url,
        &["owner/repo", "other/repo"],
        Some("30m"),
        Some("warn"),
        None,
    );
    let nested_worktree_dir = fixture.init_git_worktree_with_repo_policy("owner/repo");

    assert_success(&fixture.qgh_in(&nested_worktree_dir, ["sync", "--all", "--json"]));
    fixture.set_repo_sync_age_seconds("owner/repo", 3_600);
    fixture.set_repo_sync_age_seconds("other/repo", 0);
    fixture.insert_profile_sync_run_now("sync-unrelated-other-repo");

    let scoped_query = fixture.qgh_in(
        &nested_worktree_dir,
        ["query", "shared repo policy tracer", "--json"],
    );
    assert_success(&scoped_query);
    let scoped_json = stdout_json(&scoped_query);
    assert_eq!(scoped_json["meta"]["repo"], "owner/repo");
    assert_eq!(scoped_json["data"]["freshness"]["decision"], "stale_warn");
    assert!(scoped_json["data"]["freshness"]["snapshot_age_seconds"]
        .as_i64()
        .is_some_and(|age| age >= 3_600));
    assert_eq!(
        scoped_json["warnings"][0]["code"],
        "freshness.query_snapshot_stale"
    );

    let scoped_status = fixture.qgh_in(&nested_worktree_dir, ["status", "--json"]);
    assert_success(&scoped_status);
    let scoped_status_json = stdout_json(&scoped_status);
    assert_eq!(scoped_status_json["meta"]["repo"], "owner/repo");
    assert_eq!(
        scoped_status_json["data"]["freshness"]["decision"],
        "stale_warn"
    );
    assert!(
        scoped_status_json["data"]["freshness"]["snapshot_age_seconds"]
            .as_i64()
            .is_some_and(|age| age >= 3_600)
    );
    assert_eq!(
        scoped_status_json["warnings"][0]["code"],
        "freshness.query_snapshot_stale"
    );

    let mcp_status = fixture.mcp_in(
        &nested_worktree_dir,
        Some("work"),
        [json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "status",
                "arguments": {}
            }
        })],
    );
    assert_success(&mcp_status);
    let mcp_messages = stdout_json_lines(&mcp_status);
    let mcp_status_json = &mcp_messages[0]["result"]["structuredContent"];
    assert_eq!(mcp_status_json["meta"]["repo"], "owner/repo");
    assert_eq!(
        mcp_status_json["data"]["freshness"]["decision"],
        "stale_warn"
    );
    assert_eq!(
        mcp_status_json["warnings"][0]["code"],
        "freshness.query_snapshot_stale"
    );

    let require_fresh = fixture.qgh_in(
        &nested_worktree_dir,
        [
            "query",
            "shared repo policy tracer",
            "--require-fresh",
            "--json",
        ],
    );
    assert_eq!(require_fresh.status.code(), Some(2));
    assert_eq!(
        stdout_json(&require_fresh)["error"]["code"],
        "freshness.stale"
    );

    let other_repo_query = fixture.qgh_in(
        &nested_worktree_dir,
        [
            "query",
            "shared repo policy tracer",
            "--repo",
            "other/repo",
            "--json",
        ],
    );
    assert_success(&other_repo_query);
    let other_json = stdout_json(&other_repo_query);
    assert_eq!(other_json["data"]["freshness"]["decision"], "fresh");
    assert_eq!(other_json["warnings"], json!([]));
}

#[test]
fn rate_limited_sync_is_retryable_failure_and_preserves_local_search() {
    let fixture = TestFixture::new("primary-rate-limit");
    let server = RateLimitFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    server.set_mode(RATE_LIMIT_PRIMARY);

    let limited_sync = fixture.qgh(["sync", "--json"]);
    assert_eq!(limited_sync.status.code(), Some(5));
    assert!(stderr_text(&limited_sync).is_empty());
    let limited_json = stdout_json(&limited_sync);
    assert_eq!(limited_json["ok"], false);
    assert_eq!(limited_json["error"]["code"], "sync.backoff");
    assert_eq!(limited_json["error"]["retryable"], true);
    assert_eq!(limited_json["error"]["exit_code"], 5);
    assert_eq!(limited_json["error"]["details"]["profile_id"], "work");
    assert_eq!(
        limited_json["error"]["details"]["reason"],
        "primary_rate_limit"
    );
    assert_eq!(
        limited_json["error"]["details"]["scope"],
        "issues:owner/repo"
    );
    assert_eq!(limited_json["error"]["details"]["retry_after_seconds"], 0);
    assert_eq!(
        limited_json["error"]["details"]["retry_command"],
        "qgh sync --all --profile work --json"
    );
    assert_eq!(
        limited_json["error"]["details"]["retry_action"],
        json!({
            "reason": "sync_backoff",
            "command": "qgh sync --all --profile work",
            "json_command": "qgh sync --all --profile work --json"
        })
    );
    assert!(limited_json["error"]["details"]["reset_at"]
        .as_str()
        .is_some());
    assert!(limited_json["error"]["details"]["retry_at"]
        .as_str()
        .is_some());
    assert!(limited_json["error"]["details"]["last_successful_sync"]
        .as_str()
        .is_some());
    assert_eq!(
        limited_json["error"]["details"]["local_retrieval_available"],
        true
    );
    let limited_observations = limited_json["error"]["details"]["rate_budget"]["observations"]
        .as_array()
        .unwrap();
    assert_eq!(limited_observations.len(), 1);
    assert_eq!(limited_observations[0]["host"], "github.com");
    assert_eq!(limited_observations[0]["remaining"], 0);
    assert_eq!(limited_observations[0]["state"], "stale");
    assert_eq!(limited_observations[0]["best_effort"], true);

    let human_backoff = fixture.qgh(["sync"]);
    assert_eq!(human_backoff.status.code(), Some(5));
    assert!(stdout_text(&human_backoff).is_empty());
    let human_backoff_stderr = stderr_text(&human_backoff);
    assert!(human_backoff_stderr.contains("sync.backoff"));
    assert!(human_backoff_stderr.contains("retryable"));
    assert!(human_backoff_stderr.contains("rate budget:"));
    assert!(human_backoff_stderr.contains("best-effort"));
    assert!(human_backoff_stderr.contains("Retry now: qgh sync --all --profile work."));
    assert!(
        human_backoff_stderr.contains("Existing local query, get, and status remain available.")
    );

    let mut colored_backoff_command = fixture.base_command();
    let colored_backoff = colored_backoff_command
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .args(["--profile", "work", "sync"])
        .output()
        .unwrap();
    assert_eq!(colored_backoff.status.code(), Some(5));
    let colored_stderr = stderr_text(&colored_backoff);
    assert!(colored_stderr.contains("\u{1b}[1;33m"));
    assert!(colored_stderr.contains("! retryable sync.backoff"));
    assert!(!colored_stderr.contains("\u{1b}[1;31m"));

    let mut quiet_backoff_command = fixture.base_command();
    let quiet_backoff = quiet_backoff_command
        .env_remove("NO_COLOR")
        .env("CLICOLOR_FORCE", "1")
        .env("TERM", "xterm-256color")
        .env("LANG", "en_US.UTF-8")
        .args(["--profile", "work", "sync", "--quiet"])
        .output()
        .unwrap();
    assert_eq!(quiet_backoff.status.code(), Some(5));
    let quiet_backoff_stderr = stderr_text(&quiet_backoff);
    assert!(quiet_backoff_stderr.contains("retryable sync.backoff"));
    assert!(!quiet_backoff_stderr.contains('\u{1b}'));
    assert!(!quiet_backoff_stderr.contains('!'));

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
    let mut status_observations = status_json["data"]["sync"]["rate_budget"]["observations"]
        .as_array()
        .unwrap()
        .clone();
    let mut expected_observations = limited_observations.clone();
    assert!(status_observations[0]["observed_at"].is_string());
    for observation in &mut status_observations {
        observation.as_object_mut().unwrap().remove("observed_at");
    }
    for observation in &mut expected_observations {
        observation.as_object_mut().unwrap().remove("observed_at");
    }
    assert_eq!(status_observations, expected_observations);
    assert_eq!(
        status_json["data"]["sync"]["backoff"]["scope"],
        "issues:owner/repo"
    );
    assert_eq!(
        status_json["data"]["sync"]["backoff"]["retry_command"],
        "qgh sync --all --profile work --quiet"
    );
    assert_eq!(
        status_json["data"]["sync"]["backoff"]["retry_action"],
        json!({
            "reason": "sync_backoff",
            "command": "qgh sync --all --profile work --quiet",
            "json_command": "qgh sync --all --profile work --quiet --json"
        })
    );
    assert!(status_json["data"]["sync"]["last_sync_at"]
        .as_str()
        .is_some());

    let human_status = fixture.qgh(["status"]);
    assert_success(&human_status);
    let human_status_stdout = stdout_text(&human_status);
    assert!(human_status_stdout.contains("reset_at="));
    assert!(human_status_stdout.contains("next: retry now: qgh sync --all --profile work --quiet"));
}

#[test]
fn successful_sync_persists_rate_budget_observation_for_local_status() {
    let fixture = TestFixture::new("rate-budget-success");
    let server = RateLimitFakeGitHub::start();
    fixture.write_config(&server.base_url);

    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    let observations = sync_rate_budget_observations(&sync_json);
    assert_eq!(observations.len(), 1);
    let observation = &observations[0];
    assert_eq!(observation["host"], "github.com");
    assert_eq!(observation["resource"], "core");
    assert_eq!(observation["limit"], 5000);
    assert_eq!(observation["remaining"], 4999);
    assert!(observation["reset_at"].as_str().is_some());
    assert!(observation["observed_at"].as_str().is_some());
    assert_eq!(observation["best_effort"], true);
    assert_eq!(observation["stale"], false);

    let requests_after_sync = server.request_count();
    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    assert_eq!(server.request_count(), requests_after_sync);
    let status_json = stdout_json(&status);
    assert_eq!(
        status_sync_rate_budget_observations(&status_json),
        observations
    );

    let human_status = fixture.qgh(["status"]);
    assert_success(&human_status);
    assert_eq!(server.request_count(), requests_after_sync);
    let human = stdout_text(&human_status);
    assert!(human.contains("rate budget: core remaining 4999/5000"));
    assert!(human.contains("state=fresh best-effort"));
}

#[test]
fn response_without_rate_headers_replaces_old_budget_with_partial_observation() {
    let fixture = TestFixture::new("rate-budget-missing-headers");
    let server = RateLimitFakeGitHub::start();
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    server.set_mode(RATE_LIMIT_MISSING_HEADERS);
    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    let observations = sync_rate_budget_observations(&sync_json);
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0]["resource"], Value::Null);
    assert_eq!(observations[0]["limit"], Value::Null);
    assert_eq!(observations[0]["remaining"], Value::Null);
    assert_eq!(observations[0]["reset_at"], Value::Null);
    assert_eq!(observations[0]["state"], "partial");
}

#[test]
fn schedule_run_skips_explicit_never_synced_profiles_without_network() {
    let fixture = TestFixture::new("schedule-never-synced");
    let server = RateLimitFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles(&server.base_url);

    let scheduled = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);
    assert_success(&scheduled);
    assert!(stderr_text(&scheduled).is_empty());
    assert_eq!(server.request_count(), 0);
    let json = stdout_json(&scheduled);
    assert_eq!(json["data"]["operation"], "run");
    assert_eq!(json["data"]["pass_state"], "completed");
    assert_eq!(json["data"]["policy"]["explicit_profiles"], true);
    assert_eq!(json["data"]["policy"]["host_max_in_flight"], 1);
    assert_eq!(json["data"]["policy"]["unknown_budget_max_attempts"], 1);
    assert_eq!(json["data"]["policy"]["reserve_percent"], 20);
    let profiles = json["data"]["profiles"].as_array().unwrap();
    assert_eq!(profiles.len(), 2);
    assert!(profiles.iter().all(|profile| profile["planned"] == true));
    assert!(profiles.iter().all(|profile| profile["started"] == false));
    assert!(profiles
        .iter()
        .all(|profile| profile["outcome"] == "skipped"));
    assert!(profiles
        .iter()
        .all(|profile| profile["reason"] == "bootstrap_required"));
    assert_eq!(
        profiles[0]["next_action"]["json_command"],
        "qgh sync --all --profile work --json"
    );
    assert_eq!(json["data"]["summary"]["requested"], 2);
    assert_eq!(json["data"]["summary"]["started"], 0);
    assert_eq!(json["data"]["summary"]["skipped"], 2);

    let human = fixture.qgh_without_profile(["schedule", "run", "work", "alt"]);
    assert_success(&human);
    let human = stdout_text(&human);
    assert!(human.contains(
        "work: skipped (bootstrap_required), planned true, started false, budget unknown (best-effort; no observation)"
    ));
    assert!(human.contains("next: qgh sync --all --profile work"));
}

#[test]
fn schedule_run_skips_fresh_profiles_without_network() {
    let fixture = TestFixture::new("schedule-fresh");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles(&server.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    server.clear_requests();

    let scheduled = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);
    assert_success(&scheduled);
    assert!(server.requests().is_empty());
    let json = stdout_json(&scheduled);
    let profiles = json["data"]["profiles"].as_array().unwrap();
    assert!(profiles
        .iter()
        .all(|profile| profile["outcome"] == "skipped"));
    assert!(profiles.iter().all(|profile| profile["reason"] == "fresh"));
    assert_eq!(json["data"]["summary"]["started"], 0);
}

#[test]
fn schedule_run_requires_unique_explicit_profiles_and_rejects_global_profile() {
    let fixture = TestFixture::new("schedule-explicit-profile-boundary");
    let server = RateLimitFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles(&server.base_url);

    let duplicate = fixture.qgh_without_profile(["schedule", "run", "work", "work", "--json"]);
    assert_eq!(duplicate.status.code(), Some(2));
    assert_eq!(
        stdout_json(&duplicate)["error"]["code"],
        "validation.duplicate_profile"
    );

    let global_profile =
        fixture.qgh_in_profile(&fixture.root, "work", ["schedule", "run", "work", "--json"]);
    assert_eq!(global_profile.status.code(), Some(2));
    assert_eq!(
        stdout_json(&global_profile)["error"]["code"],
        "validation.schedule_profile_boundary"
    );
    assert_eq!(server.request_count(), 0);
}

#[test]
fn schedule_fresh_budget_at_reserve_defers_all_profiles_without_network() {
    let fixture = TestFixture::new("schedule-rate-budget-reserve");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles(&server.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    fixture.set_profile_rate_budget("work", 5_000, 1_000);
    fixture.set_profile_rate_budget("alt", 5_000, 1_000);
    server.clear_requests();

    let scheduled = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);
    assert_success(&scheduled);
    assert!(server.requests().is_empty());
    let json = stdout_json(&scheduled);
    assert_eq!(json["data"]["summary"]["started"], 0);
    assert!(json["data"]["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .all(|profile| profile["reason"] == "rate_budget_reserve"));
}

#[test]
fn schedule_shares_active_backoff_even_when_source_profile_is_fresh() {
    let fixture = TestFixture::new("schedule-shared-host-backoff");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles(&server.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    fixture.mark_profile_sync_stale("alt");
    fixture.set_profile_backoff("work");
    server.clear_requests();

    let scheduled = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);
    assert_success(&scheduled);
    assert!(server.requests().is_empty());
    let json = stdout_json(&scheduled);
    assert_eq!(json["data"]["summary"]["started"], 0);
    assert_eq!(json["data"]["profiles"][0]["reason"], "fresh");
    assert_eq!(json["data"]["profiles"][1]["reason"], "host_cooldown");

    server.clear_requests();
    let subset = fixture.qgh_without_profile(["schedule", "run", "alt", "--json"]);
    assert_success(&subset);
    let subset_json = stdout_json(&subset);
    assert_eq!(subset_json["data"]["summary"]["started"], 0);
    assert_eq!(
        subset_json["data"]["profiles"][0]["reason"],
        "host_cooldown"
    );
    assert!(server.requests().is_empty());
}

#[test]
fn schedule_promotes_a_global_guard_when_all_selected_profiles_are_in_backoff() {
    let fixture = TestFixture::new("schedule-all-selected-backoff-promotion");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles_stale(&server.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    fixture.set_profile_backoff("work");
    fixture.set_profile_backoff("alt");
    server.clear_requests();

    let first = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);
    assert_success(&first);
    let first_json = stdout_json(&first);
    assert_eq!(first_json["data"]["summary"]["started"], 0);
    assert!(first_json["data"]["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .all(|profile| profile["reason"] == "host_cooldown"));
    assert!(server.requests().is_empty());

    fixture.clear_profile_backoff("alt");
    let subset = fixture.qgh_without_profile(["schedule", "run", "alt", "--json"]);
    assert_success(&subset);
    let subset_json = stdout_json(&subset);
    assert_eq!(subset_json["data"]["summary"]["started"], 0);
    assert_eq!(
        subset_json["data"]["profiles"][0]["reason"],
        "host_cooldown"
    );
    assert!(server.requests().is_empty());
}

#[test]
fn schedule_secondary_limit_stops_remaining_profiles_on_the_same_host() {
    let fixture = TestFixture::new("schedule-runtime-secondary-limit");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles_stale(&server.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    fixture.clear_profile_rate_budget("work");
    fixture.clear_profile_rate_budget("alt");
    server.set_mode(MULTI_REPO_OWNER_SECONDARY_RATE_LIMIT);
    server.clear_requests();

    let scheduled = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);
    assert_success(&scheduled);
    let json = stdout_json(&scheduled);
    assert_eq!(json["data"]["summary"]["started"], 1);
    assert_eq!(json["data"]["profiles"][0]["outcome"], "deferred");
    assert_eq!(json["data"]["profiles"][0]["reason"], "host_cooldown");
    assert_eq!(json["data"]["profiles"][0]["error_code"], "sync.backoff");
    assert_eq!(json["data"]["profiles"][1]["outcome"], "deferred");
    assert_eq!(json["data"]["profiles"][1]["reason"], "host_cooldown");
    let requests = server.requests();
    assert!(requests
        .iter()
        .any(|request| request.contains("/repos/owner/repo/issues?")));
    assert!(!requests
        .iter()
        .any(|request| request.contains("/repos/other/repo/issues?")));

    let guard_path = fs::read_dir(fixture.data_home.join("qgh/schedule/hosts"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.ends_with(".guard.json"))
        })
        .unwrap();
    let guard: Value = serde_json::from_slice(&fs::read(guard_path).unwrap()).unwrap();
    let guarded_until =
        DateTime::parse_from_rfc3339(guard["guarded_until"].as_str().expect("guard deadline"))
            .unwrap()
            .with_timezone(&Utc);
    assert!(guarded_until > Utc::now() + Duration::minutes(50));

    let request_count = server.requests().len();
    let subset = fixture.qgh_without_profile(["schedule", "run", "alt", "--json"]);
    assert_success(&subset);
    let subset_json = stdout_json(&subset);
    assert_eq!(subset_json["data"]["summary"]["started"], 0);
    assert_eq!(
        subset_json["data"]["profiles"][0]["reason"],
        "host_cooldown"
    );
    assert_eq!(server.requests().len(), request_count);
}

#[test]
fn schedule_rechecks_budget_after_each_profile_and_stops_when_headers_disappear() {
    let fixture = TestFixture::new("schedule-rate-budget-drift");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles_stale(&server.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    fixture.set_profile_rate_budget("work", 5_000, 4_000);
    fixture.set_profile_rate_budget("alt", 5_000, 4_000);
    server.set_mode(MULTI_REPO_MISSING_RATE_HEADERS);
    server.clear_requests();

    let scheduled = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);
    assert_success(&scheduled);
    let json = stdout_json(&scheduled);
    assert_eq!(json["data"]["summary"]["started"], 1);
    assert_eq!(json["data"]["profiles"][0]["outcome"], "deferred");
    assert_eq!(
        json["data"]["profiles"][0]["reason"],
        "unknown_budget_limit"
    );
    assert_eq!(json["data"]["profiles"][1]["outcome"], "deferred");
    assert_eq!(json["data"]["profiles"][1]["reason"], "host_cooldown");
    assert_eq!(
        json["data"]["profiles"][1]["budget_snapshot"]["observations"][0]["state"],
        "partial"
    );
    assert!(server
        .requests()
        .iter()
        .all(|request| !request.contains("/repos/other/repo")));

    server.clear_requests();
    let second = fixture.qgh_without_profile(["schedule", "run", "alt", "--json"]);
    assert_success(&second);
    let second_json = stdout_json(&second);
    assert_eq!(second_json["data"]["summary"]["started"], 0);
    assert_eq!(second_json["data"]["summary"]["deferred"], 1);
    assert_eq!(
        second_json["data"]["profiles"][0]["reason"],
        "host_cooldown"
    );
    assert!(server.requests().is_empty());
}

#[test]
fn scheduled_request_gate_stops_at_the_observed_core_reserve_before_any_follow_up() {
    let fixture = TestFixture::new("schedule-per-request-reserve-gate");
    let server = ScheduledBudgetGateFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles_stale(&server.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    fixture.set_profile_rate_budget("work", 10, 3);
    fixture.set_profile_rate_budget("alt", 10, 3);
    server.set_mode(SCHEDULED_BUDGET_KNOWN_RESERVE);
    server.clear_requests();

    let scheduled = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);

    assert_success(&scheduled);
    let json = stdout_json(&scheduled);
    assert_eq!(json["data"]["summary"]["started"], 1);
    assert_eq!(json["data"]["summary"]["completed"], 0);
    assert_eq!(json["data"]["summary"]["deferred"], 2);
    assert_eq!(json["data"]["profiles"][0]["reason"], "rate_budget_reserve");
    assert_eq!(json["data"]["profiles"][1]["reason"], "host_cooldown");
    assert_eq!(
        json["data"]["profiles"][0]["budget_snapshot"]["observations"][0]["remaining"],
        2
    );
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        1,
        "unexpected follow-up request: {requests:?}"
    );
    assert!(requests[0].contains("/repos/owner/repo/issues?"));
}

#[test]
fn scheduled_mixed_budget_allows_one_probe_then_blocks_when_headers_remain_partial() {
    let fixture = TestFixture::new("schedule-per-request-unknown-gate");
    let server = ScheduledBudgetGateFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles_stale(&server.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    fixture.set_profile_unknown_resource_rate_budget("work", 5_000, 4_000);
    fixture.set_profile_rate_budget("alt", 5_000, 4_000);
    server.set_mode(SCHEDULED_BUDGET_UNKNOWN);
    server.clear_requests();

    let scheduled = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);

    assert_success(&scheduled);
    let json = stdout_json(&scheduled);
    assert_eq!(json["data"]["summary"]["started"], 1);
    assert_eq!(json["data"]["summary"]["completed"], 0);
    assert_eq!(json["data"]["summary"]["deferred"], 2);
    assert_eq!(
        json["data"]["profiles"][0]["reason"],
        "unknown_budget_limit"
    );
    assert_eq!(json["data"]["profiles"][1]["reason"], "host_cooldown");
    assert_eq!(
        json["data"]["profiles"][0]["budget_snapshot"]["observations"][0]["state"],
        "partial"
    );
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        1,
        "unexpected follow-up request: {requests:?}"
    );
    assert!(requests[0].contains("/repos/owner/repo/issues?"));
}

#[test]
fn scheduled_transport_without_headers_consumes_the_last_known_request_for_the_pass() {
    let fixture = TestFixture::new("schedule-per-request-transport-gate");
    let server = ScheduledBudgetGateFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles_stale(&server.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    fixture.set_profile_rate_budget("work", 10, 3);
    fixture.set_profile_rate_budget("alt", 10, 3);
    server.set_mode(SCHEDULED_BUDGET_TRANSPORT_DROP);
    server.clear_requests();

    let scheduled = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);

    assert_success(&scheduled);
    let json = stdout_json(&scheduled);
    assert_eq!(json["data"]["summary"]["started"], 1);
    assert_eq!(json["data"]["summary"]["failed"], 1);
    assert_eq!(json["data"]["summary"]["deferred"], 1);
    assert_eq!(json["data"]["profiles"][0]["outcome"], "failed");
    assert_eq!(json["data"]["profiles"][0]["started"], true);
    assert_eq!(json["data"]["profiles"][1]["outcome"], "deferred");
    assert_eq!(json["data"]["profiles"][1]["reason"], "host_cooldown");
    assert_eq!(json["data"]["profiles"][1]["started"], false);
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        1,
        "unexpected retry after EOF: {requests:?}"
    );

    server.clear_requests();
    let second_pass = fixture.qgh_without_profile(["schedule", "run", "alt", "--json"]);
    assert_success(&second_pass);
    let second_json = stdout_json(&second_pass);
    assert_eq!(second_json["data"]["summary"]["started"], 0);
    assert_eq!(second_json["data"]["summary"]["deferred"], 1);
    assert_eq!(
        second_json["data"]["profiles"][0]["reason"],
        "host_cooldown"
    );
    assert!(server.requests().is_empty());
}

#[test]
fn scheduled_final_missing_headers_keep_the_host_guard_after_a_completed_sync() {
    let fixture = TestFixture::new("schedule-final-missing-headers-guard");
    let server = ScheduledBudgetGateFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles_stale(&server.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    fixture.set_profile_rate_budget("work", 10, 8);
    fixture.set_profile_rate_budget("alt", 10, 8);
    server.set_mode(SCHEDULED_BUDGET_FINAL_MISSING_HEADERS);
    server.clear_requests();

    let first = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);
    assert_success(&first);
    let first_json = stdout_json(&first);
    assert_eq!(first_json["data"]["summary"]["started"], 1);
    assert_eq!(first_json["data"]["summary"]["completed"], 1);
    assert_eq!(first_json["data"]["summary"]["deferred"], 1);
    assert_eq!(first_json["data"]["profiles"][0]["reason"], "synced");
    assert_eq!(first_json["data"]["profiles"][1]["reason"], "host_cooldown");
    assert_eq!(server.requests().len(), 2);

    let second = fixture.qgh_without_profile(["schedule", "run", "alt", "--json"]);
    assert_success(&second);
    let second_json = stdout_json(&second);
    assert_eq!(second_json["data"]["summary"]["started"], 0);
    assert_eq!(second_json["data"]["summary"]["deferred"], 1);
    assert_eq!(
        second_json["data"]["profiles"][0]["reason"],
        "host_cooldown"
    );
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn managed_schedule_run_revalidates_github_cli_credentials_before_network() {
    let fixture = TestFixture::new("schedule-managed-credential-revalidation");
    let server = RateLimitFakeGitHub::start();
    fixture.write_config(&server.base_url);

    let scheduled =
        fixture.qgh_without_profile(["schedule", "run", "--manager-invoked", "work", "--json"]);

    assert_eq!(scheduled.status.code(), Some(2));
    assert_eq!(
        stdout_json(&scheduled)["error"]["code"],
        "schedule.credentials_unsupported"
    );
    assert_eq!(server.request_count(), 0);
}

#[test]
fn schedule_executes_the_profile_snapshot_planned_before_a_config_change() {
    let fixture = TestFixture::new("schedule-planned-profile-snapshot");
    let bootstrap = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_two_hosts_same_repo(&bootstrap.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    drop(bootstrap);

    let server = SlowFirstRequestFakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_two_hosts_same_repo(&server.base_url);
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");

    let mut command = fixture.base_command();
    command
        .args(["schedule", "run", "work", "alt", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let scheduled = command.spawn().unwrap();
    server.wait_until_first_request();

    fixture.replace_profile_token_env("alt", "QGH_MISSING_AFTER_PLAN");
    server.release_first_request();
    let scheduled = scheduled.wait_with_output().unwrap();
    assert_success(&scheduled);
    let json = stdout_json(&scheduled);
    assert_eq!(json["data"]["summary"]["completed"], 2);
    assert_eq!(json["data"]["summary"]["failed"], 0);
}

#[test]
fn schedule_rechecks_host_cooldown_after_waiting_for_another_profile() {
    let fixture = TestFixture::new("schedule-latest-host-cooldown");
    let bootstrap = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_work_and_alt_same_repo(&bootstrap.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    drop(bootstrap);

    let server = SlowFirstRequestFakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_work_and_alt_same_repo(&server.base_url);
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    fixture.set_profile_rate_budget("work", 5_000, 4_000);
    fixture.set_profile_rate_budget("alt", 5_000, 4_000);

    let mut command = fixture.base_command();
    command
        .args(["schedule", "run", "work", "alt", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let scheduled = command.spawn().unwrap();
    server.wait_until_first_request();

    fixture.set_profile_backoff("alt");
    server.release_first_request();
    let scheduled = scheduled.wait_with_output().unwrap();
    assert_success(&scheduled);
    let json = stdout_json(&scheduled);
    assert_eq!(json["data"]["profiles"][0]["outcome"], "completed");
    assert_eq!(json["data"]["profiles"][1]["outcome"], "deferred");
    assert_eq!(json["data"]["profiles"][1]["reason"], "host_cooldown");
    assert_eq!(json["data"]["profiles"][1]["started"], false);
}

#[test]
fn schedule_rotates_host_start_order_after_the_global_cap() {
    let fixture = TestFixture::new("schedule-host-order-rotation");
    let server = FakeGitHub::start(issue_payload_with_pr());
    let profile_ids = fixture.write_config_with_nine_hosts(&server.base_url);
    for profile_id in &profile_ids {
        assert_success(&fixture.qgh_in_profile(
            &fixture.root,
            profile_id,
            ["sync", "--all", "--json"],
        ));
        fixture.mark_profile_sync_stale(profile_id);
    }

    let mut first_args = vec!["schedule", "run"];
    first_args.extend(profile_ids.iter().map(String::as_str));
    first_args.push("--json");
    let mut first_command = fixture.base_command();
    let first = first_command.args(&first_args).output().unwrap();
    assert_success(&first);
    let first_json = stdout_json(&first);
    assert_eq!(first_json["data"]["summary"]["started"], 8);
    assert_eq!(first_json["data"]["profiles"][8]["reason"], "pass_limit");

    for profile_id in &profile_ids {
        fixture.mark_profile_sync_stale(profile_id);
    }
    let mut second_command = fixture.base_command();
    let second = second_command.args(&first_args).output().unwrap();
    assert_success(&second);
    let second_json = stdout_json(&second);
    assert_eq!(second_json["data"]["summary"]["started"], 8);
    assert_eq!(second_json["data"]["profiles"][8]["outcome"], "completed");
    assert_eq!(second_json["data"]["profiles"][7]["reason"], "pass_limit");
}

#[test]
fn schedule_auth_failure_does_not_consume_unknown_host_attempt() {
    let fixture = TestFixture::new("schedule-auth-partial-failure");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles_stale(&server.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    let config_path = fixture.config_home.join("qgh/config.toml");
    let config = fs::read_to_string(&config_path).unwrap().replacen(
        r#"env = "QGH_TEST_TOKEN""#,
        r#"env = "QGH_MISSING_SCHEDULE_TOKEN""#,
        1,
    );
    fs::write(config_path, config).unwrap();
    server.clear_requests();

    let mut command = fixture.base_command();
    let scheduled = command
        .env_remove("QGH_MISSING_SCHEDULE_TOKEN")
        .args(["schedule", "run", "work", "alt", "--json"])
        .output()
        .unwrap();
    assert_success(&scheduled);
    let json = stdout_json(&scheduled);
    assert_eq!(json["data"]["pass_state"], "completed_with_failures");
    assert_eq!(json["data"]["summary"]["started"], 1);
    assert_eq!(json["data"]["summary"]["failed"], 1);
    assert_eq!(json["data"]["summary"]["completed"], 1);
    assert_eq!(
        json["data"]["profiles"][0]["error_code"],
        "auth.token_unavailable"
    );
    assert_eq!(json["data"]["profiles"][0]["started"], false);
    assert_eq!(json["data"]["profiles"][1]["outcome"], "completed");
    let requests = server.requests();
    assert!(requests
        .iter()
        .any(|request| request.contains("/repos/other/repo/issues?")));
    assert!(!requests
        .iter()
        .any(|request| request.contains("/repos/owner/repo/issues?")));
}

#[cfg(unix)]
#[test]
fn schedule_pre_network_purge_failure_does_not_consume_unknown_host_attempt() {
    use std::os::unix::fs::symlink;

    let fixture = TestFixture::new("schedule-purge-preflight-attempt-accounting");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_work_two_repos_and_alt(&server.base_url, true);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));

    fixture.write_config_with_work_two_repos_and_alt(&server.base_url, false);
    fixture.mark_profile_sync_stale("alt");
    let work_profile = fixture.data_home.join("qgh/profiles/work");
    let index_root = work_profile.join("tantivy");
    fs::rename(&index_root, work_profile.join("tantivy-before-purge")).unwrap();
    let user_backup = fixture.root.join("user-index-backup");
    fs::create_dir_all(user_backup.join("generation-999")).unwrap();
    symlink(&user_backup, &index_root).unwrap();

    let scheduled = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);
    assert_success(&scheduled);
    let json = stdout_json(&scheduled);
    assert_eq!(json["data"]["pass_state"], "completed_with_failures");
    assert_eq!(json["data"]["summary"]["failed"], 1);
    assert_eq!(json["data"]["summary"]["completed"], 1);
    assert_eq!(json["data"]["profiles"][0]["started"], false);
    assert_eq!(
        json["data"]["profiles"][0]["error_code"],
        "purge.retry_failed"
    );
    assert_eq!(json["data"]["profiles"][1]["started"], true);
    assert_eq!(json["data"]["profiles"][1]["outcome"], "completed");
}

#[test]
fn schedule_start_rejects_env_credentials_before_manager_or_network_access() {
    let fixture = TestFixture::new("schedule-start-env-credentials");
    let server = RateLimitFakeGitHub::start();
    fixture.write_config(&server.base_url);
    let test_home = fixture.root.join("home");
    fs::create_dir_all(&test_home).unwrap();

    let mut command = fixture.base_command();
    let started = command
        .env("HOME", test_home)
        .args(["schedule", "start", "work", "--json"])
        .output()
        .unwrap();
    assert_eq!(started.status.code(), Some(2));
    assert_eq!(
        stdout_json(&started)["error"]["code"],
        "schedule.credentials_unsupported"
    );
    assert_eq!(server.request_count(), 0);
}

#[test]
fn schedule_status_and_absent_stop_are_local_only_and_idempotent() {
    let fixture = TestFixture::new("schedule-lifecycle-absent");
    let test_home = fixture.root.join("home");
    fs::create_dir_all(&test_home).unwrap();

    let mut status_command = fixture.base_command();
    let status = status_command
        .env("HOME", &test_home)
        .args(["schedule", "status", "--json"])
        .output()
        .unwrap();
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["operation"], "status");
    assert_eq!(status_json["data"]["schedule_state"], "not_installed");
    assert_eq!(status_json["data"]["installed"], false);
    assert_eq!(status_json["data"]["manager_checked"], false);
    assert_eq!(status_json["data"]["network_access"], false);
    assert_eq!(status_json["data"]["artifact_state"], "missing");

    let mut stop_command = fixture.base_command();
    let stopped = stop_command
        .env("HOME", &test_home)
        .args(["schedule", "stop", "--json"])
        .output()
        .unwrap();
    assert_success(&stopped);
    let stopped_json = stdout_json(&stopped);
    assert_eq!(stopped_json["data"]["operation"], "stop");
    assert_eq!(stopped_json["data"]["action"], "unchanged");
    assert_eq!(stopped_json["data"]["schedule_state"], "not_installed");
    assert_eq!(stopped_json["data"]["installed"], false);
    assert_eq!(stopped_json["data"]["network_access"], false);

    let mut human_command = fixture.base_command();
    let human = human_command
        .env("HOME", test_home)
        .args(["schedule", "status"])
        .output()
        .unwrap();
    assert_success(&human);
    let human = stdout_text(&human);
    assert!(human.contains("state: not_installed"));
    assert!(human.contains("profiles:"));
    assert!(human.contains("interval: n/a"));
    assert!(human.contains("artifacts: missing"));
    assert!(human.contains("manager checked: false"));
}

#[test]
fn schedule_unknown_budget_guard_blocks_next_explicit_subset_until_fallback_expires() {
    let fixture = TestFixture::new("schedule-unknown-budget-rotation");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles_stale(&server.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    fixture.clear_profile_rate_budget("work");
    fixture.clear_profile_rate_budget("alt");
    server.set_mode(MULTI_REPO_MISSING_RATE_HEADERS);
    server.clear_requests();

    let first = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);
    assert_success(&first);
    let first_json = stdout_json(&first);
    assert_eq!(first_json["data"]["summary"]["started"], 1);
    assert_eq!(first_json["data"]["profiles"][0]["outcome"], "deferred");
    assert_eq!(
        first_json["data"]["profiles"][0]["reason"],
        "unknown_budget_limit"
    );
    assert_eq!(first_json["data"]["profiles"][1]["outcome"], "deferred");
    assert_eq!(first_json["data"]["profiles"][1]["reason"], "host_cooldown");
    let first_requests = server.requests();
    assert!(first_requests
        .iter()
        .any(|request| request.contains("/repos/owner/repo/issues?")));
    assert!(!first_requests
        .iter()
        .any(|request| request.contains("/repos/other/repo/issues?")));
    let host_state_dir = fixture.data_home.join("qgh/schedule/hosts");
    let host_state_path = fs::read_dir(&host_state_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.ends_with(".json") && !name.ends_with(".guard.json"))
        })
        .unwrap();
    let host_state = fs::read_to_string(&host_state_path).unwrap();
    assert!(host_state.contains("cursor_profile_id"));
    let root_marker = fixture.root.to_string_lossy().into_owned();
    for forbidden in [
        "fixture-token",
        "owner/repo",
        "other/repo",
        root_marker.as_str(),
    ] {
        assert!(!host_state.contains(forbidden));
    }
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(host_state_path).unwrap().permissions().mode() & 0o077,
        0
    );
    let guard_path = fs::read_dir(&host_state_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.ends_with(".guard.json"))
        })
        .unwrap();
    let guard_state = fs::read_to_string(&guard_path).unwrap();
    assert!(guard_state.contains("qgh.schedule-budget-guard.v1"));
    for forbidden in [
        "fixture-token",
        "owner/repo",
        "other/repo",
        root_marker.as_str(),
    ] {
        assert!(!guard_state.contains(forbidden));
    }
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(guard_path).unwrap().permissions().mode() & 0o077,
        0
    );

    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    fixture.clear_profile_rate_budget("work");
    fixture.clear_profile_rate_budget("alt");
    server.clear_requests();
    let second = fixture.qgh_without_profile(["schedule", "run", "alt", "--json"]);
    assert_success(&second);
    let second_json = stdout_json(&second);
    assert_eq!(second_json["data"]["summary"]["started"], 0);
    assert_eq!(second_json["data"]["summary"]["deferred"], 1);
    assert_eq!(second_json["data"]["profiles"][0]["outcome"], "deferred");
    assert_eq!(
        second_json["data"]["profiles"][0]["reason"],
        "host_cooldown"
    );
    assert!(server.requests().is_empty());
}

#[test]
fn schedule_unknown_at_pass_start_attempts_only_one_profile_after_a_successful_probe() {
    let fixture = TestFixture::new("schedule-unknown-budget-sticky-pass-limit");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles_stale(&server.base_url);
    assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
    assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    fixture.clear_profile_rate_budget("work");
    fixture.clear_profile_rate_budget("alt");
    server.clear_requests();

    let first = fixture.qgh_without_profile(["schedule", "run", "work", "alt", "--json"]);
    assert_success(&first);
    let first_json = stdout_json(&first);
    assert_eq!(first_json["data"]["summary"]["started"], 1);
    assert_eq!(first_json["data"]["summary"]["completed"], 1);
    assert_eq!(first_json["data"]["summary"]["deferred"], 1);
    assert_eq!(first_json["data"]["profiles"][0]["reason"], "synced");
    assert_eq!(
        first_json["data"]["profiles"][1]["reason"],
        "unknown_budget_limit"
    );
    let first_requests = server.requests();
    assert!(first_requests
        .iter()
        .any(|request| request.contains("/repos/owner/repo/issues?")));
    assert!(!first_requests
        .iter()
        .any(|request| request.contains("/repos/other/repo/issues?")));

    server.clear_requests();
    let second = fixture.qgh_without_profile(["schedule", "run", "alt", "--json"]);
    assert_success(&second);
    let second_json = stdout_json(&second);
    assert_eq!(second_json["data"]["summary"]["started"], 1);
    assert_eq!(second_json["data"]["summary"]["completed"], 1);
    assert!(server
        .requests()
        .iter()
        .any(|request| request.contains("/repos/other/repo/issues?")));
}

#[test]
fn scheduled_write_ahead_budget_guard_survives_process_termination_and_blocks_explicit_subset() {
    let fixture = TestFixture::new("schedule-write-ahead-budget-guard-process-termination");
    {
        let bootstrap_server = MultiRepoFakeGitHub::start();
        fixture.write_config_with_work_and_alt_profiles_stale(&bootstrap_server.base_url);
        assert_success(&fixture.qgh_in_profile(&fixture.root, "work", ["sync", "--all", "--json"]));
        assert_success(&fixture.qgh_in_profile(&fixture.root, "alt", ["sync", "--all", "--json"]));
    }

    let server = SlowFirstRequestFakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_work_and_alt_profiles_stale(&server.base_url);
    fixture.mark_profile_sync_stale("work");
    fixture.mark_profile_sync_stale("alt");
    fixture.set_profile_rate_budget("work", 10, 3);
    fixture.set_profile_rate_budget("alt", 10, 3);

    let mut interrupted_command = fixture.base_command();
    interrupted_command
        .args(["schedule", "run", "work", "alt", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut interrupted = interrupted_command.spawn().unwrap();
    server.wait_until_first_request();
    interrupted.kill().unwrap();
    let interrupted = interrupted.wait_with_output().unwrap();
    assert!(!interrupted.status.success());
    server.release_first_request();
    assert_eq!(server.accepted_request_count(), 1);

    let second = fixture.qgh_without_profile(["schedule", "run", "alt", "--json"]);
    assert_success(&second);
    let second_json = stdout_json(&second);
    assert_eq!(second_json["data"]["summary"]["started"], 0);
    assert_eq!(second_json["data"]["summary"]["deferred"], 1);
    assert_eq!(
        second_json["data"]["profiles"][0]["reason"],
        "host_cooldown"
    );
    assert_eq!(server.accepted_request_count(), 1);
}

#[test]
fn concurrent_schedule_passes_keep_one_host_owner_and_defer_the_loser() {
    let fixture = TestFixture::new("schedule-concurrent-host-owner");
    {
        let bootstrap_server = FakeGitHub::start(issue_payload_with_pr());
        fixture.write_config(&bootstrap_server.base_url);
        assert_success(&fixture.qgh(["sync", "--json"]));
    }
    let server = SlowFirstRequestFakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    fixture.mark_profile_sync_stale("work");

    let mut owner_command = fixture.base_command();
    owner_command
        .args(["schedule", "run", "work", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let owner = owner_command.spawn().unwrap();
    server.wait_until_first_request();

    let loser = fixture.qgh_without_profile(["schedule", "run", "work", "--json"]);
    assert_success(&loser);
    let loser_json = stdout_json(&loser);
    assert_eq!(loser_json["data"]["summary"]["started"], 0);
    assert_eq!(loser_json["data"]["profiles"][0]["outcome"], "deferred");
    assert_eq!(loser_json["data"]["profiles"][0]["reason"], "host_busy");
    assert_eq!(server.accepted_request_count(), 1);

    server.release_first_request();
    let owner = owner.wait_with_output().unwrap();
    assert_success(&owner);
}

#[test]
fn sync_records_secondary_rate_limit_retry_after_without_generic_failure() {
    let fixture = TestFixture::new("secondary-rate-limit");
    let server = RateLimitFakeGitHub::start();
    server.set_mode(RATE_LIMIT_SECONDARY);
    fixture.write_config(&server.base_url);

    let limited_sync = fixture.qgh(["sync", "--json"]);
    assert_eq!(limited_sync.status.code(), Some(5));
    assert!(stderr_text(&limited_sync).is_empty());
    let limited_json = stdout_json(&limited_sync);
    assert_eq!(limited_json["error"]["code"], "sync.backoff");
    assert_eq!(limited_json["error"]["retryable"], true);
    assert_eq!(
        limited_json["error"]["details"]["reason"],
        "secondary_rate_limit"
    );
    assert_eq!(
        limited_json["error"]["details"]["scope"],
        "issues:owner/repo"
    );
    assert_eq!(
        limited_json["error"]["details"]["last_successful_sync"],
        Value::Null
    );
    assert_eq!(
        limited_json["error"]["details"]["local_retrieval_available"],
        false
    );

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(
        status_json["data"]["sync"]["backoff"]["reason"],
        "secondary_rate_limit"
    );
    let observations = status_json["data"]["sync"]["rate_budget"]["observations"]
        .as_array()
        .unwrap();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0]["remaining"], 42);
    assert_eq!(observations[0]["resource"], Value::Null);
    assert_eq!(observations[0]["state"], "partial");
    assert_eq!(status_json["data"]["sources"]["issue_count"], 0);

    let human_status = fixture.qgh(["status"]);
    assert_success(&human_status);
    assert!(stdout_text(&human_status).contains("next: retry now: qgh sync --all --profile work"));
}

#[test]
fn huge_retry_after_remains_a_structured_backoff_without_panicking() {
    let fixture = TestFixture::new("huge-retry-after");
    let server = RateLimitFakeGitHub::start();
    server.set_mode(RATE_LIMIT_HUGE_RETRY_AFTER);
    fixture.write_config(&server.base_url);

    let limited = fixture.qgh(["sync", "--json"]);
    assert_eq!(limited.status.code(), Some(5));
    let limited_json = stdout_json(&limited);
    assert_eq!(limited_json["error"]["code"], "sync.backoff");
    assert_eq!(limited_json["error"]["retryable"], true);
    assert_eq!(
        limited_json["error"]["details"]["retry_after_seconds"],
        i64::MAX
    );
    assert_eq!(limited_json["error"]["details"]["retry_at"], Value::Null);

    let status = fixture.qgh(["status"]);
    assert_success(&status);
    assert!(stdout_text(&status)
        .contains("next: wait for GitHub backoff to clear, then qgh sync --all --profile work"));
}

#[test]
fn sync_resumes_from_last_committed_issue_page_after_mid_pagination_backoff() {
    let fixture = TestFixture::new("paginated-backoff-resume");
    let server = PaginatedBackoffFakeGitHub::start();
    fixture.write_config(&server.base_url);

    let limited_sync = fixture.qgh(["sync", "--json"]);
    assert_eq!(limited_sync.status.code(), Some(5));
    let limited_json = stdout_json(&limited_sync);
    assert_eq!(limited_json["error"]["code"], "sync.backoff");
    assert_eq!(limited_json["error"]["retryable"], true);
    assert_eq!(
        limited_json["error"]["details"]["reason"],
        "secondary_rate_limit"
    );
    assert_eq!(
        limited_json["error"]["details"]["last_successful_sync"],
        Value::Null
    );

    let partial_status = fixture.qgh(["status", "--json"]);
    assert_success(&partial_status);
    let partial_status_json = stdout_json(&partial_status);
    assert_eq!(partial_status_json["data"]["sources"]["issue_count"], 1);
    assert_eq!(partial_status_json["data"]["index"]["active_generation"], 0);
    assert_eq!(partial_status_json["data"]["index"]["dirty_task_count"], 1);
    assert_eq!(
        partial_status_json["data"]["sync"]["cursors"]["issues:owner/repo"]["watermark"],
        "2026-01-02T00:01:00Z"
    );
    assert_eq!(
        partial_status_json["data"]["sync"]["last_sync_at"],
        Value::Null
    );
    assert_eq!(
        partial_status_json["data"]["sync"]["backoff"]["last_successful_sync"],
        Value::Null
    );
    let partial_run_id =
        stdout_json(&fixture.qgh(["get", "qgh://github.com/issue/I_PAGE_ONE", "--json"]))["data"]
            ["source"]["source_version"]["sync_run_id"]
            .clone();

    server.set_mode(PAGINATED_RESUME);
    server.clear_requests();

    let resumed_sync = fixture.qgh(["sync", "--json"]);
    assert_success(&resumed_sync);
    let resumed_json = stdout_json(&resumed_sync);
    assert_eq!(resumed_json["data"]["sync_state"], "ok");
    let resumed_run_id = resumed_json["data"]["sync_run_id"].clone();
    assert_eq!(
        resumed_json["data"]["cursors"]["watermarks"]["issues:owner/repo"],
        "2026-01-02T00:02:00Z"
    );

    let requests = server.requests();
    assert!(
        requests.iter().any(|request| request.contains(
            "GET /repos/owner/repo/issues?state=all&sort=updated&direction=asc&per_page=100&since=2026-01-02T00%3A00%3A00Z"
        )),
        "resume must use the page-one cursor with the existing 60-second overlap: {requests:#?}"
    );

    let final_status = fixture.qgh(["status", "--json"]);
    assert_success(&final_status);
    let final_status_json = stdout_json(&final_status);
    assert_eq!(final_status_json["data"]["sources"]["issue_count"], 2);
    assert_eq!(final_status_json["data"]["index"]["active_generation"], 1);
    assert_eq!(final_status_json["data"]["index"]["dirty_task_count"], 0);
    assert_eq!(final_status_json["data"]["sync"]["backoff"], Value::Null);
    assert_eq!(
        final_status_json["data"]["sync"]["cursors"]["issues:owner/repo"]["watermark"],
        "2026-01-02T00:02:00Z"
    );
    assert!(final_status_json["data"]["sync"]["last_sync_at"]
        .as_str()
        .is_some());

    let duplicate_id = "qgh://github.com/issue/I_PAGE_ONE";
    let resumed_id = "qgh://github.com/issue/I_PAGE_TWO";
    fixture.assert_source_version_count(duplicate_id, 1);
    fixture.assert_source_version_count(resumed_id, 1);
    let duplicate_get = fixture.qgh(["get", duplicate_id, "--json"]);
    assert_success(&duplicate_get);
    let resumed_get = fixture.qgh(["get", resumed_id, "--json"]);
    assert_success(&resumed_get);
    assert_eq!(
        stdout_json(&duplicate_get)["data"]["source"]["source_version"]["sync_run_id"],
        partial_run_id
    );
    assert_eq!(
        stdout_json(&resumed_get)["data"]["source"]["source_version"]["sync_run_id"],
        resumed_run_id
    );

    let query = fixture.qgh(["query", "second durable page", "--json"]);
    assert_success(&query);
    assert_eq!(
        stdout_json(&query)["data"]["results"][0]["source_id"],
        resumed_id
    );
}

#[test]
fn first_backfill_backoff_does_not_publish_or_fabricate_embedding_snapshot() {
    let fixture = TestFixture::new("backfill-backoff-embedding-provenance");
    let server = PaginatedBackoffFakeGitHub::start();
    fixture.write_config_with_repos_and_embedding(
        &server.base_url,
        &["owner/repo"],
        r#"
[embedding]
provider = "local"
model_path = "/definitely/not/a/model"
file = "onnx/model.onnx"
pooling = "cls"
query_prefix = "query: "
quantization = "none"
"#,
    );

    let backfill = fixture.qgh(["sync", "--backfill", "--json"]);
    assert_eq!(backfill.status.code(), Some(5));
    let backfill_json = stdout_json(&backfill);
    assert_eq!(backfill_json["error"]["code"], "sync.backoff");
    assert_eq!(backfill_json["error"]["retryable"], true);
    assert_eq!(
        backfill_json["error"]["details"]["reason"],
        "secondary_rate_limit"
    );
    assert_eq!(
        backfill_json["error"]["details"]["retry_command"],
        "qgh sync --all --backfill --profile work --json"
    );
    assert_eq!(
        backfill_json["error"]["details"]["local_retrieval_available"],
        false
    );
    let persisted_backoff = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(
        persisted_backoff["data"]["sync"]["backoff"]["retry_command"],
        "qgh sync --all --backfill --profile work --json"
    );
    assert_eq!(
        fixture.embedding_sync_run_reference_count("embedding-sync"),
        0
    );
    assert_eq!(fixture.active_retrieval_publication_count(), 0);
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
fn doctor_reports_null_rate_limit_headers_when_token_is_unavailable() {
    let fixture = TestFixture::new("doctor-missing-token");
    fixture.write_config_with_missing_token_profile("http://127.0.0.1:1");

    let doctor = fixture.qgh_in_profile(&fixture.root, "strict", ["doctor", "--json"]);
    assert_success(&doctor);
    let doctor_json = stdout_json(&doctor);
    let checks = doctor_json["data"]["checks"].as_array().unwrap();
    let rate_limit = checks
        .iter()
        .find(|check| check["name"] == "rate_limit_headers")
        .unwrap();
    assert_eq!(rate_limit["ok"], false);
    assert_eq!(rate_limit["headers"]["x-ratelimit-remaining"], Value::Null);
    assert_eq!(rate_limit["headers"]["x-ratelimit-reset"], Value::Null);
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn doctor_verifies_prepared_artifacts_without_exposing_local_details() {
    let fixture = TestFixture::new("doctor-embedding-artifacts");
    let server = FakeGitHub::start(issue_payload_with_pr());
    let (manifest_path, manifest_hash) = fixture.write_prepared_embedding_manifest();
    fixture.write_config_with_embedding(
        &server.base_url,
        &format!(
            "provider = \"local\"\nmanifest_path = \"{}\"",
            manifest_path.display()
        ),
    );

    let embed = fixture.qgh(["embed", "--force", "--json"]);
    assert!(
        !embed.status.success(),
        "invalid ONNX fixture must not initialize"
    );

    let doctor = fixture.qgh(["doctor", "--json"]);
    assert_success(&doctor);
    let doctor_json = stdout_json(&doctor);
    let checks = doctor_json["data"]["checks"].as_array().unwrap();
    assert_eq!(doctor_check_ok(checks, "embedding_artifacts"), Some(true));
    assert_eq!(doctor_check_ok(checks, "embedding_runtime"), Some(false));
    assert_eq!(doctor_check_ok(checks, "embedding_generation"), Some(false));
    for check in checks.iter().filter(|check| {
        check["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("embedding_"))
    }) {
        assert_eq!(
            json_object_keys(check),
            BTreeSet::from(["name".to_string(), "ok".to_string()])
        );
    }

    fs::write(
        fixture.prepared_snapshot_artifact(&manifest_hash, "onnx/model.onnx"),
        b"corrupt!",
    )
    .unwrap();
    let corrupt = fixture.qgh(["doctor", "--json"]);
    assert_success(&corrupt);
    let corrupt_json = stdout_json(&corrupt);
    let corrupt_checks = corrupt_json["data"]["checks"].as_array().unwrap();
    assert_eq!(
        doctor_check_ok(corrupt_checks, "embedding_artifacts"),
        Some(false)
    );
    assert_eq!(
        doctor_check_ok(corrupt_checks, "embedding_runtime"),
        Some(false)
    );
    let stdout = String::from_utf8_lossy(&corrupt.stdout);
    assert!(!stdout.contains("corrupt!"));
    assert!(!stdout.contains(manifest_path.to_string_lossy().as_ref()));
}

#[test]
fn ghes_comment_parent_issue_ids_use_profile_host() {
    let fixture = TestFixture::new("ghes-parent-issue-source-id");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config_with_host("ghe.internal.example", &server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let parent_id = "qgh://ghe.internal.example/issue/I_kwDOISSUE1";
    let comment_id = "qgh://ghe.internal.example/issue-comment/IC_kwDOCOMMENT1";

    let get = fixture.qgh(["get", comment_id, "--json"]);
    assert_success(&get);
    let get_json = stdout_json(&get);
    assert_eq!(
        get_json["data"]["source"]["parent_issue"]["source_id"],
        parent_id
    );

    let query = fixture.qgh(["query", "comment-only mitigation", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    assert_eq!(
        query_json["data"]["results"][0]["parent_issue"]["source_id"],
        parent_id
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
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "query": "anything",
                    "max_age": "0d"
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "status",
                "arguments": {
                    "max_age": "0d"
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "query": "anything",
                    "limit": 0
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "query": "anything",
                    "issue": 0
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "query": "anything",
                    "state": "merged"
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "query": "anything",
                    "repo": "owner"
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "query": "anything",
                    "rerank": "yes"
                }
            }
        }),
    ]);
    assert_success(&output);
    assert!(stderr_text(&output).is_empty());
    let messages = stdout_json_lines(&output);
    assert_eq!(messages.len(), 10);
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
        if tool["name"] == "query" {
            assert!(tool["inputSchema"]["properties"].get("max_age").is_some());
            assert!(tool["inputSchema"]["properties"]
                .get("require_fresh")
                .is_some());
            assert_eq!(
                tool["inputSchema"]["properties"]["limit"]["minimum"],
                json!(1)
            );
            assert_eq!(
                tool["inputSchema"]["properties"]["issue"]["minimum"],
                json!(1)
            );
            assert_eq!(
                tool["inputSchema"]["properties"]["repo"]["pattern"],
                "^[^/]+/[^/]+$"
            );
            assert_eq!(
                tool["inputSchema"]["properties"]["rerank"]["type"],
                "boolean"
            );
        }
        if tool["name"] == "status" {
            assert!(tool["inputSchema"]["properties"].get("max_age").is_some());
            assert!(tool["inputSchema"]["properties"]
                .get("require_fresh")
                .is_some());
        }
        if tool["name"] == "get" {
            assert!(
                tool["inputSchema"]["properties"]
                    .get("verify_lifecycle")
                    .is_none(),
                "MCP get must stay local-only/read-only; lifecycle verification is CLI-only"
            );
        }
        assert_eq!(tool["outputSchema"]["type"], "object");
        assert!(tool["outputSchema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("schema_version")));
        assert_eq!(
            tool["outputSchema"]["properties"]["meta"]["additionalProperties"],
            false
        );
        assert!(
            tool["outputSchema"]["properties"]["meta"]["properties"]["repo_source"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("command"))
        );
    }

    let validation = &messages[2]["result"];
    assert_eq!(validation["isError"], true);
    assert_eq!(
        validation["structuredContent"]["error"]["code"],
        "validation.mcp"
    );
    assert_eq!(validation["structuredContent"]["schema_version"], "qgh.v2");
    assert!(validation["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("validation.mcp"));

    for validation in [
        &messages[3]["result"],
        &messages[4]["result"],
        &messages[5]["result"],
        &messages[6]["result"],
        &messages[7]["result"],
        &messages[8]["result"],
        &messages[9]["result"],
    ] {
        assert_eq!(validation["isError"], true);
        assert_eq!(
            validation["structuredContent"]["error"]["code"],
            "validation.mcp"
        );
    }
    assert!(
        messages[3]["result"]["structuredContent"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_age")
    );
    assert!(
        messages[4]["result"]["structuredContent"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_age")
    );
    assert!(
        messages[5]["result"]["structuredContent"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("limit")
    );
    assert!(
        messages[6]["result"]["structuredContent"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("issue")
    );
    assert!(
        messages[7]["result"]["structuredContent"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("state")
    );
    assert!(
        messages[8]["result"]["structuredContent"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("repo")
    );
}

#[test]
fn mcp_rejects_malformed_envelopes_and_method_params() {
    let fixture = TestFixture::new("mcp-strict-envelope");
    fixture.write_config("http://127.0.0.1:1");

    let output = fixture.mcp([
        json!({"id": 1, "method": "ping"}),
        json!({"jsonrpc": "1.0", "id": 2, "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "ping", "extra": true}),
        json!([]),
        json!({"jsonrpc": "2.0", "id": 5, "method": "initialize", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 6, "method": "ping", "params": "bad"}),
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list", "params": {"bogus": true}}),
        json!({"jsonrpc": "2.0", "id": 8, "method": "ping", "params": null}),
        json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list", "params": {"_meta": {}}}),
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "initialize",
            "params": {
                "protocolVersion": "2099-01-01",
                "capabilities": {"futureCapability": true},
                "clientInfo": {"name": "future-client", "version": "1", "futureField": true},
                "_meta": {"trace": "opaque"}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "query": "anything",
                    "repo": "owner/repo?access_token=PRIVATE_MCP_REPO_MARKER"
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {"unexpected": "PRIVATE_NOTIFICATION_MARKER"}
        }),
        json!({
            "jsonrpc": "2.0",
            "method": "future/notification",
            "params": "PRIVATE_UNKNOWN_NOTIFICATION_MARKER"
        }),
    ]);

    assert_success(&output);
    assert!(stderr_text(&output).is_empty());
    let messages = stdout_json_lines(&output);
    assert_eq!(messages.len(), 11);
    for (message, id) in messages[..3].iter().zip([1, 2, 3]) {
        assert_eq!(message["id"], id);
        assert_eq!(message["error"]["code"], -32600);
    }
    assert_eq!(messages[3]["id"], Value::Null);
    assert_eq!(messages[3]["error"]["code"], -32600);
    for (message, id) in messages[4..7].iter().zip([5, 6, 7]) {
        assert_eq!(message["id"], id);
        assert_eq!(message["error"]["code"], -32602);
    }
    assert_eq!(messages[7]["result"], json!({}));
    assert_eq!(messages[8]["result"]["tools"].as_array().unwrap().len(), 3);
    assert_eq!(messages[9]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(messages[10]["result"]["isError"], true);
    assert_eq!(
        messages[10]["result"]["structuredContent"]["error"]["code"],
        "validation.mcp"
    );
    assert!(!format!("{}{}", stdout_text(&output), stderr_text(&output))
        .contains("PRIVATE_MCP_REPO_MARKER"));
    assert!(!format!("{}{}", stdout_text(&output), stderr_text(&output))
        .contains("PRIVATE_NOTIFICATION_MARKER"));
    assert!(!format!("{}{}", stdout_text(&output), stderr_text(&output))
        .contains("PRIVATE_UNKNOWN_NOTIFICATION_MARKER"));
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
    assert_eq!(issue_query["warnings"], json!([]));
    assert_eq!(issue_query["data"]["freshness"]["decision"], "fresh");
    assert_eq!(issue_query["data"]["freshness"]["remote_checked"], false);
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
    assert_eq!(
        issue_get["data"]["source"]["lifecycle_check"]["reason"],
        "not_requested"
    );
    assert_eq!(
        issue_get["data"]["source"]["lifecycle_check"]["remote_checked"],
        false
    );
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
    assert_eq!(
        comment_get["data"]["source"]["lifecycle_check"]["reason"],
        "not_requested"
    );
    assert!(comment_get["data"]["source"]["body"]
        .as_str()
        .unwrap()
        .contains("comment-only mitigation"));

    let status = &messages[5]["result"]["structuredContent"];
    assert_eq!(status["ok"], true);
    assert_eq!(status["data"]["profile_id"], "work");
    assert_eq!(status["warnings"], json!([]));
    assert_eq!(status["data"]["freshness"]["decision"], "fresh");
    assert_eq!(status["data"]["freshness"]["remote_checked"], false);
    assert_eq!(status["data"]["sources"]["issue_count"], 1);
    assert_eq!(status["data"]["sources"]["comment_count"], 1);
}

#[test]
fn mcp_get_rejects_lifecycle_verification_to_preserve_read_only_contract() {
    let fixture = TestFixture::new("mcp-get-lifecycle-read-only");
    let server = LifecycleFakeGitHub::start();
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));
    let issue_source_id = "qgh://github.com/issue/I_kwDOISSUE1";

    server.set_mode(LIFECYCLE_UNAVAILABLE_ISSUE);
    let request_count_before_mcp_get = server.request_count();
    let output = fixture.mcp([
        json!({
            "jsonrpc": "2.0",
            "id": 1,
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
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "get",
                "arguments": {
                    "source_id": issue_source_id,
                    "verify_lifecycle": true
                }
            }
        }),
    ]);
    assert_success(&output);
    assert!(stderr_text(&output).is_empty());
    let messages = stdout_json_lines(&output);
    assert_eq!(messages.len(), 2);

    let default_get = &messages[0]["result"]["structuredContent"];
    assert_eq!(default_get["ok"], true);
    assert_eq!(default_get["data"]["source"]["source_id"], issue_source_id);
    assert_eq!(
        default_get["data"]["source"]["lifecycle_check"]["reason"],
        "not_requested"
    );

    let rejected_get = &messages[1]["result"];
    assert_eq!(rejected_get["isError"], true);
    assert_eq!(
        rejected_get["structuredContent"]["error"]["code"],
        "validation.mcp"
    );
    assert!(rejected_get["structuredContent"]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Unknown MCP parameter `verify_lifecycle`"));
    assert_eq!(
        server.request_count(),
        request_count_before_mcp_get,
        "MCP get must reject lifecycle verification before probing GitHub"
    );
}

#[test]
fn mcp_without_profile_uses_repo_policy_single_match_scope() {
    let fixture = TestFixture::new("mcp-policy-single-match");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles(&server.base_url);
    let nested_worktree_dir = fixture.init_git_worktree_with_repo_policy("owner/repo");
    assert_success(&fixture.qgh_in(&nested_worktree_dir, ["sync", "--all", "--json"]));

    let output = fixture.mcp_without_profile_in(
        &nested_worktree_dir,
        [
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
                        "query": "shared repo policy tracer",
                        "limit": 10
                    }
                }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "get",
                    "arguments": {
                        "source_id": "qgh://github.com/issue/I_POLICY_OWNER"
                    }
                }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {
                    "name": "status",
                    "arguments": {}
                }
            }),
        ],
    );
    assert_success(&output);
    assert!(stderr_text(&output).is_empty());
    let messages = stdout_json_lines(&output);
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[1]["result"]["tools"].as_array().unwrap().len(), 3);

    let query = &messages[2]["result"]["structuredContent"];
    assert_eq!(query["ok"], true);
    assert_eq!(query["meta"]["profile_id"], "work");
    assert_eq!(query["meta"]["profile_source"], "single_match");
    assert_eq!(query["meta"]["repo"], "owner/repo");
    assert_eq!(query["meta"]["repo_source"], "repo_policy");
    assert!(query["meta"]["repo_policy_path"]
        .as_str()
        .unwrap()
        .ends_with(".qgh.toml"));
    assert_eq!(query["data"]["results"][0]["repo"], "owner/repo");

    let get = &messages[3]["result"]["structuredContent"];
    assert_eq!(get["ok"], true);
    assert_eq!(get["meta"]["profile_id"], "work");
    assert_eq!(get["meta"]["repo"], "owner/repo");
    assert_eq!(
        get["data"]["source"]["source_id"],
        "qgh://github.com/issue/I_POLICY_OWNER"
    );

    let status = &messages[4]["result"]["structuredContent"];
    assert_eq!(status["ok"], true);
    assert_eq!(status["meta"]["profile_source"], "single_match");
    assert_eq!(
        status["data"]["resolution"]["effective_repo_scope"],
        "owner/repo"
    );
    assert_eq!(status["data"]["resolution"]["repo_source"], "repo_policy");
}

#[test]
fn mcp_without_profile_uses_git_origin_single_match_scope_without_repo_policy() {
    let fixture = TestFixture::new("mcp-origin-single-match");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");
    assert_success(&fixture.qgh_without_profile_in(&nested_worktree_dir, ["sync", "--json"]));

    let output = fixture.mcp_without_profile_in(
        &nested_worktree_dir,
        [
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "query",
                    "arguments": {
                        "query": "shared repo policy tracer"
                    }
                }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "status",
                    "arguments": {}
                }
            }),
        ],
    );
    assert_success(&output);
    assert!(stderr_text(&output).is_empty());
    let messages = stdout_json_lines(&output);
    let query = &messages[0]["result"]["structuredContent"];
    assert_eq!(query["ok"], true);
    assert_eq!(query["meta"]["profile_source"], "single_match");
    assert_eq!(query["meta"]["repo"], "owner/repo");
    assert_eq!(query["meta"]["repo_source"], "git_remote");
    assert_eq!(query["data"]["results"][0]["repo"], "owner/repo");

    let status = &messages[1]["result"]["structuredContent"];
    assert_eq!(status["ok"], true);
    assert_eq!(status["data"]["resolution"]["repo_source"], "git_remote");
}

#[test]
fn mcp_repo_argument_override_uses_command_scope_and_checks_allowlist() {
    let fixture = TestFixture::new("mcp-repo-override");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    let nested_worktree_dir = fixture.init_git_worktree_with_repo_policy("owner/repo");
    assert_success(&fixture.qgh_in(&nested_worktree_dir, ["sync", "--all", "--json"]));

    let output = fixture.mcp_without_profile_in(
        &nested_worktree_dir,
        [json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "query": "shared repo policy tracer",
                    "repo": "other/repo",
                    "limit": 10
                }
            }
        })],
    );
    assert_success(&output);
    assert!(stderr_text(&output).is_empty());
    let messages = stdout_json_lines(&output);
    let query = &messages[0]["result"]["structuredContent"];
    assert_eq!(query["ok"], true);
    assert_eq!(query["meta"]["profile_id"], "work");
    assert_eq!(query["meta"]["profile_source"], "single_match");
    assert_eq!(query["meta"]["repo"], "other/repo");
    assert_eq!(query["meta"]["repo_source"], "command");
    assert_eq!(query["meta"]["repo_policy_path"], Value::Null);
    assert_eq!(query["data"]["results"][0]["repo"], "other/repo");

    let restricted = TestFixture::new("mcp-repo-override-restricted");
    let restricted_server = MultiRepoFakeGitHub::start();
    restricted.write_config(&restricted_server.base_url);
    let restricted_worktree = restricted.init_git_worktree_with_repo_policy("owner/repo");
    assert_success(&restricted.qgh_in(&restricted_worktree, ["sync", "--json"]));

    let error_output = restricted.mcp_in(
        &restricted_worktree,
        Some("work"),
        [json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "query",
                "arguments": {
                    "query": "shared repo policy tracer",
                    "repo": "other/repo"
                }
            }
        })],
    );
    assert_success(&error_output);
    assert!(stderr_text(&error_output).is_empty());
    let error_messages = stdout_json_lines(&error_output);
    let result = &error_messages[0]["result"];
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"]["code"],
        "validation.invalid_repo"
    );
}

#[test]
fn query_get_args_carry_profile_for_no_profile_repo_override_round_trip() {
    let fixture = TestFixture::new("repo-override-get-profile");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles(&server.base_url);
    let nested_worktree_dir = fixture.init_git_worktree_with_repo_policy("owner/repo");

    assert_success(&fixture.qgh_in(&nested_worktree_dir, ["sync", "--json"]));
    assert_success(&fixture.qgh_in_profile(
        &nested_worktree_dir,
        "alt",
        ["sync", "--all", "--json"],
    ));

    let cli_query = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "query",
            "shared repo policy tracer",
            "--repo",
            "other/repo",
            "--json",
        ],
    );
    assert_success(&cli_query);
    let cli_query_json = stdout_json(&cli_query);
    let cli_result = &cli_query_json["data"]["results"][0];
    assert_eq!(cli_query_json["meta"]["profile_id"], "alt");
    assert_eq!(
        cli_result["source_id"],
        "qgh://github.com/issue/I_POLICY_OTHER"
    );
    assert_eq!(cli_result["get_args"]["source_id"], cli_result["source_id"]);
    assert_eq!(cli_result["get_args"]["profile_id"], "alt");

    let cli_get = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "get",
            cli_result["get_args"]["source_id"].as_str().unwrap(),
            "--profile-id",
            cli_result["get_args"]["profile_id"].as_str().unwrap(),
            "--json",
        ],
    );
    assert_success(&cli_get);
    assert_eq!(
        stdout_json(&cli_get)["data"]["source"]["source_id"],
        cli_result["source_id"]
    );

    let cli_batch_get = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        [
            "get",
            cli_result["get_args"]["source_id"].as_str().unwrap(),
            cli_result["get_args"]["source_id"].as_str().unwrap(),
            "--profile-id",
            cli_result["get_args"]["profile_id"].as_str().unwrap(),
            "--json",
        ],
    );
    assert_success(&cli_batch_get);
    let cli_batch_json = stdout_json(&cli_batch_get);
    assert_eq!(cli_batch_json["meta"]["profile_source"], "get_args");
    assert_eq!(cli_batch_json["data"]["profile_id"], "alt");
    assert_eq!(cli_batch_json["data"]["summary"]["requested"], 2);
    assert_eq!(
        cli_batch_json["data"]["items"][0]["source"]["source_id"],
        cli_result["source_id"]
    );

    let mcp = fixture.mcp_without_profile_in(
        &nested_worktree_dir,
        [
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "query",
                    "arguments": {
                        "query": "shared repo policy tracer",
                        "repo": "other/repo"
                    }
                }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "get",
                    "arguments": {
                        "source_id": "qgh://github.com/issue/I_POLICY_OTHER",
                        "profile_id": "alt"
                    }
                }
            }),
        ],
    );
    assert_success(&mcp);
    assert!(stderr_text(&mcp).is_empty());
    let messages = stdout_json_lines(&mcp);
    let mcp_query = &messages[0]["result"]["structuredContent"];
    assert_eq!(mcp_query["ok"], true);
    assert_eq!(mcp_query["meta"]["profile_id"], "alt");
    assert_eq!(
        mcp_query["data"]["results"][0]["get_args"]["profile_id"],
        "alt"
    );

    let mcp_get = &messages[1]["result"]["structuredContent"];
    assert_eq!(mcp_get["ok"], true);
    assert_eq!(mcp_get["meta"]["profile_id"], "alt");
    assert_eq!(
        mcp_get["data"]["source"]["source_id"],
        "qgh://github.com/issue/I_POLICY_OTHER"
    );

    let mut cli_conflict = fixture.base_command();
    let cli_conflict = cli_conflict
        .current_dir(&nested_worktree_dir)
        .args([
            "--profile",
            "work",
            "get",
            "qgh://github.com/issue/I_POLICY_OTHER",
            "--profile-id",
            "alt",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(cli_conflict.status.code(), Some(2));
    assert_eq!(
        stdout_json(&cli_conflict)["error"]["code"],
        "validation.cli"
    );

    let mcp_conflict = fixture.mcp_in(
        &nested_worktree_dir,
        Some("work"),
        [json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "get",
                "arguments": {
                    "source_id": "qgh://github.com/issue/I_POLICY_OTHER",
                    "profile_id": "alt"
                }
            }
        })],
    );
    assert_success(&mcp_conflict);
    assert!(stderr_text(&mcp_conflict).is_empty());
    let conflict_messages = stdout_json_lines(&mcp_conflict);
    assert_eq!(conflict_messages[0]["result"]["isError"], true);
    assert_eq!(
        conflict_messages[0]["result"]["structuredContent"]["error"]["code"],
        "validation.mcp"
    );
}

#[test]
fn mcp_resolution_failures_are_structured_tool_errors() {
    let no_match = TestFixture::new("mcp-resolution-no-match");
    no_match.write_config("http://127.0.0.1:1");
    let no_match_worktree = no_match.init_git_worktree_with_repo_policy("other/repo");
    let no_match_output = no_match.mcp_without_profile_in(
        &no_match_worktree,
        [
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
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "query",
                    "arguments": {
                        "query": "anything"
                    }
                }
            }),
        ],
    );
    assert_success(&no_match_output);
    assert!(stderr_text(&no_match_output).is_empty());
    let no_match_messages = stdout_json_lines(&no_match_output);
    assert_eq!(no_match_messages[0]["id"], 1);
    assert_eq!(no_match_messages[1]["result"]["isError"], true);
    assert_eq!(
        no_match_messages[1]["result"]["structuredContent"]["error"]["code"],
        "config.no_matching_profile"
    );

    let ambiguous = TestFixture::new("mcp-resolution-ambiguous");
    ambiguous.write_config_with_duplicate_owner_profiles("http://127.0.0.1:1");
    let ambiguous_worktree = ambiguous.init_git_worktree_with_repo_policy("owner/repo");
    let ambiguous_output = ambiguous.mcp_without_profile_in(
        &ambiguous_worktree,
        [json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "status",
                "arguments": {}
            }
        })],
    );
    assert_success(&ambiguous_output);
    assert!(stderr_text(&ambiguous_output).is_empty());
    let ambiguous_messages = stdout_json_lines(&ambiguous_output);
    assert_eq!(ambiguous_messages[0]["result"]["isError"], true);
    assert_eq!(
        ambiguous_messages[0]["result"]["structuredContent"]["error"]["code"],
        "config.ambiguous_profile"
    );

    let invalid_policy = TestFixture::new("mcp-resolution-invalid-policy");
    invalid_policy.write_config("http://127.0.0.1:1");
    let invalid_policy_worktree = invalid_policy.init_git_worktree_with_repo_policy("owner/repo");
    fs::write(
        invalid_policy.root.join(".qgh.toml"),
        r#"
schema_version = "qgh.repo.v1"
token = "ghp_plaintext"

[repo]
github = "owner/repo"
"#,
    )
    .unwrap();
    let invalid_policy_output = invalid_policy.mcp_without_profile_in(
        &invalid_policy_worktree,
        [json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "status",
                "arguments": {}
            }
        })],
    );
    assert_success(&invalid_policy_output);
    assert!(stderr_text(&invalid_policy_output).is_empty());
    let invalid_policy_messages = stdout_json_lines(&invalid_policy_output);
    assert_eq!(invalid_policy_messages[0]["result"]["isError"], true);
    assert_eq!(
        invalid_policy_messages[0]["result"]["structuredContent"]["error"]["code"],
        "config.invalid_repo_policy"
    );
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
fn concurrent_syncs_use_one_profile_writer_and_return_sync_busy() {
    let fixture = TestFixture::new("single-flight-sync");
    let server = SlowFirstRequestFakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);

    let mut first_command = fixture.base_command();
    first_command
        .args(["--profile", "work", "sync", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let first = first_command.spawn().unwrap();
    server.wait_until_first_request();

    let mut second_command = fixture.base_command();
    second_command
        .args(["--profile", "work", "sync", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut second = second_command.spawn().unwrap();
    let deadline = std::time::Instant::now() + StdDuration::from_secs(2);
    while second.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        thread::sleep(StdDuration::from_millis(10));
    }
    server.release_first_request();
    let second = second.wait_with_output().unwrap();
    assert_eq!(second.status.code(), Some(5));
    assert!(stderr_text(&second).is_empty());
    let second_json = stdout_json(&second);
    assert_eq!(second_json["ok"], false);
    assert_eq!(second_json["error"]["code"], "sync.busy");
    assert_eq!(second_json["error"]["retryable"], true);
    assert_eq!(second_json["error"]["details"]["profile_id"], "work");

    let first = first.wait_with_output().unwrap();
    assert_success(&first);

    let after_release = fixture.qgh(["sync", "--json"]);
    assert_success(&after_release);
}

#[test]
fn profile_writer_lease_is_shared_by_sync_and_targeted_sync() {
    let fixture = TestFixture::new("single-flight-cross-command");
    let server = SlowFirstRequestFakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);

    let mut owner_command = fixture.base_command();
    owner_command
        .args(["--profile", "work", "sync", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let owner = owner_command.spawn().unwrap();
    server.wait_until_first_request();

    let targeted = fixture.qgh(["sync", "issue", "42", "--repo", "owner/repo", "--json"]);
    assert_eq!(targeted.status.code(), Some(5));
    assert!(stderr_text(&targeted).is_empty());
    let targeted_json = stdout_json(&targeted);
    assert_eq!(targeted_json["error"]["code"], "sync.busy");
    assert_eq!(targeted_json["error"]["retryable"], true);
    assert_eq!(server.accepted_request_count(), 1);

    server.release_first_request();
    assert_success(&owner.wait_with_output().unwrap());
}

#[test]
fn profile_sync_lease_is_released_when_owner_process_is_killed() {
    let fixture = TestFixture::new("single-flight-sync-killed-owner");
    let server = SlowFirstRequestFakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);

    let mut owner_command = fixture.base_command();
    owner_command
        .args(["--profile", "work", "sync", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut owner = owner_command.spawn().unwrap();
    server.wait_until_first_request();
    owner.kill().unwrap();
    owner.wait().unwrap();
    server.release_first_request();

    let recovered = fixture.qgh(["sync", "--json"]);
    assert_success(&recovered);
}

#[test]
fn sync_and_status_report_effective_sequential_request_concurrency() {
    let fixture = TestFixture::new("truthful-sync-concurrency");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);

    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);
    let scheduler = &stdout_json(&sync)["data"]["scheduler"];
    assert_eq!(scheduler["mode"], "sequential");
    assert_eq!(scheduler["max_in_flight_requests"], 1);
    assert_eq!(scheduler["hard_cap"], 1);
    assert_eq!(scheduler["configured_max_in_flight_requests"], 4);
    assert_eq!(scheduler["configuration_hard_cap"], 16);

    let skipped = fixture.qgh(["sync", "--if-stale", "--json"]);
    assert_success(&skipped);
    assert_eq!(stdout_json(&skipped)["data"]["scheduler"], *scheduler);

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    assert_eq!(
        stdout_json(&status)["data"]["sync"]["scheduler"],
        *scheduler
    );

    let human_sync = fixture.qgh(["sync"]);
    assert_success(&human_sync);
    let human_sync = stdout_text(&human_sync);
    assert!(human_sync.contains(
        "sync requests: sequential; effective max in-flight 1 (hard cap 1); configured 4 (compatibility only, configuration hard cap 16)"
    ));

    let human_status = fixture.qgh(["status"]);
    assert_success(&human_status);
    let human_status = stdout_text(&human_status);
    assert!(human_status.contains(
        "sync requests: sequential; effective max in-flight 1 (hard cap 1); configured 4 (compatibility only, configuration hard cap 16)"
    ));
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
    assert!(cli_contract.contains("qgh.v2"));
    assert!(cli_contract.contains("docs/schemas/envelope.schema.json"));
    assert!(cli_contract.contains("docs/schemas/error.schema.json"));
}

#[test]
fn profile_config_rejects_cross_origin_api_and_secret_repo_values_content_free() {
    for (name, api_base_url, repo, marker) in [
        (
            "config-cross-origin-api",
            "https://attacker.invalid/PRIVATE_API_CONFIG_MARKER",
            "owner/repo",
            "PRIVATE_API_CONFIG_MARKER",
        ),
        (
            "config-secret-repo",
            "https://api.github.com",
            "owner/repo?token=PRIVATE_REPO_CONFIG_MARKER",
            "PRIVATE_REPO_CONFIG_MARKER",
        ),
    ] {
        let fixture = TestFixture::new(name);
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["{repo}"]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
        );
        fs::write(fixture.config_home.join("qgh/config.toml"), config).unwrap();

        let status = fixture.qgh(["status", "--json"]);
        assert_eq!(status.status.code(), Some(2));
        let output = format!("{}{}", stdout_text(&status), stderr_text(&status));
        assert!(!output.contains(marker), "{output}");
    }
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

    let secret_repo = fixture.qgh([
        "query",
        "anything",
        "--repo",
        "owner/repo?access_token=PRIVATE_CLI_REPO_MARKER",
        "--json",
    ]);
    assert_eq!(secret_repo.status.code(), Some(2));
    assert_eq!(
        stdout_json(&secret_repo)["error"]["code"],
        "validation.invalid_repo"
    );
    assert!(
        !format!("{}{}", stdout_text(&secret_repo), stderr_text(&secret_repo))
            .contains("PRIVATE_CLI_REPO_MARKER")
    );

    let wiki_filter = fixture.qgh(["query", "anything", "--wiki", "Home", "--json"]);
    assert_eq!(wiki_filter.status.code(), Some(2));
    assert_eq!(
        stdout_json(&wiki_filter)["error"]["code"],
        "validation.unsupported_filter"
    );

    let zero_limit = fixture.qgh(["query", "anything", "--limit", "0", "--json"]);
    assert_eq!(zero_limit.status.code(), Some(2));
    assert_eq!(
        stdout_json(&zero_limit)["error"]["code"],
        "validation.invalid_query"
    );

    let zero_issue = fixture.qgh(["query", "anything", "--issue", "0", "--json"]);
    assert_eq!(zero_issue.status.code(), Some(2));
    let zero_issue_json = stdout_json(&zero_issue);
    assert_eq!(
        zero_issue_json["error"]["code"],
        "validation.invalid_issue_number"
    );
    assert_eq!(zero_issue_json["error"]["details"]["issue_number"], 0);

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
fn no_matching_profile_is_a_structured_usage_error() {
    let fixture = TestFixture::new("missing-profile");
    let output = fixture.qgh_without_profile(["status", "--json"]);
    assert_eq!(output.status.code(), Some(2));

    let json = stdout_json(&output);
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "config.no_matching_profile");
    assert_eq!(json["error"]["exit_code"], 2);
    assert!(stderr_text(&output).is_empty());
}

#[test]
fn profile_resolution_uses_cli_then_env_then_repo_scope_single_match() {
    let fixture = TestFixture::new("profile-resolution-precedence");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_work_and_alt_profiles(&server.base_url);
    let nested_worktree_dir = fixture.init_git_worktree_with_repo_policy("owner/repo");

    assert_success(&fixture.qgh_in(&nested_worktree_dir, ["sync", "--json"]));

    let mut cli_over_env = fixture.base_command();
    let cli_over_env = cli_over_env
        .current_dir(&nested_worktree_dir)
        .env("QGH_PROFILE", "alt")
        .args(["--profile", "work", "status", "--json"])
        .output()
        .unwrap();
    assert_success(&cli_over_env);
    assert_eq!(stdout_json(&cli_over_env)["data"]["profile_id"], "work");

    let mut env_profile = fixture.base_command();
    let env_profile = env_profile
        .current_dir(&nested_worktree_dir)
        .env("QGH_PROFILE", "alt")
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert_eq!(env_profile.status.code(), Some(2));
    assert_eq!(
        stdout_json(&env_profile)["error"]["code"],
        "config.invalid_repo_policy"
    );

    let mut allowed_env_profile = fixture.base_command();
    let allowed_env_profile = allowed_env_profile
        .current_dir(&nested_worktree_dir)
        .env("QGH_PROFILE", "work")
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert_success(&allowed_env_profile);
    let allowed_env_json = stdout_json(&allowed_env_profile);
    assert_eq!(allowed_env_json["data"]["profile_id"], "work");
    assert_eq!(allowed_env_json["meta"]["profile_source"], "env");

    let single_match_query = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        ["query", "shared repo policy tracer", "--json"],
    );
    assert_success(&single_match_query);
    let single_match_json = stdout_json(&single_match_query);
    assert_eq!(single_match_json["data"]["profile_id"], "work");
    assert_eq!(single_match_json["meta"]["profile_id"], "work");
    assert_eq!(single_match_json["meta"]["profile_source"], "single_match");
    assert_eq!(single_match_json["meta"]["repo"], "owner/repo");
    assert_eq!(single_match_json["meta"]["repo_source"], "repo_policy");
    assert_eq!(
        single_match_json["data"]["results"][0]["repo"],
        "owner/repo"
    );

    let status = fixture.qgh_without_profile_in(&nested_worktree_dir, ["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["data"]["profile_id"], "work");
    assert_eq!(status_json["meta"]["profile_source"], "single_match");
    assert_eq!(status_json["data"]["resolution"]["profile_id"], "work");
    assert_eq!(
        status_json["data"]["resolution"]["effective_repo_scope"],
        "owner/repo"
    );
    assert_eq!(
        status_json["data"]["resolution"]["repo_source"],
        "repo_policy"
    );
    assert!(status_json["data"]["resolution"]["repo_policy_path"]
        .as_str()
        .unwrap()
        .ends_with(".qgh.toml"));

    let get = fixture.qgh_without_profile_in(
        &nested_worktree_dir,
        ["get", "qgh://github.com/issue/I_POLICY_OWNER", "--json"],
    );
    assert_success(&get);
    let get_json = stdout_json(&get);
    assert_eq!(get_json["data"]["profile_id"], "work");
    assert_eq!(get_json["meta"]["profile_source"], "single_match");
}

#[test]
fn profile_resolution_treats_host_case_as_one_identity() {
    let fixture = TestFixture::new("profile-resolution-host-case");
    fixture.write_config_with_host("GitHub.COM", "http://127.0.0.1:1");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("https://github.com/owner/repo.git");

    let status = fixture.qgh_without_profile_in(&nested_worktree_dir, ["status", "--json"]);
    assert_success(&status);
    let json = stdout_json(&status);
    assert_eq!(json["data"]["profile_id"], "work");
    assert_eq!(json["meta"]["profile_source"], "single_match");

    let explicit = fixture.qgh_in_profile(&nested_worktree_dir, "work", ["status", "--json"]);
    assert_success(&explicit);
    assert_eq!(stdout_json(&explicit)["meta"]["profile_source"], "cli");
}

#[test]
fn profile_resolution_reports_no_match_and_ambiguous_match() {
    let fixture = TestFixture::new("profile-resolution-errors");
    fixture.write_config_with_repos("http://127.0.0.1:1", &["other/repo"]);
    let nested_worktree_dir = fixture.init_git_worktree_with_repo_policy("owner/repo");

    let no_match =
        fixture.qgh_without_profile_in(&nested_worktree_dir, ["query", "anything", "--json"]);
    assert_eq!(no_match.status.code(), Some(2));
    let no_match_json = stdout_json(&no_match);
    assert_eq!(no_match_json["error"]["code"], "config.no_matching_profile");
    assert_eq!(no_match_json["error"]["details"]["repo"], "owner/repo");
    assert!(no_match_json["error"]["hint"]
        .as_str()
        .unwrap()
        .contains("--profile"));

    let human_no_match =
        fixture.qgh_without_profile_in(&nested_worktree_dir, ["query", "anything"]);
    assert_eq!(human_no_match.status.code(), Some(2));
    assert!(stderr_text(&human_no_match).contains("--profile"));

    fixture.write_config_with_duplicate_owner_profiles("http://127.0.0.1:1");
    let ambiguous =
        fixture.qgh_without_profile_in(&nested_worktree_dir, ["query", "anything", "--json"]);
    assert_eq!(ambiguous.status.code(), Some(2));
    let ambiguous_json = stdout_json(&ambiguous);
    assert_eq!(ambiguous_json["error"]["code"], "config.ambiguous_profile");
    assert_eq!(ambiguous_json["error"]["details"]["repo"], "owner/repo");
    assert_eq!(
        ambiguous_json["error"]["details"]["matching_profile_ids"],
        json!(["other", "work"])
    );
}

#[test]
fn status_and_doctor_report_effective_scope_diagnostics() {
    let fixture = TestFixture::new("effective-scope-diagnostics");
    let server = FakeGitHub::start(issue_payload_with_pr());
    fixture.write_config(&server.base_url);
    let nested_worktree_dir = fixture.init_git_worktree_with_repo_policy("owner/repo");

    let status_before = server.request_count();
    let status = fixture.qgh_without_profile_in(&nested_worktree_dir, ["status", "--json"]);
    assert_success(&status);
    assert_eq!(
        server.request_count(),
        status_before,
        "status must remain local-only while reporting resolution"
    );
    let status_json = stdout_json(&status);
    assert_eq!(
        status_json["data"]["resolution"]["profile_source"],
        "single_match"
    );
    assert_eq!(
        status_json["data"]["resolution"]["effective_repo_scope"],
        "owner/repo"
    );
    assert!(status_json["data"]["paths"]["profile_data"]
        .as_str()
        .unwrap()
        .contains("profiles/work"));

    let doctor = fixture.qgh_without_profile_in(&nested_worktree_dir, ["doctor", "--json"]);
    assert_success(&doctor);
    let doctor_json = stdout_json(&doctor);
    let checks = doctor_json["data"]["checks"].as_array().unwrap();
    assert!(checks
        .iter()
        .any(|check| check["name"] == "repo_policy" && check["ok"] == true));
    assert!(checks
        .iter()
        .any(|check| check["name"] == "profile_resolution" && check["ok"] == true));
    assert_eq!(
        doctor_json["data"]["resolution"]["allowlist_match_count"],
        1
    );
    assert_eq!(
        doctor_json["data"]["resolution"]["repo_source"],
        "repo_policy"
    );
}

#[test]
fn env_profile_resolution_preserves_token_source_strictness() {
    let fixture = TestFixture::new("profile-resolution-token-source");
    fixture.write_config_with_missing_token_profile("http://127.0.0.1:1");

    let mut sync = fixture.base_command();
    let sync = sync
        .env("QGH_PROFILE", "strict")
        .env_remove("QGH_MISSING_TOKEN")
        .args(["sync", "--json"])
        .output()
        .unwrap();
    assert_eq!(sync.status.code(), Some(3));
    assert_eq!(
        stdout_json(&sync)["error"]["code"],
        "auth.token_unavailable"
    );
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

    server.set_mode(TARGET_REFRESH_DIFF);
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
    let observations = sync_rate_budget_observations(&third_sync_json);
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0]["remaining"], 4997);
    assert_eq!(observations[0]["state"], "fresh");
    fixture.assert_source_version_count(issue_id, 2);
    fixture.assert_source_version_count(comment_id, 2);
}

#[test]
fn unchanged_bm25_sync_reuses_publication_until_source_content_changes() {
    let fixture = TestFixture::new("bm25-no-change-publication");
    let server = EditingFakeGitHub::start();
    fixture.write_config(&server.base_url);

    let first_sync = fixture.qgh(["sync", "--json"]);
    assert_success(&first_sync);
    let first_sync_json = stdout_json(&first_sync);
    let first_footprint = fixture.bm25_publication_footprint();
    let source_id = "qgh://github.com/issue/I_kwDOISSUE1";
    let first_query =
        stdout_json(&fixture.qgh(["query", "round-trip through get before citation", "--json"]));
    let first_get = stdout_json(&fixture.qgh(["get", source_id, "--json"]));
    assert_query_result_round_trips_to_get_result(
        &first_query["data"]["results"][0],
        &first_get["data"]["source"],
    );

    let second_sync = fixture.qgh(["sync", "--json"]);
    assert_success(&second_sync);
    let second_sync_json = stdout_json(&second_sync);
    assert_ne!(
        second_sync_json["data"]["sync_run_id"], first_sync_json["data"]["sync_run_id"],
        "no-change sync must still record normal sync freshness metadata"
    );
    assert_eq!(second_sync_json["data"]["index"]["dirty_task_count"], 0);
    assert_eq!(
        fixture.bm25_publication_footprint(),
        first_footprint,
        "a verified BM25-only no-change sync must not publish or retain another generation"
    );
    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(
        status["data"]["index"]["active_generation"],
        first_footprint.active_generation
    );
    let second_query =
        stdout_json(&fixture.qgh(["query", "round-trip through get before citation", "--json"]));
    let second_get = stdout_json(&fixture.qgh(["get", source_id, "--json"]));
    assert_query_result_round_trips_to_get_result(
        &second_query["data"]["results"][0],
        &second_get["data"]["source"],
    );

    server.set_mode(TARGET_REFRESH_DIFF);
    let edited_sync = fixture.qgh(["sync", "--json"]);
    assert_success(&edited_sync);
    let edited_footprint = fixture.bm25_publication_footprint();
    assert_ne!(
        edited_footprint.active_publication_id,
        first_footprint.active_publication_id
    );
    assert_ne!(
        edited_footprint.active_generation,
        first_footprint.active_generation
    );
    assert_eq!(
        edited_footprint.generation_rows,
        first_footprint.generation_rows + 1
    );
    assert_eq!(
        edited_footprint.generation_directories,
        first_footprint.generation_directories + 1
    );
    assert!(edited_footprint.artifact_bytes > first_footprint.artifact_bytes);
    let updated_query = stdout_json(&fixture.qgh(["query", "updated issue body", "--json"]));
    assert_eq!(updated_query["data"]["results"][0]["source_id"], source_id);
    let old_query =
        stdout_json(&fixture.qgh(["query", "round-trip through get before citation", "--json"]));
    assert_eq!(old_query["data"]["results"].as_array().unwrap().len(), 0);
}

#[test]
fn completed_sync_publishes_new_lexical_snapshot_when_embedding_refresh_fails() {
    let fixture = TestFixture::new("completed-sync-lexical-embedding-fallback");
    let server = EditingFakeGitHub::start();
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    server.set_mode(TARGET_REFRESH_DIFF);
    fixture.write_config_with_repos_and_embedding(
        &server.base_url,
        &["owner/repo"],
        r#"
[embedding]
provider = "local"
model_path = "/definitely/not/a/model"
file = "onnx/model.onnx"
pooling = "cls"
query_prefix = "query: "
quantization = "none"
"#,
    );
    let sync = fixture.qgh(["sync", "--json"]);
    assert_success(&sync);
    let sync_json = stdout_json(&sync);
    assert!(warning_codes(&sync_json)
        .iter()
        .any(|code| code.starts_with("embedding.sync_") && code.ends_with("_failed")));
    let reported_generation = sync_json["data"]["index"]["active_generation"]
        .as_i64()
        .unwrap();
    assert_eq!(
        fixture.active_retrieval_publication_generation(),
        reported_generation
    );

    let updated = fixture.qgh(["query", "updated issue body", "--json"]);
    assert_success(&updated);
    assert_eq!(
        stdout_json(&updated)["data"]["results"][0]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );
    let stale = fixture.qgh(["query", "round-trip through get before citation", "--json"]);
    assert_success(&stale);
    assert!(stdout_json(&stale)["data"]["results"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn sync_issue_requires_single_target_repo_scope() {
    let fixture = TestFixture::new("targeted-refresh-repo-required");
    fixture.write_config_with_repos("http://127.0.0.1:1", &["owner/repo", "other/repo"]);

    let refresh = fixture.qgh(["sync", "issue", "42", "--json"]);
    assert_eq!(refresh.status.code(), Some(2));
    let refresh_json = stdout_json(&refresh);
    assert_eq!(refresh_json["error"]["code"], "validation.repo_required");
    assert_eq!(refresh_json["error"]["details"]["profile_id"], "work");
    assert_eq!(refresh_json["error"]["details"]["repo_count"], 2);
}

#[test]
fn sync_issue_refreshes_target_issue_and_reconciles_comment_diff() {
    let fixture = TestFixture::new("targeted-refresh-comment-diff");
    let server = TargetedRefreshFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    let issue_id = "qgh://github.com/issue/I_kwDOISSUE1";
    let updated_comment_id = "qgh://github.com/issue-comment/IC_TARGET_1";
    let added_comment_id = "qgh://github.com/issue-comment/IC_TARGET_2";
    let deleted_comment_id = "qgh://github.com/issue-comment/IC_TARGET_3";
    assert_success(&fixture.qgh(["query", "deleteonlysentinel", "--json"]));

    server.set_mode(2);
    let refresh = fixture.qgh(["sync", "issue", "42", "--json"]);
    assert_success(&refresh);
    let refresh_json = stdout_json(&refresh);
    assert_eq!(refresh_json["data"]["sync_state"], "ok");
    assert_eq!(refresh_json["data"]["target"]["kind"], "issue");
    assert_eq!(refresh_json["data"]["target"]["repo"], "owner/repo");
    assert_eq!(refresh_json["data"]["target"]["issue_number"], 42);
    assert_eq!(refresh_json["data"]["lifecycle"]["status"], "active");
    assert_eq!(refresh_json["data"]["issues"]["fetched"], 1);
    assert_eq!(refresh_json["data"]["issues"]["upserted"], 1);
    assert_eq!(refresh_json["data"]["comments"]["fetched"], 2);
    assert_eq!(refresh_json["data"]["comments"]["added"], 1);
    assert_eq!(refresh_json["data"]["comments"]["updated"], 1);
    assert_eq!(refresh_json["data"]["comments"]["deleted"], 1);
    assert_eq!(refresh_json["data"]["comments"]["upserted"], 2);
    assert_eq!(refresh_json["data"]["index"]["dirty_task_count"], 0);

    let refreshed_issue = stdout_json(&fixture.qgh(["get", issue_id, "--json"]));
    assert!(refreshed_issue["data"]["source"]["body"]
        .as_str()
        .unwrap()
        .contains("targeted refresh issue body"));
    fixture.assert_source_version_count(issue_id, 2);
    fixture.assert_source_version_count(updated_comment_id, 2);
    fixture.assert_source_version_count(added_comment_id, 1);
    fixture.assert_tombstone_reason(deleted_comment_id, "deleted");

    let updated_comment = stdout_json(&fixture.qgh(["get", updated_comment_id, "--json"]));
    assert!(updated_comment["data"]["source"]["body"]
        .as_str()
        .unwrap()
        .contains("targeted refresh updated comment"));
    let added_comment = stdout_json(&fixture.qgh(["get", added_comment_id, "--json"]));
    assert!(added_comment["data"]["source"]["body"]
        .as_str()
        .unwrap()
        .contains("targeted refresh added comment"));

    let deleted_get = fixture.qgh(["get", deleted_comment_id, "--json"]);
    assert_eq!(deleted_get.status.code(), Some(4));
    assert_eq!(
        stdout_json(&deleted_get)["error"]["details"]["reason"],
        "deleted"
    );
    let deleted_query = fixture.qgh(["query", "deleteonlysentinel", "--json"]);
    assert_success(&deleted_query);
    assert_eq!(
        stdout_json(&deleted_query)["data"]["results"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let requests = server.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.starts_with("GET /repos/owner/repo/issues/42 ")),
        "targeted refresh must fetch the issue object directly: {requests:#?}"
    );
    assert!(
        requests.iter().any(|request| request
            .starts_with("GET /repos/owner/repo/issues/42/comments?per_page=100 ")),
        "targeted refresh must fetch the complete per-issue comment list without since: {requests:#?}"
    );
}

#[cfg(unix)]
#[test]
fn sync_issue_marks_every_confirmed_missing_comment_pending_before_batch_failure() {
    use std::os::unix::fs::symlink;

    let fixture = TestFixture::new("targeted-refresh-purge-batch-failure");
    let server = TargetedRefreshFakeGitHub::start();
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    let profile_dir = fixture.data_home.join("qgh/profiles/work");
    let index_root = profile_dir.join("tantivy");
    let saved_index_root = profile_dir.join("tantivy-before-batch-failure");
    fs::rename(&index_root, &saved_index_root).unwrap();
    let user_backup = fixture.root.join("user-created-batch-failure-backup");
    fs::create_dir_all(user_backup.join("generation-999")).unwrap();
    symlink(&user_backup, &index_root).unwrap();

    server.set_mode(TARGET_REFRESH_EMPTY_COMMENTS);
    let failed = fixture.qgh(["sync", "issue", "42", "--json"]);
    assert_eq!(failed.status.code(), Some(6));
    let failed_stdout = stdout_text(&failed);
    assert!(failed_stdout.contains("purge.retry_failed"));
    for private_marker in [
        "IC_TARGET_1",
        "IC_TARGET_3",
        "targeted refresh original comment",
        "deleteonlysentinel",
    ] {
        assert!(
            !failed_stdout.contains(private_marker),
            "purge error exposed private marker `{private_marker}`"
        );
    }

    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(status["data"]["purge"]["pending_count"], 2);
    assert_eq!(status["data"]["purge"]["target_kinds"], json!(["source"]));
    for source_id in [
        "qgh://github.com/issue-comment/IC_TARGET_1",
        "qgh://github.com/issue-comment/IC_TARGET_3",
    ] {
        let get = fixture.qgh(["get", source_id, "--json"]);
        assert_eq!(get.status.code(), Some(6));
        assert_eq!(stdout_json(&get)["error"]["code"], "purge.read_fenced");
    }

    fs::remove_file(&index_root).unwrap();
    fs::rename(&saved_index_root, &index_root).unwrap();
    let retry = fixture.qgh(["sync", "issue", "42", "--json"]);
    assert_success(&retry);
    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(status["data"]["purge"]["pending_count"], 0);
    fixture.assert_tombstone_reason("qgh://github.com/issue-comment/IC_TARGET_1", "deleted");
    fixture.assert_tombstone_reason("qgh://github.com/issue-comment/IC_TARGET_3", "deleted");
}

#[test]
fn sync_issue_human_output_reports_comment_diff_on_stdout_and_stderr() {
    let fixture = TestFixture::new("targeted-refresh-human-output");
    let server = TargetedRefreshFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    server.set_mode(TARGET_REFRESH_DIFF);
    let refresh = fixture.qgh(["sync", "issue", "42"]);
    assert_success(&refresh);
    let stdout = stdout_text(&refresh);
    assert!(!stdout.starts_with('{'));
    assert!(stdout.contains("qgh sync complete"));
    assert!(stdout.contains("comments: fetched 2, upserted 2"));
    assert!(stdout.contains("comment changes: added 1, updated 1, deleted 1"));
    let stderr = stderr_text(&refresh);
    assert!(stderr.contains("qgh sync issue: fetching repo=owner/repo issue_number=42"));
    assert!(stderr.contains("qgh sync issue: stored comments added=1 updated=1 deleted=1"));
}

#[test]
fn sync_issue_marks_deleted_issue_with_lifecycle_reason() {
    let fixture = TestFixture::new("targeted-refresh-deleted");
    let server = TargetedRefreshFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    server.set_mode(TARGET_REFRESH_DELETED);
    let refresh = fixture.qgh(["sync", "issue", "42", "--json"]);
    assert_success(&refresh);
    let refresh_json = stdout_json(&refresh);
    assert_eq!(refresh_json["data"]["lifecycle"]["status"], "deleted");
    assert_eq!(refresh_json["data"]["lifecycle"]["reason"], "deleted");
    assert_eq!(refresh_json["data"]["lifecycle"]["http_status"], 404);
    assert_eq!(refresh_json["data"]["issues"]["tombstoned"], 1);
    assert_eq!(refresh_json["data"]["comments"]["deleted"], 2);
    assert_eq!(refresh_json["data"]["comments"]["tombstoned"], 2);

    let issue_get = fixture.qgh(["get", "qgh://github.com/issue/I_kwDOISSUE1", "--json"]);
    assert_eq!(issue_get.status.code(), Some(4));
    assert_eq!(
        stdout_json(&issue_get)["error"]["details"]["reason"],
        "deleted"
    );
}

#[test]
fn sync_issue_marks_permission_loss_with_distinct_reason() {
    let fixture = TestFixture::new("targeted-refresh-permission");
    let server = TargetedRefreshFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    server.set_mode(TARGET_REFRESH_PERMISSION_LOSS);
    let refresh = fixture.qgh(["sync", "issue", "42", "--json"]);
    assert_success(&refresh);
    let refresh_json = stdout_json(&refresh);
    assert_eq!(
        refresh_json["data"]["lifecycle"]["status"],
        "permission_loss"
    );
    assert_eq!(
        refresh_json["data"]["lifecycle"]["reason"],
        "permission_loss"
    );
    assert_eq!(refresh_json["data"]["lifecycle"]["http_status"], 403);
    assert_eq!(refresh_json["data"]["issues"]["tombstoned"], 1);
    fixture.assert_successful_sync_run(
        refresh_json["data"]["sync_run_id"]
            .as_str()
            .expect("permission-loss sync_run_id"),
        "purge_successor",
    );

    let issue_get = fixture.qgh(["get", "qgh://github.com/issue/I_kwDOISSUE1", "--json"]);
    assert_eq!(issue_get.status.code(), Some(4));
    assert_eq!(
        stdout_json(&issue_get)["error"]["details"]["reason"],
        "permission_loss"
    );
}

#[test]
fn sync_issue_permission_loss_purges_entire_repo_and_preserves_unrelated_repo() {
    let fixture = TestFixture::new("targeted-refresh-repo-permission");
    let server = MultiRepoFakeGitHub::start();
    server.set_mode(MULTI_REPO_OWNER_TWO_ISSUES);
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    assert_success(&fixture.qgh(["sync", "--json"]));

    server.set_mode(MULTI_REPO_OWNER_PERMISSION_LOSS);
    let refresh = fixture.qgh(["sync", "issue", "42", "--repo", "owner/repo", "--json"]);
    assert_success(&refresh);
    let refresh_json = stdout_json(&refresh);
    assert_eq!(
        refresh_json["data"]["lifecycle"]["reason"],
        "permission_loss"
    );
    assert_eq!(refresh_json["data"]["issues"]["tombstoned"], 2);
    assert_eq!(refresh_json["data"]["comments"]["tombstoned"], 0);

    for source_id in [
        "qgh://github.com/issue/I_POLICY_OWNER",
        "qgh://github.com/issue/I_POLICY_OWNER_SECOND",
    ] {
        let removed = fixture.qgh(["get", source_id, "--json"]);
        assert_eq!(removed.status.code(), Some(4));
        assert_eq!(
            stdout_json(&removed)["error"]["details"]["reason"],
            "permission_loss"
        );
    }

    let unrelated = fixture.qgh(["get", "qgh://github.com/issue/I_POLICY_OTHER", "--json"]);
    assert_success(&unrelated);
    assert_eq!(
        stdout_json(&unrelated)["data"]["source"]["repo"],
        "other/repo"
    );
}

#[test]
fn full_sync_permission_loss_queues_then_continues_other_repositories() {
    let fixture = TestFixture::new("full-sync-repo-permission-short-circuit");
    let server = MultiRepoFakeGitHub::start();
    fixture.write_config_with_repos(&server.base_url, &["owner/repo", "other/repo"]);
    assert_success(&fixture.qgh(["sync", "--json"]));
    fixture.set_last_sync_age_seconds(3_600);
    let before =
        stdout_json(&fixture.qgh(["status", "--json"]))["data"]["sync"]["last_sync_at"].clone();

    server.clear_requests();
    server.set_mode(MULTI_REPO_OWNER_BULK_PERMISSION_LOSS);
    let sync = fixture.qgh(["sync", "--json"]);
    let requests = server.requests();
    assert_success(&sync);

    assert!(requests
        .iter()
        .any(|line| line.starts_with("GET /repos/owner/repo/issues?")));
    assert!(requests
        .iter()
        .any(|line| line.starts_with("GET /repos/owner/repo ")));
    assert!(requests
        .iter()
        .any(|line| line.starts_with("GET /repos/other/repo/issues?")));

    let after = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_ne!(after["data"]["sync"]["last_sync_at"], before);
    assert_eq!(after["data"]["purge"]["pending_count"], 0);

    let removed = fixture.qgh(["get", "qgh://github.com/issue/I_POLICY_OWNER", "--json"]);
    assert_eq!(removed.status.code(), Some(4));
    let unrelated = fixture.qgh(["get", "qgh://github.com/issue/I_POLICY_OTHER", "--json"]);
    assert_success(&unrelated);
}

#[test]
fn sync_issue_auth_failure_does_not_tombstone_local_sources() {
    let fixture = TestFixture::new("targeted-refresh-auth-failed");
    let server = TargetedRefreshFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    server.set_mode(TARGET_REFRESH_AUTH_FAILED);
    let refresh = fixture.qgh(["sync", "issue", "42", "--json"]);
    assert_eq!(refresh.status.code(), Some(3));
    assert_eq!(
        stdout_json(&refresh)["error"]["code"],
        "auth.token_unavailable"
    );
    fixture.assert_no_tombstone("qgh://github.com/issue/I_kwDOISSUE1");

    let local_query = fixture.qgh(["query", "BM25 issue body tracer", "--json"]);
    assert_success(&local_query);
    assert_eq!(
        stdout_json(&local_query)["data"]["results"][0]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );
}

#[test]
fn sync_issue_transient_failure_is_retryable_and_preserves_local_sources() {
    let fixture = TestFixture::new("targeted-refresh-transient");
    let server = TargetedRefreshFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    server.set_mode(TARGET_REFRESH_TRANSIENT);
    let refresh = fixture.qgh(["sync", "issue", "42", "--json"]);
    assert_eq!(refresh.status.code(), Some(3));
    let refresh_json = stdout_json(&refresh);
    assert_eq!(refresh_json["error"]["code"], "github.request_failed");
    assert_eq!(refresh_json["error"]["retryable"], true);
    assert!(refresh_json["error"]["hint"]
        .as_str()
        .unwrap()
        .contains("local content was not removed"));
    fixture.assert_no_tombstone("qgh://github.com/issue/I_kwDOISSUE1");

    let local_query = fixture.qgh(["query", "BM25 issue body tracer", "--json"]);
    assert_success(&local_query);
    assert_eq!(
        stdout_json(&local_query)["data"]["results"][0]["source_id"],
        "qgh://github.com/issue/I_kwDOISSUE1"
    );
}

#[test]
fn sync_issue_secondary_rate_limit_without_retry_after_does_not_tombstone() {
    let fixture = TestFixture::new("targeted-refresh-ambiguous-rate-limit");
    let server = TargetedRefreshFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    server.set_mode(TARGET_REFRESH_SECONDARY_RATE_LIMIT_NO_RETRY_AFTER);
    let refresh = fixture.qgh(["sync", "issue", "42", "--json"]);
    assert_eq!(refresh.status.code(), Some(5));
    let refresh_json = stdout_json(&refresh);
    assert_eq!(refresh_json["error"]["code"], "sync.backoff");
    assert_eq!(refresh_json["error"]["retryable"], true);
    assert_eq!(
        refresh_json["error"]["details"]["reason"],
        "secondary_rate_limit"
    );
    assert_eq!(
        refresh_json["error"]["details"]["scope"],
        "issue:owner/repo#42"
    );
    assert_eq!(
        refresh_json["error"]["details"]["retry_command"],
        "qgh sync issue 42 --repo owner/repo --profile work --json"
    );
    let persisted_backoff = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(
        persisted_backoff["data"]["sync"]["backoff"]["retry_command"],
        "qgh sync issue 42 --repo owner/repo --profile work --json"
    );
    fixture.assert_no_tombstone("qgh://github.com/issue/I_kwDOISSUE1");
}

#[test]
fn sync_issue_follows_transfer_alias_and_tombstones_old_issue() {
    let fixture = TestFixture::new("targeted-refresh-transfer");
    let server = TargetedRefreshFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    server.set_mode(TARGET_REFRESH_TRANSFER);
    let refresh = fixture.qgh(["sync", "issue", "42", "--json"]);
    assert_success(&refresh);
    let refresh_json = stdout_json(&refresh);
    assert_eq!(refresh_json["data"]["lifecycle"]["status"], "transferred");
    assert_eq!(refresh_json["data"]["lifecycle"]["reason"], "transferred");
    assert_eq!(
        refresh_json["data"]["lifecycle"]["alias_chain"][0],
        "/repos/owner/repo/issues/43"
    );
    assert_eq!(refresh_json["data"]["issues"]["upserted"], 1);
    assert_eq!(refresh_json["data"]["issues"]["tombstoned"], 1);
    assert_eq!(refresh_json["data"]["comments"]["added"], 1);
    assert_eq!(refresh_json["data"]["comments"]["deleted"], 2);

    let old_issue_get = fixture.qgh(["get", "qgh://github.com/issue/I_kwDOISSUE1", "--json"]);
    assert_eq!(old_issue_get.status.code(), Some(4));
    assert_eq!(
        stdout_json(&old_issue_get)["error"]["details"]["reason"],
        "transferred"
    );

    let transferred_query = fixture.qgh(["query", "transferredtargetsentinel", "--json"]);
    assert_success(&transferred_query);
    let transferred_json = stdout_json(&transferred_query);
    assert!(transferred_json["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|result| result["source_id"] == "qgh://github.com/issue/I_TARGET_TRANSFER"));
}

#[test]
fn sync_issue_queues_confirmed_transition_before_comment_state_read_failure() {
    let fixture = TestFixture::new("targeted-refresh-transition-before-read");
    let server = TargetedRefreshFakeGitHub::start();
    fixture.write_config(&server.base_url);
    server.set_mode(TARGET_REFRESH_INITIAL_WITH_TRANSFER_TARGET);
    assert_success(&fixture.qgh(["sync", "--json"]));
    fixture.corrupt_source_version_body_hash("qgh://github.com/issue-comment/IC_TARGET_TRANSFER");

    server.set_mode(TARGET_REFRESH_TRANSFER);
    let refresh = fixture.qgh(["sync", "issue", "42", "--json"]);
    assert_eq!(refresh.status.code(), Some(6));
    assert_eq!(stdout_json(&refresh)["error"]["code"], "storage.failure");
    let output = format!("{}{}", stdout_text(&refresh), stderr_text(&refresh));
    assert!(!output.contains("transferredtargetsentinel"));
    assert!(!output.contains("fixture-token"));

    let status = stdout_json(&fixture.qgh(["status", "--json"]));
    assert_eq!(status["data"]["purge"]["pending_count"], 1);
    assert_eq!(status["data"]["purge"]["target_kinds"], json!(["issue"]));
    assert_eq!(status["data"]["purge"]["retrieval_blocked"], true);

    let query = fixture.qgh(["query", "local read must remain fenced", "--json"]);
    assert_eq!(query.status.code(), Some(6));
    assert_eq!(stdout_json(&query)["error"]["code"], "purge.read_fenced");
}

#[test]
fn sync_issue_transfer_alias_cycle_is_guarded() {
    let fixture = TestFixture::new("targeted-refresh-transfer-cycle");
    let server = TargetedRefreshFakeGitHub::start();
    fixture.write_config(&server.base_url);

    assert_success(&fixture.qgh(["sync", "--json"]));
    server.set_mode(TARGET_REFRESH_TRANSFER_CYCLE);
    let refresh = fixture.qgh(["sync", "issue", "42", "--json"]);
    assert_eq!(refresh.status.code(), Some(2));
    let refresh_json = stdout_json(&refresh);
    assert_eq!(refresh_json["error"]["code"], "sync.transfer_cycle");
    assert!(
        refresh_json["error"]["details"]["alias_chain"]
            .as_array()
            .unwrap()
            .len()
            >= 2
    );
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

fn ghes_issue_payload() -> &'static str {
    r#"[
      {
        "id": 1001,
        "node_id": "I_kwDOISSUE1",
        "number": 42,
        "title": "GHES exact locator",
        "body": "Public GHES exact locator regression fixture.",
        "state": "open",
        "locked": false,
        "comments": 1,
        "html_url": "https://ghe.internal.example/owner/repo/issues/42",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:05Z",
        "closed_at": null,
        "user": {"login": "fixture-user"},
        "labels": [],
        "milestone": null,
        "assignees": []
      }
    ]"#
}

fn ghes_issue_comments_payload() -> &'static str {
    r#"[
      {
        "id": 5001,
        "node_id": "IC_kwDOCOMMENT1",
        "body": "Public GHES exact comment locator fixture.",
        "html_url": "https://ghe.internal.example/owner/repo/issues/42#issuecomment-5001",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-03T04:05:06Z",
        "user": {"login": "fixture-user"}
      }
    ]"#
}

fn issue_payload_with_terminal_control_words() -> &'static str {
    r#"[
      {
        "id": 1001,
        "node_id": "I_kwDOISSUE1",
        "number": 42,
        "title": "Source fidelity",
        "body": "first line\r\nnext: reproduce this\r\nrepair: preserve this\r\nlast line",
        "state": "open",
        "locked": false,
        "comments": 1,
        "html_url": "https://github.com/owner/repo/issues/42",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:05Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}],
        "milestone": null,
        "assignees": []
      }
    ]"#
}

fn limit_policy_issue_payload() -> &'static str {
    r#"[
      {
        "id": 4101,
        "node_id": "I_LIMIT_1",
        "number": 1,
        "title": "Policy limit one",
        "body": "repo policy limit tracer result one.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/1",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:01Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [],
        "milestone": null,
        "assignees": []
      },
      {
        "id": 4102,
        "node_id": "I_LIMIT_2",
        "number": 2,
        "title": "Policy limit two",
        "body": "repo policy limit tracer result two.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/2",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:02Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [],
        "milestone": null,
        "assignees": []
      },
      {
        "id": 4103,
        "node_id": "I_LIMIT_3",
        "number": 3,
        "title": "Policy limit three",
        "body": "repo policy limit tracer result three.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/3",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:03Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [],
        "milestone": null,
        "assignees": []
      },
      {
        "id": 4104,
        "node_id": "I_LIMIT_4",
        "number": 4,
        "title": "Policy limit four",
        "body": "repo policy limit tracer result four.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/4",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:04Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [],
        "milestone": null,
        "assignees": []
      },
      {
        "id": 4105,
        "node_id": "I_LIMIT_5",
        "number": 5,
        "title": "Policy limit five",
        "body": "repo policy limit tracer result five.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/5",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:05Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [],
        "milestone": null,
        "assignees": []
      }
    ]"#
}

fn rerank_depth_issue_payload() -> &'static str {
    let issues = (1..=12)
        .map(|number| {
            json!({
                "id": 5_000 + number,
                "node_id": format!("I_RERANK_DEPTH_{number}"),
                "number": number,
                "title": format!("Fixed rerank depth {number}"),
                "body": format!("fixed rerank depth tracer result {number}."),
                "state": "open",
                "locked": false,
                "comments": 0,
                "html_url": format!("https://github.com/owner/repo/issues/{number}"),
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": format!("2026-01-02T03:04:{number:02}Z"),
                "closed_at": null,
                "user": {"login": "bob"},
                "labels": [],
                "milestone": null,
                "assignees": []
            })
        })
        .collect::<Vec<_>>();
    Box::leak(serde_json::to_string(&issues).unwrap().into_boxed_str())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bm25PublicationFootprint {
    active_publication_id: i64,
    active_generation: i64,
    generation_rows: i64,
    generation_directories: usize,
    artifact_bytes: u64,
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

    fn write_config_with_host(&self, host: &str, api_base_url: &str) {
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "{host}"
api_base_url = "{api_base_url}"
web_base_url = "https://{host}"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn write_config_with_repos(&self, api_base_url: &str, repos: &[&str]) {
        self.write_config_with_repos_and_embedding(api_base_url, repos, "");
    }

    fn write_config_with_repos_and_embedding(
        &self,
        api_base_url: &str,
        repos: &[&str],
        embedding: &str,
    ) {
        let repos = repos
            .iter()
            .map(|repo| format!(r#""{repo}""#))
            .collect::<Vec<_>>()
            .join(", ");
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = [{repos}]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"

{embedding}
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn write_config_with_embedding(&self, api_base_url: &str, embedding: &str) {
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"

[embedding]
{embedding}
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn write_config_with_reranker(&self, api_base_url: &str, reranker: &str) {
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[reranker]
{reranker}

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn write_default_embedding_config(&self, api_base_url: &str) {
        self.write_config_with_embedding(
            api_base_url,
            r#"
provider = "local"
model = "hf:Snowflake/snowflake-arctic-embed-l-v2.0"
file = "onnx/model_quantized.onnx"
pooling = "cls"
query_prefix = "query: "
"#,
        );
    }

    #[cfg(feature = "fastembed-provider")]
    fn write_local_embedding_tokenizer_model(&self) -> PathBuf {
        use tokenizers::models::wordlevel::WordLevel;
        use tokenizers::pre_tokenizers::whitespace::Whitespace;
        use tokenizers::Tokenizer;

        let model_dir = self.data_home.join("embedding-model");
        fs::create_dir_all(model_dir.join("onnx")).unwrap();
        fs::write(model_dir.join("onnx/model.onnx"), b"not-used").unwrap();

        let mut vocab = HashMap::new();
        vocab.insert("[UNK]".to_string(), 0);
        let model = WordLevel::builder()
            .vocab(vocab.into_iter().collect())
            .unk_token("[UNK]".to_string())
            .build()
            .unwrap();
        let mut tokenizer = Tokenizer::new(model);
        tokenizer.with_pre_tokenizer(Some(Whitespace));
        tokenizer
            .save(model_dir.join("tokenizer.json"), false)
            .unwrap();
        fs::write(
            model_dir.join("config.json"),
            r#"{"hidden_size":4,"max_position_embeddings":32}"#,
        )
        .unwrap();
        fs::write(model_dir.join("special_tokens_map.json"), "{}").unwrap();
        fs::write(
            model_dir.join("tokenizer_config.json"),
            r#"{"model_max_length":32}"#,
        )
        .unwrap();
        fs::write(
            model_dir.join("modules.json"),
            r#"[
                {"type":"sentence_transformers.models.Normalize"},
                {"prompts":{"query":"query: ","document":""}}
            ]"#,
        )
        .unwrap();

        model_dir
    }

    #[cfg(feature = "fastembed-provider")]
    fn write_prepared_embedding_manifest(&self) -> (PathBuf, String) {
        let root = self.write_local_embedding_tokenizer_model();
        let declarations = [
            (ArtifactRole::OnnxModel, "onnx/model.onnx"),
            (ArtifactRole::Tokenizer, "tokenizer.json"),
            (ArtifactRole::Config, "config.json"),
            (ArtifactRole::SpecialTokensMap, "special_tokens_map.json"),
            (ArtifactRole::TokenizerConfig, "tokenizer_config.json"),
        ];
        let artifacts = declarations
            .into_iter()
            .map(|(role, relative_path)| {
                let bytes = fs::read(root.join(relative_path)).unwrap();
                ModelArtifactV1 {
                    role,
                    relative_path: relative_path.to_string(),
                    sha256: Sha256::digest(&bytes)
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                    byte_size: bytes.len() as u64,
                    external_initializer_name: None,
                }
            })
            .collect::<Vec<_>>();
        let manifest = ModelManifestV1 {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION.to_string(),
            preset_id: None,
            provider: ModelProviderKind::Fastembed,
            model_source: ModelSourceV1::Local {
                declared_id: "offline-fixture".to_string(),
            },
            artifacts,
            tokenizer: TokenizerKind::HfTokenizerJson,
            query_prefix: Some(DEFAULT_QUERY_PREFIX.to_string()),
            document_prefix: Some(String::new()),
            pooling: PoolingKind::Cls,
            normalization: NormalizationKind::L2,
            native_dimension: 3,
            output_dimension: 3,
            max_length: 32,
            quantization: QuantizationKind::None,
            context_template_version: qgh::context::METADATA_CONTEXT_TEMPLATE_VERSION.to_string(),
        };
        let manifest_hash = manifest.hash();
        let manifest_path = root.join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        (manifest_path, manifest_hash)
    }

    #[cfg(feature = "fastembed-provider")]
    fn prepared_snapshot_artifact(&self, manifest_hash: &str, relative_path: &str) -> PathBuf {
        self.cache_home
            .join("qgh/prepared-models/snapshots")
            .join(manifest_hash)
            .join(relative_path)
    }

    #[cfg(feature = "fastembed-provider")]
    fn single_prepared_request_alias(&self) -> PathBuf {
        let request_dir = self.cache_home.join("qgh/prepared-models/requests");
        let aliases = fs::read_dir(request_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(aliases.len(), 1);
        aliases.into_iter().next().unwrap()
    }

    #[cfg(feature = "fastembed-provider")]
    fn write_corrupt_prepared_request_alias(&self, options: &FastembedProviderOptions) {
        let identity = format!(
            "manifest={:?}\nmodel={:?}\nmodel_path={:?}\nfile={:?}\npooling={:?}\nquery_prefix={:?}\nquantization={:?}",
            options.manifest_path,
            options.model,
            options.model_path,
            options.file,
            options.pooling,
            options.query_prefix,
            options.quantization
        );
        let key = Sha256::digest(identity.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let requests = self.cache_home.join("qgh/prepared-models/requests");
        fs::create_dir_all(&requests).unwrap();
        fs::write(requests.join(format!("{key}.json")), b"{").unwrap();
    }

    fn write_config_repo_listing_comments(&self, api_base_url: &str) {
        self.write_config_repo_listing_comments_with_repo(api_base_url, "owner/repo");
    }

    fn write_config_repo_listing_comments_with_repo(&self, api_base_url: &str, repo: &str) {
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["{repo}"]
comments_mode = "repo_listing"

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn write_config_with_work_and_alt_profiles(&self, api_base_url: &str) {
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"

[profiles.alt]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["other/repo"]

[profiles.alt.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn write_config_with_work_and_alt_same_repo(&self, api_base_url: &str) {
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"

[profiles.alt]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.alt.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn write_config_with_work_two_repos_and_alt(
        &self,
        api_base_url: &str,
        include_removed_repo: bool,
    ) {
        let work_repos = if include_removed_repo {
            r#"["owner/repo", "other/repo"]"#
        } else {
            r#"["owner/repo"]"#
        };
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = {work_repos}

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"

[profiles.alt]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.alt.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn write_config_with_two_hosts_same_repo(&self, api_base_url: &str) {
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "a.example"
api_base_url = "{api_base_url}"
web_base_url = "https://a.example"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"

[profiles.alt]
host = "b.example"
api_base_url = "{api_base_url}"
web_base_url = "https://b.example"
repos = ["owner/repo"]

[profiles.alt.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn write_config_with_nine_hosts(&self, api_base_url: &str) -> Vec<String> {
        let profile_ids = (0..9)
            .map(|index| format!("p{index:02}"))
            .collect::<Vec<_>>();
        let profiles = profile_ids
            .iter()
            .enumerate()
            .map(|(index, profile_id)| {
                format!(
                    r#"
[profiles.{profile_id}]
host = "h{index:02}.example"
api_base_url = "{api_base_url}"
web_base_url = "https://h{index:02}.example"
repos = ["owner/repo"]

[profiles.{profile_id}.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
                )
            })
            .collect::<String>();
        fs::write(
            self.config_home.join("qgh/config.toml"),
            format!("schema_version = \"qgh.config.v1\"\n{profiles}"),
        )
        .unwrap();
        profile_ids
    }

    fn replace_profile_token_env(&self, profile_id: &str, token_env: &str) {
        let path = self.config_home.join("qgh/config.toml");
        let mut config = fs::read_to_string(&path).unwrap();
        let profile_start = config
            .find(&format!("[profiles.{profile_id}]"))
            .expect("profile must exist");
        let token_start = config[profile_start..]
            .find(&format!("[profiles.{profile_id}.token_source]"))
            .map(|offset| profile_start + offset)
            .expect("profile token source must exist");
        let env_start = config[token_start..]
            .find("env = \"")
            .map(|offset| token_start + offset + "env = \"".len())
            .expect("profile token env must exist");
        let env_end = config[env_start..]
            .find('"')
            .map(|offset| env_start + offset)
            .expect("profile token env must close");
        config.replace_range(env_start..env_end, token_env);
        fs::write(path, config).unwrap();
    }

    fn write_config_with_work_and_alt_profiles_stale(&self, api_base_url: &str) {
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["owner/repo"]
sync_max_age = "1s"

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"

[profiles.alt]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["other/repo"]
sync_max_age = "1s"

[profiles.alt.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn write_config_with_duplicate_owner_profiles(&self, api_base_url: &str) {
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.other]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.other.token_source]
type = "env"
env = "QGH_TEST_TOKEN"

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn write_config_with_missing_token_profile(&self, api_base_url: &str) {
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.strict]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.strict.token_source]
type = "env"
env = "QGH_MISSING_TOKEN"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn write_config_with_credential_store(&self, api_base_url: &str) {
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "credential_store"
service = "qgh"
account = "work"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
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

    fn write_config_with_reconcile_after_duration(&self, api_base_url: &str, duration: &str) {
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = ["owner/repo"]
reconcile_after = "{duration}"

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn write_config_with_freshness(
        &self,
        api_base_url: &str,
        query_max_age: Option<&str>,
        query_stale_behavior: Option<&str>,
        active_issue_max_age: Option<&str>,
    ) {
        self.write_config_with_freshness_and_repos(
            api_base_url,
            &["owner/repo"],
            query_max_age,
            query_stale_behavior,
            active_issue_max_age,
        );
    }

    fn write_config_with_freshness_and_repos(
        &self,
        api_base_url: &str,
        repos: &[&str],
        query_max_age: Option<&str>,
        query_stale_behavior: Option<&str>,
        active_issue_max_age: Option<&str>,
    ) {
        let repos = repos
            .iter()
            .map(|repo| format!(r#""{repo}""#))
            .collect::<Vec<_>>()
            .join(", ");
        let query_max_age = query_max_age
            .map(|duration| format!(r#"query_max_age = "{duration}""#))
            .unwrap_or_default();
        let query_stale_behavior = query_stale_behavior
            .map(|behavior| format!(r#"query_stale_behavior = "{behavior}""#))
            .unwrap_or_default();
        let active_issue_max_age = active_issue_max_age
            .map(|duration| format!(r#"active_issue_max_age = "{duration}""#))
            .unwrap_or_default();
        let config = format!(
            r#"
schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{api_base_url}"
web_base_url = "https://github.com"
repos = [{repos}]
{query_max_age}
{query_stale_behavior}
{active_issue_max_age}

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn init_git_worktree_with_repo_policy(&self, repo: &str) -> PathBuf {
        let nested = self.init_git_worktree();
        self.write_repo_policy(repo);
        nested
    }

    fn init_git_worktree_with_origin(&self, remote: &str) -> PathBuf {
        let nested = self.init_git_worktree();
        let remote_add = Command::new("git")
            .args(["remote", "add", "origin", remote])
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert_success(&remote_add);
        nested
    }

    fn init_git_worktree(&self) -> PathBuf {
        let init = Command::new("git")
            .arg("init")
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert_success(&init);
        let nested = self.root.join("nested/worktree/path");
        fs::create_dir_all(&nested).unwrap();
        nested
    }

    fn write_repo_policy(&self, repo: &str) {
        self.write_repo_policy_with_query_limit(repo, 10);
    }

    fn write_repo_policy_with_query_limit(&self, repo: &str, limit: usize) {
        let policy = format!(
            r#"
schema_version = "qgh.repo.v1"

[repo]
github = "{repo}"

[defaults]
scope = "repo"
state = "all"
source_types = ["issue", "issue_comment"]
labels = []

[query]
limit = {limit}
"#
        );
        fs::write(self.root.join(".qgh.toml"), policy).unwrap();
    }

    fn write_repo_policy_with_freshness(
        &self,
        repo: &str,
        max_age: Option<&str>,
        stale_behavior: Option<&str>,
        active_issue_max_age: Option<&str>,
    ) {
        let max_age = max_age
            .map(|duration| format!(r#"max_age = "{duration}""#))
            .unwrap_or_default();
        let stale_behavior = stale_behavior
            .map(|behavior| format!(r#"stale_behavior = "{behavior}""#))
            .unwrap_or_default();
        let active_issue_max_age = active_issue_max_age
            .map(|duration| format!(r#"active_issue_max_age = "{duration}""#))
            .unwrap_or_default();
        let policy = format!(
            r#"
schema_version = "qgh.repo.v1"

[repo]
github = "{repo}"

[defaults]
scope = "repo"
state = "all"
source_types = ["issue", "issue_comment"]
labels = []

[query]
limit = 10
{max_age}
{stale_behavior}
{active_issue_max_age}
"#
        );
        fs::write(self.root.join(".qgh.toml"), policy).unwrap();
    }

    fn mark_profile_sync_stale(&self, profile_id: &str) {
        let db_path = self
            .data_home
            .join(format!("qgh/profiles/{profile_id}/qgh.sqlite3"));
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "UPDATE sync_runs
             SET completed_at = '2000-01-01T00:00:00Z'
             WHERE completed_successfully = 1 AND snapshot_kind = 'remote_sync'",
            [],
        )
        .unwrap();
    }

    fn set_profile_rate_budget(&self, profile_id: &str, limit: i64, remaining: i64) {
        let db_path = self
            .data_home
            .join(format!("qgh/profiles/{profile_id}/qgh.sqlite3"));
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "DELETE FROM rate_budget_observations WHERE lower(host) = 'github.com'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rate_budget_observations
                (host, resource_key, resource, limit_value, remaining, reset_at, observed_at, best_effort)
             VALUES ('github.com', 'core', 'core', ?1, ?2, ?3, ?4, 1)",
            rusqlite::params![
                limit,
                remaining,
                (Utc::now() + Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true),
                Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
            ],
        )
        .unwrap();
    }

    fn clear_profile_rate_budget(&self, profile_id: &str) {
        let db_path = self
            .data_home
            .join(format!("qgh/profiles/{profile_id}/qgh.sqlite3"));
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "DELETE FROM rate_budget_observations WHERE lower(host) = 'github.com'",
            [],
        )
        .unwrap();
    }

    fn set_profile_unknown_resource_rate_budget(
        &self,
        profile_id: &str,
        limit: i64,
        remaining: i64,
    ) {
        let db_path = self
            .data_home
            .join(format!("qgh/profiles/{profile_id}/qgh.sqlite3"));
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "DELETE FROM rate_budget_observations WHERE lower(host) = 'github.com'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rate_budget_observations
                (host, resource_key, resource, limit_value, remaining, reset_at, observed_at, best_effort)
             VALUES ('github.com', '', NULL, ?1, ?2, ?3, ?4, 1)",
            rusqlite::params![
                limit,
                remaining,
                (Utc::now() + Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true),
                Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
            ],
        )
        .unwrap();
    }

    fn set_profile_backoff(&self, profile_id: &str) {
        let db_path = self
            .data_home
            .join(format!("qgh/profiles/{profile_id}/qgh.sqlite3"));
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "INSERT INTO sync_backoff_state
                (id, reason, scope, retry_after_seconds, reset_at, observed_at, last_successful_sync, retry_command)
             VALUES (1, 'secondary_rate_limit', 'host', 3600, ?1, ?2, NULL, ?3)
             ON CONFLICT(id) DO UPDATE SET
                reason = excluded.reason,
                scope = excluded.scope,
                retry_after_seconds = excluded.retry_after_seconds,
                reset_at = excluded.reset_at,
                observed_at = excluded.observed_at,
                retry_command = excluded.retry_command",
            rusqlite::params![
                (Utc::now() + Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true),
                Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                format!("qgh sync --all --profile {profile_id}")
            ],
        )
        .unwrap();
    }

    fn clear_profile_backoff(&self, profile_id: &str) {
        let db_path = self
            .data_home
            .join(format!("qgh/profiles/{profile_id}/qgh.sqlite3"));
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute("DELETE FROM sync_backoff_state", []).unwrap();
    }

    fn qgh<const N: usize>(&self, args: [&str; N]) -> Output {
        let mut cmd = self.base_command();
        cmd.args(["--profile", "work"]).args(args);
        cmd.output().unwrap()
    }

    #[cfg(feature = "vector-search")]
    fn qgh_with_document_vectors<const N: usize>(
        &self,
        args: [&str; N],
        document_vectors: &Value,
    ) -> Output {
        let mut cmd = self.base_command();
        cmd.env(
            "QGH_TEST_EMBEDDING_DOCUMENT_VECTORS",
            document_vectors.to_string(),
        )
        .args(["--profile", "work"])
        .args(args);
        cmd.output().unwrap()
    }

    fn qgh_with_rerank_scores<const N: usize>(&self, args: [&str; N], scores: &Value) -> Output {
        let mut cmd = self.base_command();
        cmd.env("QGH_TEST_RERANK_SCORES", scores.to_string())
            .args(["--profile", "work"])
            .args(args);
        cmd.output().unwrap()
    }

    #[cfg(feature = "vector-search")]
    fn initialize_embedding_schema_for_test(&self) {
        let output = self.qgh_with_document_vectors(["embed", "--force", "--json"], &json!({}));
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            stdout_json(&output)["error"]["code"],
            "embedding.test_vectors_empty"
        );
    }

    fn qgh_in<const N: usize>(&self, cwd: &Path, args: [&str; N]) -> Output {
        let mut cmd = self.base_command();
        cmd.current_dir(cwd).args(["--profile", "work"]).args(args);
        cmd.output().unwrap()
    }

    fn qgh_in_profile<const N: usize>(&self, cwd: &Path, profile: &str, args: [&str; N]) -> Output {
        let mut cmd = self.base_command();
        cmd.current_dir(cwd).args(["--profile", profile]).args(args);
        cmd.output().unwrap()
    }

    fn qgh_without_profile<const N: usize>(&self, args: [&str; N]) -> Output {
        let mut cmd = self.base_command();
        cmd.args(args);
        cmd.output().unwrap()
    }

    fn qgh_without_profile_in<const N: usize>(&self, cwd: &Path, args: [&str; N]) -> Output {
        let mut cmd = self.base_command();
        cmd.current_dir(cwd).args(args);
        cmd.output().unwrap()
    }

    fn qgh_without_profile_in_with_stdin<const N: usize>(
        &self,
        cwd: &Path,
        args: [&str; N],
        stdin_text: &str,
    ) -> Output {
        let mut cmd = self.base_command();
        cmd.current_dir(cwd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(stdin_text.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn mcp<const N: usize>(&self, messages: [Value; N]) -> Output {
        self.mcp_in(&self.root, Some("work"), messages)
    }

    fn mcp_without_profile_in<const N: usize>(&self, cwd: &Path, messages: [Value; N]) -> Output {
        self.mcp_in(cwd, None, messages)
    }

    fn mcp_in<const N: usize>(
        &self,
        cwd: &Path,
        profile: Option<&str>,
        messages: [Value; N],
    ) -> Output {
        let mut cmd = self.base_command();
        cmd.current_dir(cwd);
        if let Some(profile) = profile {
            cmd.args(["--profile", profile]);
        }
        cmd.arg("mcp")
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
            .env_remove("QGH_PROFILE")
            .env_remove("RUST_LOG")
            .current_dir(&self.root);
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

    fn assert_sqlite_chunks_empty(&self) {
        let chunk_count = self.sqlite_chunk_count();
        assert_eq!(
            chunk_count, 0,
            "BM25-only sync must not materialize chunks without [embedding]"
        );
        assert_eq!(
            self.sqlite_vector_table_count(),
            0,
            "BM25-only sync must not create sqlite-vec tables without [embedding]"
        );
        assert_eq!(
            self.sqlite_embedding_schema_table_count(),
            0,
            "BM25-only sync must not migrate embedding schema without [embedding]"
        );
    }

    fn sqlite_embedding_schema_table_count(&self) -> i64 {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT count(*)
             FROM sqlite_schema
             WHERE type = 'table'
               AND name IN ('chunks', 'embedding_fingerprints', 'chunk_embeddings')",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn sqlite_chunk_count(&self) -> i64 {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let chunks_table_exists: bool = conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'chunks'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if !chunks_table_exists {
            return 0;
        }
        conn.query_row("SELECT count(*) FROM chunks", [], |row| row.get(0))
            .unwrap()
    }

    fn sqlite_vector_table_count(&self) -> i64 {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT count(*)
             FROM sqlite_schema
             WHERE type = 'table' AND name LIKE 'chunk_embedding_vectors%'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[cfg(feature = "vector-search")]
    fn insert_chunk_for_source(&self, source_id: &str, body: &str) -> i64 {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let source_version_id: i64 = conn
            .query_row(
                "SELECT coalesce(im.latest_version_id, cm.latest_version_id)
                 FROM source_entities se
                 LEFT JOIN issue_metadata im ON im.source_id = se.source_id
                 LEFT JOIN comment_metadata cm ON cm.source_id = se.source_id
                 WHERE se.source_id = ?1 AND se.lifecycle_state = 'active'",
                [source_id],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO chunks (source_id, source_version_id, body)
             VALUES (?1, ?2, ?3)",
            (source_id, source_version_id, body),
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[cfg(feature = "vector-search")]
    fn sqlite_chunk_ids_for_source(&self, source_id: &str) -> Vec<i64> {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM chunks WHERE source_id = ?1 ORDER BY source_version_id, id")
            .unwrap();
        stmt.query_map([source_id], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[cfg(feature = "vector-search")]
    fn insert_active_embedding_fingerprint(&self, model_id: &str) {
        self.insert_active_embedding_fingerprint_with_revision(model_id, "fixture-sha");
    }

    #[cfg(feature = "vector-search")]
    fn insert_matching_active_embedding_fingerprint(&self) {
        self.insert_active_embedding_fingerprint_with_revision(
            DEFAULT_HF_MODEL_ID,
            DEFAULT_HF_MODEL_REVISION,
        );
    }

    #[cfg(feature = "vector-search")]
    fn corrupt_active_embedding_fingerprint_json(&self) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "UPDATE embedding_fingerprints
             SET fingerprint_json = '{not-json'
             WHERE active = 1",
            [],
        )
        .unwrap();
    }

    #[cfg(feature = "vector-search")]
    fn create_mismatched_vector_table(&self) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "CREATE TABLE chunk_embedding_vectors (embedding \"float[2]\")",
            [],
        )
        .unwrap();
    }

    #[cfg(feature = "vector-search")]
    fn insert_embedding_for_chunk(&self, chunk_id: i64) {
        self.insert_embedding_record_for_chunk(chunk_id, true);
    }

    #[cfg(feature = "vector-search")]
    fn insert_embedding_metadata_for_chunk(&self, chunk_id: i64) {
        self.insert_embedding_record_for_chunk(chunk_id, false);
    }

    #[cfg(feature = "vector-search")]
    fn insert_embedding_record_for_chunk(&self, chunk_id: i64, include_vector: bool) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        if include_vector {
            register_test_sqlite_vec(&conn);
            conn.execute(
                "CREATE VIRTUAL TABLE IF NOT EXISTS chunk_embedding_vectors
                 USING vec0(embedding float[3])",
                [],
            )
            .unwrap();
        }
        let fingerprint_id: i64 = conn
            .query_row(
                "SELECT id FROM embedding_fingerprints WHERE active = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO chunk_embeddings
                (chunk_id, fingerprint_id, vector_json, embedded_at)
             VALUES (?1, ?2, '[0.1,0.2,0.3]', '2026-01-02T00:00:00Z')",
            (chunk_id, fingerprint_id),
        )
        .unwrap();
        if include_vector {
            let vector_blob = [0.1_f32, 0.2, 0.3]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>();
            conn.execute(
                "INSERT INTO chunk_embedding_vectors(rowid, embedding) VALUES (?1, ?2)",
                rusqlite::params![chunk_id, vector_blob],
            )
            .unwrap();
        }
    }

    #[cfg(feature = "vector-search")]
    fn insert_active_embedding_fingerprint_with_revision(
        &self,
        model_id: &str,
        model_revision: &str,
    ) {
        let fingerprint = EmbeddingFingerprintSeed {
            provider: "local".to_string(),
            model_id: model_id.to_string(),
            model_revision: model_revision.to_string(),
            pooling: PoolingKind::Cls,
            query_prefix: DEFAULT_QUERY_PREFIX.to_string(),
        }
        .with_dimension(3);
        let fingerprint_hash = fingerprint.hash();
        let fingerprint_json = serde_json::to_string(&fingerprint).unwrap();
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute("UPDATE embedding_fingerprints SET active = 0", [])
            .unwrap();
        conn.execute(
            "INSERT INTO embedding_fingerprints
                (fingerprint_hash, fingerprint_json, provider, model_id, model_revision,
                 dimension, pooling, query_prefix, chunker_version, source_schema_version,
                 created_at, active)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, '2026-01-02T00:00:00Z', 1)",
            rusqlite::params![
                fingerprint_hash,
                fingerprint_json,
                fingerprint.provider,
                fingerprint.model_id,
                fingerprint.model_revision,
                fingerprint.dimension as i64,
                fingerprint.pooling.as_str(),
                fingerprint.query_prefix,
                fingerprint.chunker_version,
                fingerprint.source_schema_version
            ],
        )
        .unwrap();
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

    fn assert_tombstone_reason(&self, source_id: &str, expected: &str) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let reason: String = conn
            .query_row(
                "SELECT reason FROM tombstones WHERE source_id = ?1",
                [source_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reason, expected);
    }

    fn assert_no_tombstone(&self, source_id: &str) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM tombstones WHERE source_id = ?1",
                [source_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    fn install_repo_comment_upsert_failure_trigger(&self) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_repo_comment_upsert
             BEFORE UPDATE ON sync_runs
             WHEN NEW.fetched_comment_count > OLD.fetched_comment_count
             BEGIN
                 SELECT RAISE(ABORT, 'fixture repo-comment upsert failure');
             END;",
        )
        .unwrap();
    }

    fn corrupt_source_version_body_hash(&self, source_id: &str) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "UPDATE source_versions SET body_hash = X'80' WHERE source_id = ?1",
            [source_id],
        )
        .unwrap();
    }

    fn set_issue_metadata_repo_casing(&self, source_id: &str, repo: &str) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "UPDATE issue_metadata SET repo = ?2 WHERE source_id = ?1",
            (source_id, repo),
        )
        .unwrap();
    }

    fn clear_retrieval_publication(&self) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute("DELETE FROM retrieval_publication_pointer", [])
            .unwrap();
        conn.execute("UPDATE retrieval_publications SET active = 0", [])
            .unwrap();
        conn.execute("UPDATE index_generations SET active = 0", [])
            .unwrap();
    }

    fn mark_successor_repair_required(&self) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let epoch: String = conn
            .query_row(
                "SELECT value FROM profile_meta WHERE key = 'content_write_epoch'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        for (key, value) in [
            ("successor_repair_required", "1"),
            ("successor_repair_requested_epoch", epoch.as_str()),
            ("successor_repair_reason", "purge"),
        ] {
            conn.execute(
                "INSERT INTO profile_meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                (key, value),
            )
            .unwrap();
        }
    }

    fn assert_successful_sync_run(&self, sync_run_id: &str, expected_snapshot_kind: &str) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let (completed, snapshot_kind): (i64, String) = conn
            .query_row(
                "SELECT completed_successfully, snapshot_kind FROM sync_runs WHERE id = ?1",
                [sync_run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(completed, 1);
        assert_eq!(snapshot_kind, expected_snapshot_kind);
    }

    fn active_retrieval_publication_generation(&self) -> i64 {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT rp.tantivy_generation
             FROM retrieval_publication_pointer p
             JOIN retrieval_publications rp ON rp.publication_id = p.publication_id
             WHERE p.id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn active_retrieval_publication_id(&self) -> i64 {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT publication_id FROM retrieval_publication_pointer WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn bm25_publication_footprint(&self) -> Bm25PublicationFootprint {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let (active_publication_id, active_generation, generation_rows): (i64, i64, i64) = conn
            .query_row(
                "SELECT pointer.publication_id, publication.tantivy_generation,
                        (SELECT count(*) FROM index_generations)
                 FROM retrieval_publication_pointer pointer
                 JOIN retrieval_publications publication
                   ON publication.publication_id = pointer.publication_id
                 WHERE pointer.id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let index_root = self.data_home.join("qgh/profiles/work/tantivy");
        let generation_paths = fs::read_dir(index_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.is_dir()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("generation-"))
            })
            .collect::<Vec<_>>();
        let artifact_bytes = generation_paths
            .iter()
            .map(|path| directory_file_bytes(path))
            .sum();
        Bm25PublicationFootprint {
            active_publication_id,
            active_generation,
            generation_rows,
            generation_directories: generation_paths.len(),
            artifact_bytes,
        }
    }

    fn seed_open_repair_candidates(&self, source_id: &str, pending_purge: bool) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "DELETE FROM schema_migrations WHERE version = ?1",
            ["qgh.tantivy.commit_inventory.v1"],
        )
        .unwrap();
        conn.execute(
            "UPDATE index_generations
             SET source_inventory_hash = ?1
             WHERE active = 1",
            ["0000000000000000000000000000000000000000000000000000000000000000"],
        )
        .unwrap();
        if pending_purge {
            conn.execute(
                "INSERT INTO purge_requests
                    (target_kind, target_value, trigger, purge_pending,
                     current_stage, failure_stage, completion_ready, created_at, updated_at)
                 VALUES ('source', ?1, 'confirmed_delete', 1,
                         'secure_delete', NULL, 0,
                         '2026-01-04T00:00:00Z', '2026-01-04T00:00:00Z')",
                [source_id],
            )
            .unwrap();
        }
    }

    fn open_repair_state(&self, source_id: &str) -> Value {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let (
            pointer_count,
            pointer_id,
            active_publications,
            active_generations,
            inventory_migration,
            guarded_sources,
            lifecycle_state,
            content_write_epoch,
        ): (i64, i64, i64, i64, i64, i64, String, String) = conn
            .query_row(
                "SELECT
                    (SELECT count(*) FROM retrieval_publication_pointer),
                    coalesce((SELECT max(publication_id)
                              FROM retrieval_publication_pointer), 0),
                    (SELECT count(*) FROM retrieval_publications WHERE active = 1),
                    (SELECT count(*) FROM index_generations WHERE active = 1),
                    (SELECT count(*) FROM schema_migrations WHERE version =
                        'qgh.tantivy.commit_inventory.v1'),
                    (SELECT count(*) FROM purge_target_sources WHERE source_id = ?1),
                    (SELECT lifecycle_state FROM source_entities WHERE source_id = ?1),
                    (SELECT value FROM profile_meta WHERE key = 'content_write_epoch')",
                [source_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .unwrap();
        json!({
            "pointer_count": pointer_count,
            "pointer_id": pointer_id,
            "active_publications": active_publications,
            "active_generations": active_generations,
            "inventory_migration": inventory_migration,
            "guarded_sources": guarded_sources,
            "lifecycle_state": lifecycle_state,
            "content_write_epoch": content_write_epoch
        })
    }

    fn remove_active_tantivy_generation(&self) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let path: String = conn
            .query_row(
                "SELECT generation.path
                 FROM retrieval_publication_pointer pointer
                 JOIN retrieval_publications publication
                   ON publication.publication_id = pointer.publication_id
                 JOIN index_generations generation
                   ON generation.generation = publication.tantivy_generation
                 WHERE pointer.id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        fs::remove_dir_all(path).unwrap();
    }

    fn active_retrieval_publication_count(&self) -> i64 {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT count(*) FROM retrieval_publication_pointer",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn embedding_sync_run_reference_count(&self, sync_run_id: &str) -> i64 {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let table_exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'embedding_generations'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if table_exists == 0 {
            return 0;
        }
        conn.query_row(
            "SELECT count(*) FROM embedding_generations
             WHERE source_sync_run_id = ?1 OR source_snapshot_hash = ?1",
            [sync_run_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn set_last_sync_age_seconds(&self, age_seconds: i64) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let timestamp = (Utc::now() - Duration::seconds(age_seconds))
            .to_rfc3339_opts(SecondsFormat::Secs, true);
        let changed = conn
            .execute(
                "UPDATE sync_runs SET started_at = ?1, completed_at = ?1",
                [&timestamp],
            )
            .unwrap();
        assert!(changed >= 1);
        let changed = conn
            .execute(
                "UPDATE repository_sync_state SET last_successful_sync_at = ?1",
                [&timestamp],
            )
            .unwrap();
        assert!(changed >= 1);
    }

    fn set_repo_sync_age_seconds(&self, repo: &str, age_seconds: i64) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let timestamp = (Utc::now() - Duration::seconds(age_seconds))
            .to_rfc3339_opts(SecondsFormat::Secs, true);
        conn.execute(
            "INSERT INTO repository_sync_state (repo, last_successful_sync_at)
             VALUES (?1, ?2)
             ON CONFLICT(repo) DO UPDATE SET last_successful_sync_at = excluded.last_successful_sync_at",
            (repo, timestamp),
        )
        .unwrap();
    }

    fn insert_profile_sync_run_now(&self, id: &str) {
        let db_path = self.data_home.join("qgh/profiles/work/qgh.sqlite3");
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        conn.execute(
            "INSERT INTO sync_runs
                (id, started_at, completed_at, fetched_issue_count, upserted_issue_count, fetched_comment_count, upserted_comment_count, skipped_pull_request_count)
             VALUES (?1, ?2, ?2, 0, 0, 0, 0, 0)",
            (id, timestamp),
        )
        .unwrap();
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
                self.data_home.join("qgh/profiles/work/sync.lock"),
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

fn directory_file_bytes(directory: &Path) -> u64 {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .map(|path| {
            if path.is_dir() {
                directory_file_bytes(&path)
            } else {
                fs::metadata(path).unwrap().len()
            }
        })
        .sum()
}

#[cfg(feature = "vector-search")]
fn register_test_sqlite_vec(conn: &rusqlite::Connection) {
    type SqliteVecEntryPoint = unsafe extern "C" fn(
        db: *mut rusqlite::ffi::sqlite3,
        pz_err_msg: *mut *mut c_char,
        p_api: *const rusqlite::ffi::sqlite3_api_routines,
    ) -> c_int;

    let entry_point = unsafe {
        std::mem::transmute::<unsafe extern "C" fn(), SqliteVecEntryPoint>(
            sqlite_vec::sqlite3_vec_init,
        )
    };
    let rc = unsafe { entry_point(conn.handle(), std::ptr::null_mut(), std::ptr::null()) };
    assert_eq!(rc, rusqlite::ffi::SQLITE_OK);
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
        Self::start_with_comments(issue_payload, issue_comments_payload())
    }

    fn start_with_comments(issue_payload: &'static str, comments_payload: &'static str) -> Self {
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
                    Ok(stream) => {
                        handle_connection(stream, issue_payload, comments_payload, &thread_requests)
                    }
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

struct SlowFirstRequestFakeGitHub {
    base_url: String,
    accepted_requests: Arc<AtomicUsize>,
    first_request_release: Arc<(Mutex<bool>, Condvar)>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SlowFirstRequestFakeGitHub {
    fn start(issue_payload: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let accepted_requests = Arc::new(AtomicUsize::new(0));
        let first_request_release = Arc::new((Mutex::new(false), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_requests = Arc::clone(&accepted_requests);
        let thread_release = Arc::clone(&first_request_release);
        let thread_stop = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            let requests = Arc::new(Mutex::new(Vec::new()));
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(stream) => {
                        let request_number = thread_requests.fetch_add(1, Ordering::SeqCst);
                        if request_number == 0 {
                            let (released, condition) = &*thread_release;
                            let mut released = released.lock().unwrap();
                            while !*released {
                                released = condition.wait(released).unwrap();
                            }
                        }
                        handle_connection(stream, issue_payload, "[]", &requests);
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url,
            accepted_requests,
            first_request_release,
            stop,
            handle: Some(handle),
        }
    }

    fn wait_until_first_request(&self) {
        let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
        while self.accepted_requests.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "first sync did not reach fake GitHub"
            );
            thread::sleep(StdDuration::from_millis(10));
        }
    }

    fn release_first_request(&self) {
        let (released, condition) = &*self.first_request_release;
        *released.lock().unwrap() = true;
        condition.notify_all();
    }

    fn accepted_request_count(&self) -> usize {
        self.accepted_requests.load(Ordering::SeqCst)
    }
}

impl Drop for SlowFirstRequestFakeGitHub {
    fn drop(&mut self) {
        self.release_first_request();
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct MultiRepoFakeGitHub {
    base_url: String,
    mode: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MultiRepoFakeGitHub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);
        let mode = Arc::new(AtomicUsize::new(MULTI_REPO_ACTIVE));
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
                        handle_multi_repo_connection(stream, &thread_mode, &thread_requests)
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

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn clear_requests(&self) {
        self.requests.lock().unwrap().clear();
    }

    fn set_mode(&self, mode: usize) {
        self.mode.store(mode, Ordering::SeqCst);
    }
}

impl Drop for MultiRepoFakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

const MULTI_REPO_ACTIVE: usize = 1;
const MULTI_REPO_OWNER_TWO_ISSUES: usize = 2;
const MULTI_REPO_OWNER_PERMISSION_LOSS: usize = 3;
const MULTI_REPO_OWNER_BULK_PERMISSION_LOSS: usize = 4;
const MULTI_REPO_OWNER_SECONDARY_RATE_LIMIT: usize = 5;
const MULTI_REPO_MISSING_RATE_HEADERS: usize = 6;

fn handle_multi_repo_connection(
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
    {
        if mode == MULTI_REPO_OWNER_SECONDARY_RATE_LIMIT {
            (
                "403 Forbidden",
                r#"{"message":"secondary rate limit exceeded"}"#,
            )
        } else if mode == MULTI_REPO_OWNER_BULK_PERMISSION_LOSS {
            ("404 Not Found", r#"{"message":"not found"}"#)
        } else if mode == MULTI_REPO_OWNER_TWO_ISSUES {
            ("200 OK", multi_repo_owner_two_issue_payload())
        } else {
            ("200 OK", multi_repo_owner_issue_payload())
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42/comments?") {
        ("200 OK", "[]")
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42 ") {
        if mode == MULTI_REPO_OWNER_BULK_PERMISSION_LOSS {
            ("404 Not Found", r#"{"message":"not found"}"#)
        } else if mode == MULTI_REPO_OWNER_PERMISSION_LOSS {
            ("403 Forbidden", r#"{"message":"resource not accessible"}"#)
        } else {
            ("200 OK", multi_repo_owner_issue_object_payload())
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/43/comments?") {
        ("200 OK", "[]")
    } else if request_line.starts_with("GET /repos/owner/repo/issues/43 ") {
        ("200 OK", multi_repo_owner_second_issue_object_payload())
    } else if request_line.starts_with("GET /repos/owner/repo ") {
        if mode == MULTI_REPO_OWNER_BULK_PERMISSION_LOSS {
            ("404 Not Found", r#"{"message":"not found"}"#)
        } else if mode == MULTI_REPO_OWNER_PERMISSION_LOSS {
            ("403 Forbidden", r#"{"message":"resource not accessible"}"#)
        } else {
            ("200 OK", r#"{"full_name":"owner/repo"}"#)
        }
    } else if request_line.starts_with("GET /repos/other/repo/issues?")
        && request_line.contains("state=all")
    {
        ("200 OK", multi_repo_other_issue_payload())
    } else if request_line.starts_with("GET /repos/other/repo/issues/7/comments?") {
        ("200 OK", "[]")
    } else if request_line.starts_with("GET /repos/other/repo/issues/7 ") {
        ("200 OK", multi_repo_other_issue_object_payload())
    } else if request_line.starts_with("GET /repos/other/repo ") {
        ("200 OK", r#"{"full_name":"other/repo"}"#)
    } else {
        ("404 Not Found", r#"{"message":"not found"}"#)
    };
    let rate_headers = if mode == MULTI_REPO_OWNER_SECONDARY_RATE_LIMIT
        && request_line.starts_with("GET /repos/owner/repo/issues?")
    {
        "retry-after: 3600\r\nx-ratelimit-remaining: 42\r\n".to_string()
    } else if mode == MULTI_REPO_MISSING_RATE_HEADERS {
        String::new()
    } else {
        "x-ratelimit-resource: core\r\nx-ratelimit-limit: 5000\r\nx-ratelimit-remaining: 4999\r\nx-ratelimit-reset: 4102444800\r\n".to_string()
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{rate_headers}\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

struct RepoCommentListingFakeGitHub {
    base_url: String,
    mode: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl RepoCommentListingFakeGitHub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);
        let mode = Arc::new(AtomicUsize::new(REPO_COMMENT_LISTING_ACTIVE));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_mode = Arc::clone(&mode);
        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);
        let thread_base_url = base_url.clone();

        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(stream) => handle_repo_comment_listing_connection(
                        stream,
                        &thread_base_url,
                        &thread_mode,
                        &thread_requests,
                    ),
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

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn set_mode(&self, mode: usize) {
        self.mode.store(mode, Ordering::SeqCst);
    }
}

impl Drop for RepoCommentListingFakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

const REPO_COMMENT_LISTING_ACTIVE: usize = 1;
const REPO_COMMENT_LISTING_PERMISSION_AFTER_PAGE: usize = 2;

fn handle_repo_comment_listing_connection(
    mut stream: TcpStream,
    base_url: &str,
    mode: &Arc<AtomicUsize>,
    requests: &Arc<Mutex<Vec<String>>>,
) {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().unwrap_or("").to_string();
    let request_line_lower = request_line.to_ascii_lowercase();
    requests.lock().unwrap().push(request_line.clone());
    let mode = mode.load(Ordering::SeqCst);

    let (status, body, extra_headers) = if request_line_lower
        .starts_with("get /repos/owner/repo/issues/comments?")
    {
        if mode == REPO_COMMENT_LISTING_PERMISSION_AFTER_PAGE
            && request_line_lower.contains("page=2")
        {
            (
                "403 Forbidden",
                r#"{"message":"resource not accessible"}"#,
                String::new(),
            )
        } else if mode == REPO_COMMENT_LISTING_PERMISSION_AFTER_PAGE {
            (
                "200 OK",
                repo_listing_permission_page_payload(),
                format!(
                    "link: <{base_url}/repos/owner/repo/issues/comments?per_page=100&page=2>; rel=\"next\"\r\n"
                ),
            )
        } else {
            ("200 OK", repo_listing_comments_payload(), String::new())
        }
    } else if request_line_lower.starts_with("get /repos/owner/repo/issues?")
        && request_line_lower.contains("state=all")
    {
        if mode == REPO_COMMENT_LISTING_PERMISSION_AFTER_PAGE
            && request_line.contains("/repos/OWNER/REPO/")
        {
            ("304 Not Modified", "", String::new())
        } else {
            ("200 OK", repo_listing_issue_payload(), String::new())
        }
    } else if request_line_lower.starts_with("get /repos/owner/repo ") {
        if mode == REPO_COMMENT_LISTING_PERMISSION_AFTER_PAGE {
            (
                "403 Forbidden",
                r#"{"message":"resource not accessible"}"#,
                String::new(),
            )
        } else {
            ("200 OK", r#"{"full_name":"owner/repo"}"#, String::new())
        }
    } else if request_line_lower.starts_with("get /repos/owner/repo/issues/2 ") {
        (
            "200 OK",
            repo_listing_pull_request_object_payload(),
            String::new(),
        )
    } else if request_line_lower.starts_with("get /repos/owner/repo/issues/3 ") {
        (
            "200 OK",
            repo_listing_unsynced_issue_object_payload(),
            String::new(),
        )
    } else if request_line_lower.contains("/comments?") {
        // Per-issue comment endpoint must not be used in repo_listing mode.
        ("200 OK", "[]", String::new())
    } else {
        ("404 Not Found", r#"{"message":"not found"}"#, String::new())
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{extra_headers}x-ratelimit-remaining: 4999\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn repo_listing_issue_payload() -> &'static str {
    r#"[
      {
        "id": 7001,
        "node_id": "I_REPO_LISTING_1",
        "number": 1,
        "title": "Repo listing parent issue",
        "body": "parent issue body for repo listing comment tracer.",
        "state": "open",
        "locked": false,
        "comments": 1,
        "html_url": "https://github.com/owner/repo/issues/1",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T00:00:00Z",
        "closed_at": null,
        "user": {"login": "alice"},
        "labels": [],
        "milestone": null,
        "assignees": []
      }
    ]"#
}

fn repo_listing_comments_payload() -> &'static str {
    r#"[
      {
        "id": 9001,
        "node_id": "IC_REPO_LISTING_ISSUE",
        "body": "repo level comment tracer on an issue parent.",
        "html_url": "https://github.com/owner/repo/issues/1#issuecomment-9001",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-03T00:00:00Z",
        "user": {"login": "alice"},
        "issue_url": "https://api.github.com/repos/owner/repo/issues/1"
      },
      {
        "id": 9002,
        "node_id": "IC_REPO_LISTING_PR",
        "body": "repo level comment tracer on a pull request parent.",
        "html_url": "https://github.com/owner/repo/pull/2#issuecomment-9002",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-03T00:00:00Z",
        "user": {"login": "bob"},
        "issue_url": "https://api.github.com/repos/owner/repo/issues/2"
      },
      {
        "id": 9003,
        "node_id": "IC_REPO_LISTING_UNSYNCED",
        "body": "repo level comment tracer on an unsynced issue parent.",
        "html_url": "https://github.com/owner/repo/issues/3#issuecomment-9003",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-03T00:00:00Z",
        "user": {"login": "carol"},
        "issue_url": "https://api.github.com/repos/owner/repo/issues/3"
      }
    ]"#
}

fn repo_listing_permission_page_payload() -> &'static str {
    r#"[
      {
        "id": 9010,
        "node_id": "IC_REPO_LISTING_PENDING",
        "body": "repo listing pending evidence tracer.",
        "html_url": "https://github.com/owner/repo/issues/1#issuecomment-9010",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-03T00:00:00Z",
        "user": {"login": "alice"},
        "issue_url": "https://api.github.com/repos/owner/repo/issues/1"
      }
    ]"#
}

fn repo_listing_pull_request_object_payload() -> &'static str {
    r#"{
        "id": 7002,
        "node_id": "PR_REPO_LISTING_2",
        "number": 2,
        "title": "Repo listing pull request",
        "body": "a pull request, not an issue.",
        "state": "open",
        "locked": false,
        "comments": 1,
        "html_url": "https://github.com/owner/repo/pull/2",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T00:00:00Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [],
        "milestone": null,
        "assignees": [],
        "pull_request": {"url": "https://api.github.com/repos/owner/repo/pulls/2"}
    }"#
}

fn repo_listing_unsynced_issue_object_payload() -> &'static str {
    r#"{
        "id": 7003,
        "node_id": "I_REPO_LISTING_3",
        "number": 3,
        "title": "Unsynced issue parent",
        "body": "a real issue that is not in the local corpus yet.",
        "state": "open",
        "locked": false,
        "comments": 1,
        "html_url": "https://github.com/owner/repo/issues/3",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T00:00:00Z",
        "closed_at": null,
        "user": {"login": "carol"},
        "labels": [],
        "milestone": null,
        "assignees": []
    }"#
}

fn multi_repo_owner_issue_payload() -> &'static str {
    r#"[
      {
        "id": 3001,
        "node_id": "I_POLICY_OWNER",
        "number": 42,
        "title": "Owner repo policy issue",
        "body": "shared repo policy tracer from the owner repository.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/42",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:05Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}],
        "milestone": null,
        "assignees": []
      }
    ]"#
}

fn multi_repo_owner_two_issue_payload() -> &'static str {
    r#"[
      {
        "id": 3001,
        "node_id": "I_POLICY_OWNER",
        "number": 42,
        "title": "Owner repo policy issue",
        "body": "shared repo policy tracer from the owner repository.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/42",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:05Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}],
        "milestone": null,
        "assignees": []
      },
      {
        "id": 3003,
        "node_id": "I_POLICY_OWNER_SECOND",
        "number": 43,
        "title": "Second owner repo policy issue",
        "body": "second owner repository lifecycle tracer.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/43",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:06Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}],
        "milestone": null,
        "assignees": []
      }
    ]"#
}

fn multi_repo_owner_issue_object_payload() -> &'static str {
    r#"{
        "id": 3001,
        "node_id": "I_POLICY_OWNER",
        "number": 42,
        "title": "Owner repo policy issue",
        "body": "shared repo policy tracer from the owner repository.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/42",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:05Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}],
        "milestone": null,
        "assignees": []
    }"#
}

fn multi_repo_owner_second_issue_object_payload() -> &'static str {
    r#"{
        "id": 3003,
        "node_id": "I_POLICY_OWNER_SECOND",
        "number": 43,
        "title": "Second owner repo policy issue",
        "body": "second owner repository lifecycle tracer.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/43",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:06Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}],
        "milestone": null,
        "assignees": []
    }"#
}

fn multi_repo_other_issue_payload() -> &'static str {
    r#"[
      {
        "id": 3002,
        "node_id": "I_POLICY_OTHER",
        "number": 7,
        "title": "Other repo policy issue",
        "body": "shared repo policy tracer from the other repository.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/other/repo/issues/7",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:05Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}],
        "milestone": null,
        "assignees": []
      }
    ]"#
}

fn multi_repo_other_issue_object_payload() -> &'static str {
    r#"{
        "id": 3002,
        "node_id": "I_POLICY_OTHER",
        "number": 7,
        "title": "Other repo policy issue",
        "body": "shared repo policy tracer from the other repository.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/other/repo/issues/7",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:05Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}],
        "milestone": null,
        "assignees": []
    }"#
}

fn handle_connection(
    mut stream: TcpStream,
    issue_payload: &'static str,
    comments_payload: &'static str,
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
        comments_payload
    } else if request_line.starts_with("GET /repos/owner/repo/issues/")
        && request_line.contains("/comments?")
        && request_line.contains("per_page=100")
    {
        "[]"
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
        || body == comments_payload
        || body == issue_object_payload()
        || body == issue_comment_object_payload()
        || body == rate_limit_payload()
        || body == "[]"
    {
        "200 OK"
    } else {
        "404 Not Found"
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\nx-ratelimit-resource: core\r\nx-ratelimit-limit: 5000\r\nx-ratelimit-remaining: 4999\r\nx-ratelimit-reset: 4102444800\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

struct DoctorRedirectFakeGitHub {
    base_url: String,
    redirected_requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    trusted_address: String,
    redirected_address: String,
    handles: Vec<JoinHandle<()>>,
}

impl DoctorRedirectFakeGitHub {
    fn start() -> Self {
        let trusted = TcpListener::bind("127.0.0.1:0").unwrap();
        let redirected = TcpListener::bind("127.0.0.1:0").unwrap();
        trusted.set_nonblocking(true).unwrap();
        redirected.set_nonblocking(true).unwrap();
        let trusted_address = trusted.local_addr().unwrap().to_string();
        let redirected_address = redirected.local_addr().unwrap().to_string();
        let base_url = format!("http://{trusted_address}");
        let redirect_url = format!("http://{redirected_address}/capture");
        let stop = Arc::new(AtomicBool::new(false));
        let redirected_requests = Arc::new(AtomicUsize::new(0));

        let trusted_stop = Arc::clone(&stop);
        let trusted_handle = thread::spawn(move || {
            while !trusted_stop.load(Ordering::SeqCst) {
                match trusted.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = [0_u8; 4096];
                        let _ = stream.read(&mut request);
                        let response = format!(
                            "HTTP/1.1 302 Found\r\nlocation: {redirect_url}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        let redirected_stop = Arc::clone(&stop);
        let redirected_count = Arc::clone(&redirected_requests);
        let redirected_handle = thread::spawn(move || {
            while !redirected_stop.load(Ordering::SeqCst) {
                match redirected.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = [0_u8; 4096];
                        let _ = stream.read(&mut request);
                        redirected_count.fetch_add(1, Ordering::SeqCst);
                        let response = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nx-ratelimit-remaining: 1\r\nx-ratelimit-reset: 1\r\nconnection: close\r\n\r\n{}";
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url,
            redirected_requests,
            stop,
            trusted_address,
            redirected_address,
            handles: vec![trusted_handle, redirected_handle],
        }
    }

    fn redirected_request_count(&self) -> usize {
        self.redirected_requests.load(Ordering::SeqCst)
    }
}

impl Drop for DoctorRedirectFakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(&self.trusted_address);
        let _ = TcpStream::connect(&self.redirected_address);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

struct HeaderCheckingFakeGitHub {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl HeaderCheckingFakeGitHub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(stream) => handle_header_checking_connection(stream),
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for HeaderCheckingFakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_header_checking_connection(mut stream: TcpStream) {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().unwrap_or("");
    let lower = request.to_ascii_lowercase();
    let has_required_headers = lower.contains("user-agent: qgh/")
        && lower.contains("x-github-api-version: 2022-11-28")
        && lower.contains("accept: application/vnd.github+json")
        && !lower.contains("authorization:");

    let (status, body) = if !has_required_headers {
        (
            "403 Forbidden",
            r#"{"message":"Missing required GitHub REST request headers"}"#,
        )
    } else if request_line.starts_with("GET /repos/owner/repo/issues?")
        && request_line.contains("state=all")
        && request_line.contains("per_page=100")
    {
        ("200 OK", issue_payload_with_pr())
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42/comments?")
        && request_line.contains("per_page=100")
    {
        ("200 OK", issue_comments_payload())
    } else if request_line.starts_with("GET /rate_limit ") {
        ("200 OK", rate_limit_payload())
    } else {
        ("404 Not Found", r#"{"message":"not found"}"#)
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

    let (status, body, location) = if request_line.starts_with("GET /repos/owner/repo/issues?")
        && request_line.contains("state=all")
        && request_line.contains("per_page=100")
    {
        ("200 OK", issue_payload_with_pr(), None)
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42/comments?")
        && request_line.contains("per_page=100")
    {
        if mode == LIFECYCLE_DELETED_COMMENT {
            ("200 OK", "[]", None)
        } else {
            ("200 OK", issue_comments_payload(), None)
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42 ") {
        if mode == LIFECYCLE_UNAVAILABLE_ISSUE {
            ("404 Not Found", r#"{"message":"not found"}"#, None)
        } else if mode == LIFECYCLE_MOVED_ISSUE {
            (
                "301 Moved Permanently",
                r#"{"message":"moved"}"#,
                Some("/repos/owner/repo/issues/43"),
            )
        } else {
            ("200 OK", issue_object_payload(), None)
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/comments/5001 ") {
        if mode == LIFECYCLE_DELETED_COMMENT {
            ("404 Not Found", r#"{"message":"not found"}"#, None)
        } else {
            ("200 OK", issue_comment_object_payload(), None)
        }
    } else if request_line.starts_with("GET /repos/owner/repo ") {
        ("200 OK", r#"{"full_name":"owner/repo"}"#, None)
    } else {
        ("404 Not Found", r#"{"message":"not found"}"#, None)
    };
    let location_header = location
        .map(|location| format!("location: {location}\r\n"))
        .unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{location_header}x-ratelimit-remaining: 4999\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

const RATE_LIMIT_ACTIVE: usize = 1;
const RATE_LIMIT_PRIMARY: usize = 2;
const RATE_LIMIT_SECONDARY: usize = 3;
const RATE_LIMIT_HUGE_RETRY_AFTER: usize = 4;
const RATE_LIMIT_MISSING_HEADERS: usize = 5;

struct RateLimitFakeGitHub {
    base_url: String,
    mode: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl RateLimitFakeGitHub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);
        let mode = Arc::new(AtomicUsize::new(RATE_LIMIT_ACTIVE));
        let requests = Arc::new(AtomicUsize::new(0));
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
                        thread_requests.fetch_add(1, Ordering::SeqCst);
                        handle_rate_limit_connection(stream, &thread_mode)
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
        self.requests.load(Ordering::SeqCst)
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
    if request_line.starts_with("GET /repos/owner/repo/issues?")
        && request_line.contains("state=all")
        && mode == RATE_LIMIT_HUGE_RETRY_AFTER
    {
        let body = r#"{"message":"secondary rate limit"}"#;
        let response = format!(
            "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\nretry-after: {}\r\nx-ratelimit-remaining: 42\r\n\r\n{body}",
            body.len(),
            i64::MAX
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
    let rate_headers = if mode == RATE_LIMIT_MISSING_HEADERS {
        String::new()
    } else {
        "x-ratelimit-resource: core\r\nx-ratelimit-limit: 5000\r\nx-ratelimit-remaining: 4999\r\nx-ratelimit-reset: 4102444800\r\n".to_string()
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{rate_headers}\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

const SCHEDULED_BUDGET_BOOTSTRAP: usize = 1;
const SCHEDULED_BUDGET_KNOWN_RESERVE: usize = 2;
const SCHEDULED_BUDGET_UNKNOWN: usize = 3;
const SCHEDULED_BUDGET_TRANSPORT_DROP: usize = 4;
const SCHEDULED_BUDGET_FINAL_MISSING_HEADERS: usize = 5;

struct ScheduledBudgetGateFakeGitHub {
    base_url: String,
    mode: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ScheduledBudgetGateFakeGitHub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let mode = Arc::new(AtomicUsize::new(SCHEDULED_BUDGET_BOOTSTRAP));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_mode = Arc::clone(&mode);
        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);
        let thread_base_url = base_url.clone();

        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(stream) => handle_scheduled_budget_gate_connection(
                        stream,
                        &thread_mode,
                        &thread_requests,
                        &thread_base_url,
                    ),
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

    fn clear_requests(&self) {
        self.requests.lock().unwrap().clear();
    }
}

impl Drop for ScheduledBudgetGateFakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_scheduled_budget_gate_connection(
    mut stream: TcpStream,
    mode: &Arc<AtomicUsize>,
    requests: &Arc<Mutex<Vec<String>>>,
    base_url: &str,
) {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().unwrap_or("").to_string();
    let request_number = {
        let mut requests = requests.lock().unwrap();
        requests.push(request_line.clone());
        requests.len()
    };

    let mode = mode.load(Ordering::SeqCst);
    if mode == SCHEDULED_BUDGET_TRANSPORT_DROP {
        return;
    }
    let is_issue_listing = request_line.contains("/issues?") && request_line.contains("state=all");
    let body = "[]";
    let link = if mode != SCHEDULED_BUDGET_BOOTSTRAP && is_issue_listing {
        format!("link: <{base_url}/repos/owner/repo/issues?page=2>; rel=\"next\"\r\n")
    } else {
        String::new()
    };
    let rate_headers = match mode {
        SCHEDULED_BUDGET_BOOTSTRAP => "x-ratelimit-resource: core\r\nx-ratelimit-limit: 5000\r\nx-ratelimit-remaining: 4999\r\nx-ratelimit-reset: 4102444800\r\n".to_string(),
        SCHEDULED_BUDGET_KNOWN_RESERVE => "x-ratelimit-resource: core\r\nx-ratelimit-limit: 10\r\nx-ratelimit-remaining: 2\r\nx-ratelimit-reset: 4102444800\r\n".to_string(),
        SCHEDULED_BUDGET_UNKNOWN => String::new(),
        SCHEDULED_BUDGET_FINAL_MISSING_HEADERS if request_number == 1 => "x-ratelimit-resource: core\r\nx-ratelimit-limit: 10\r\nx-ratelimit-remaining: 7\r\nx-ratelimit-reset: 4102444800\r\n".to_string(),
        SCHEDULED_BUDGET_FINAL_MISSING_HEADERS => String::new(),
        _ => unreachable!("unsupported scheduled budget test mode"),
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{link}{rate_headers}\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

const PAGINATED_BACKOFF: usize = 1;
const PAGINATED_RESUME: usize = 2;

struct PaginatedBackoffFakeGitHub {
    base_url: String,
    mode: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PaginatedBackoffFakeGitHub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);
        let mode = Arc::new(AtomicUsize::new(PAGINATED_BACKOFF));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_mode = Arc::clone(&mode);
        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);
        let thread_base_url = base_url.clone();

        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(stream) => handle_paginated_backoff_connection(
                        stream,
                        &thread_mode,
                        &thread_requests,
                        &thread_base_url,
                    ),
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

    fn clear_requests(&self) {
        self.requests.lock().unwrap().clear();
    }
}

impl Drop for PaginatedBackoffFakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_paginated_backoff_connection(
    mut stream: TcpStream,
    mode: &Arc<AtomicUsize>,
    requests: &Arc<Mutex<Vec<String>>>,
    base_url: &str,
) {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().unwrap_or("").to_string();
    requests.lock().unwrap().push(request_line.clone());

    let mode = mode.load(Ordering::SeqCst);
    let page_two_url = format!("{base_url}/repos/owner/repo/issues?page=2");
    let link_header = format!("link: <{page_two_url}>; rel=\"next\"\r\n");

    if request_line.starts_with("GET /repos/owner/repo/issues?page=2") && mode == PAGINATED_BACKOFF
    {
        let body = r#"{"message":"secondary rate limit"}"#;
        let response = format!(
            "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\nretry-after: 0\r\nx-ratelimit-remaining: 42\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        return;
    }

    let (status, body, extra_headers) = if request_line.starts_with("GET /repos/owner/repo/issues?")
        && request_line.contains("state=all")
        && request_line.contains("sort=updated")
        && request_line.contains("direction=asc")
        && request_line.contains("per_page=100")
    {
        ("200 OK", paginated_issue_page_one_payload(), link_header)
    } else if request_line.starts_with("GET /repos/owner/repo/issues?page=2") {
        ("200 OK", paginated_issue_page_two_payload(), String::new())
    } else if (request_line.starts_with("GET /repos/owner/repo/issues/1/comments?")
        || request_line.starts_with("GET /repos/owner/repo/issues/2/comments?"))
        && request_line.contains("per_page=100")
    {
        ("200 OK", "[]", String::new())
    } else if request_line.starts_with("GET /repos/owner/repo/issues/1 ") {
        (
            "200 OK",
            paginated_issue_one_object_payload(),
            String::new(),
        )
    } else if request_line.starts_with("GET /repos/owner/repo/issues/2 ") {
        (
            "200 OK",
            paginated_issue_two_object_payload(),
            String::new(),
        )
    } else {
        ("404 Not Found", r#"{"message":"not found"}"#, String::new())
    };

    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{extra_headers}x-ratelimit-remaining: 4999\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

fn paginated_issue_page_one_payload() -> &'static str {
    r#"[
      {
        "id": 5101,
        "node_id": "I_PAGE_ONE",
        "number": 1,
        "title": "First durable page",
        "body": "first durable page should survive a later pagination backoff.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/1",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T00:01:00Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [],
        "milestone": null,
        "assignees": []
      }
    ]"#
}

fn paginated_issue_page_two_payload() -> &'static str {
    r#"[
      {
        "id": 5102,
        "node_id": "I_PAGE_TWO",
        "number": 2,
        "title": "Second durable page",
        "body": "second durable page should be found after resume.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/2",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T00:02:00Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [],
        "milestone": null,
        "assignees": []
      }
    ]"#
}

fn paginated_issue_one_object_payload() -> &'static str {
    r#"{
        "id": 5101,
        "node_id": "I_PAGE_ONE",
        "number": 1,
        "title": "First durable page",
        "body": "first durable page should survive a later pagination backoff.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/1",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T00:01:00Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [],
        "milestone": null,
        "assignees": []
    }"#
}

fn paginated_issue_two_object_payload() -> &'static str {
    r#"{
        "id": 5102,
        "node_id": "I_PAGE_TWO",
        "number": 2,
        "title": "Second durable page",
        "body": "second durable page should be found after resume.",
        "state": "open",
        "locked": false,
        "comments": 0,
        "html_url": "https://github.com/owner/repo/issues/2",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T00:02:00Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [],
        "milestone": null,
        "assignees": []
    }"#
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

fn sync_rate_budget_observations(value: &Value) -> &Vec<Value> {
    value["data"]["rate_budget"]["observations"]
        .as_array()
        .expect("sync rate budget observations")
}

fn status_sync_rate_budget_observations(value: &Value) -> &Vec<Value> {
    value["data"]["sync"]["rate_budget"]["observations"]
        .as_array()
        .expect("status rate budget observations")
}

fn json_object_keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("JSON object")
        .keys()
        .cloned()
        .collect()
}

#[cfg(feature = "fastembed-provider")]
fn doctor_check_ok(checks: &[Value], name: &str) -> Option<bool> {
    checks
        .iter()
        .find(|check| check["name"] == name)
        .and_then(|check| check["ok"].as_bool())
}

fn warning_codes(output_json: &Value) -> Vec<&str> {
    output_json["warnings"]
        .as_array()
        .expect("warnings array")
        .iter()
        .map(|warning| warning["code"].as_str().expect("warning code"))
        .collect()
}

fn result_source_ids(output_json: &Value) -> Vec<String> {
    output_json["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["source_id"].as_str().unwrap().to_string())
        .collect()
}

#[cfg(feature = "fastembed-provider")]
fn embedding_sync_warning(output_json: &Value) -> &Value {
    let warnings = output_json["warnings"].as_array().unwrap();
    warnings
        .iter()
        .find(|warning| {
            warning["message"]
                .as_str()
                .is_some_and(|message| message.contains("BM25 index refresh remains available"))
        })
        .expect("embedding sync warning")
}

#[cfg(feature = "fastembed-provider")]
fn assert_embedding_sync_warning(output_json: &Value) {
    let warning = embedding_sync_warning(output_json);
    assert_eq!(warning["severity"], "warn");
    assert!(warning["message"]
        .as_str()
        .unwrap()
        .contains("BM25 index refresh remains available"));
    assert_eq!(
        json_object_keys(warning),
        BTreeSet::from([
            "code".to_string(),
            "message".to_string(),
            "severity".to_string(),
        ])
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

fn assert_query_result_round_trips_to_get_result(result: &Value, source: &Value) {
    assert_eq!(result["get_args"]["source_id"], source["source_id"]);
    assert_eq!(result["source_id"], source["source_id"]);
    assert_eq!(result["entity_type"], source["entity_type"]);
    assert_eq!(result["canonical_url"], source["canonical_url"]);
    assert_eq!(result["source_version"], source["source_version"]);
}

const EDITING_SAME_BODY_NEW_TIMESTAMP: usize = 3;

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
        } else if mode == EDITING_SAME_BODY_NEW_TIMESTAMP {
            (
                "200 OK",
                "\"issues-same-body-v3\"",
                same_body_new_timestamp_issue_payload(),
            )
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
        if mode == EDITING_SAME_BODY_NEW_TIMESTAMP {
            (
                "200 OK",
                "\"issue-same-body-v3\"",
                same_body_new_timestamp_issue_object_payload(),
            )
        } else if mode == 2 {
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
    let remaining = if status == "304 Not Modified" {
        4997
    } else {
        4999
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\netag: {etag}\r\nx-ratelimit-resource: core\r\nx-ratelimit-limit: 5000\r\nx-ratelimit-remaining: {remaining}\r\nx-ratelimit-reset: 4102444800\r\n\r\n{body}",
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

fn same_body_new_timestamp_issue_payload() -> &'static str {
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
        "updated_at": "2026-01-05T00:00:00Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}, {"name": "mvp"}],
        "milestone": {"title": "MVP"},
        "assignees": [{"login": "alice"}]
      }
    ]"#
}

fn same_body_new_timestamp_issue_object_payload() -> &'static str {
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
        "updated_at": "2026-01-05T00:00:00Z",
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

const TARGET_REFRESH_ACTIVE: usize = 1;
const TARGET_REFRESH_DIFF: usize = 2;
const TARGET_REFRESH_DELETED: usize = 3;
const TARGET_REFRESH_PERMISSION_LOSS: usize = 4;
const TARGET_REFRESH_TRANSFER: usize = 5;
const TARGET_REFRESH_TRANSFER_CYCLE: usize = 6;
const TARGET_REFRESH_AUTH_FAILED: usize = 7;
const TARGET_REFRESH_SECONDARY_RATE_LIMIT_NO_RETRY_AFTER: usize = 8;
const TARGET_REFRESH_EMPTY_COMMENTS: usize = 9;
const TARGET_REFRESH_INITIAL_WITH_TRANSFER_TARGET: usize = 10;
const TARGET_REFRESH_TRANSIENT: usize = 11;

struct TargetedRefreshFakeGitHub {
    base_url: String,
    mode: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TargetedRefreshFakeGitHub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);
        let mode = Arc::new(AtomicUsize::new(TARGET_REFRESH_ACTIVE));
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
                        handle_targeted_refresh_connection(stream, &thread_mode, &thread_requests)
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

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for TargetedRefreshFakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_targeted_refresh_connection(
    mut stream: TcpStream,
    mode: &Arc<AtomicUsize>,
    requests: &Arc<Mutex<Vec<String>>>,
) {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
    let request_line = request.lines().next().unwrap_or("").to_string();
    requests.lock().unwrap().push(request_line.clone());
    let mode = mode.load(Ordering::SeqCst);

    let (status, body, location) = if request_line.starts_with("GET /repos/owner/repo/issues?")
        && request_line.contains("state=all")
    {
        if mode == TARGET_REFRESH_INITIAL_WITH_TRANSFER_TARGET {
            (
                "200 OK",
                targeted_initial_with_transfer_target_payload(),
                None,
            )
        } else {
            ("200 OK", targeted_initial_issue_list_payload(), None)
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42/comments?")
        && request_line.contains("per_page=100")
    {
        if mode == TARGET_REFRESH_DIFF {
            ("200 OK", targeted_refreshed_comments_payload(), None)
        } else if mode == TARGET_REFRESH_EMPTY_COMMENTS {
            ("200 OK", "[]", None)
        } else {
            ("200 OK", targeted_initial_comments_payload(), None)
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42 ") {
        if mode == TARGET_REFRESH_DIFF {
            ("200 OK", targeted_refreshed_issue_object_payload(), None)
        } else if mode == TARGET_REFRESH_DELETED {
            ("404 Not Found", r#"{"message":"not found"}"#, None)
        } else if mode == TARGET_REFRESH_PERMISSION_LOSS {
            (
                "403 Forbidden",
                r#"{"message":"resource not accessible"}"#,
                None,
            )
        } else if mode == TARGET_REFRESH_AUTH_FAILED {
            ("401 Unauthorized", r#"{"message":"Bad credentials"}"#, None)
        } else if mode == TARGET_REFRESH_TRANSIENT {
            (
                "503 Service Unavailable",
                r#"{"message":"temporarily unavailable"}"#,
                None,
            )
        } else if mode == TARGET_REFRESH_SECONDARY_RATE_LIMIT_NO_RETRY_AFTER {
            (
                "403 Forbidden",
                r#"{"message":"You have exceeded a secondary rate limit. Please wait a few minutes before you try again."}"#,
                None,
            )
        } else if matches!(
            mode,
            TARGET_REFRESH_TRANSFER | TARGET_REFRESH_TRANSFER_CYCLE
        ) {
            (
                "301 Moved Permanently",
                r#"{"message":"moved"}"#,
                Some("/repos/owner/repo/issues/43"),
            )
        } else {
            ("200 OK", targeted_initial_issue_object_payload(), None)
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/43/comments?")
        && request_line.contains("per_page=100")
    {
        ("200 OK", targeted_transferred_comments_payload(), None)
    } else if request_line.starts_with("GET /repos/owner/repo/issues/43 ") {
        if mode == TARGET_REFRESH_TRANSFER_CYCLE {
            (
                "301 Moved Permanently",
                r#"{"message":"moved"}"#,
                Some("/repos/owner/repo/issues/42"),
            )
        } else {
            ("200 OK", targeted_transferred_issue_object_payload(), None)
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/comments/7001 ") {
        if mode == TARGET_REFRESH_DIFF {
            (
                "200 OK",
                targeted_refreshed_comment_one_object_payload(),
                None,
            )
        } else {
            (
                "200 OK",
                targeted_initial_comment_one_object_payload(),
                None,
            )
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/comments/7002 ") {
        ("200 OK", targeted_added_comment_object_payload(), None)
    } else if request_line.starts_with("GET /repos/owner/repo/issues/comments/7003 ") {
        if mode == TARGET_REFRESH_DIFF {
            ("404 Not Found", r#"{"message":"not found"}"#, None)
        } else {
            ("200 OK", targeted_deleted_comment_object_payload(), None)
        }
    } else if request_line.starts_with("GET /repos/owner/repo/issues/comments/7043 ") {
        (
            "200 OK",
            targeted_transferred_comment_object_payload(),
            None,
        )
    } else if request_line.starts_with("GET /repos/owner/repo ") {
        if mode == TARGET_REFRESH_PERMISSION_LOSS {
            (
                "403 Forbidden",
                r#"{"message":"resource not accessible"}"#,
                None,
            )
        } else {
            ("200 OK", r#"{"full_name":"owner/repo"}"#, None)
        }
    } else {
        ("404 Not Found", r#"{"message":"not found"}"#, None)
    };
    let location_header = location
        .map(|location| format!("location: {location}\r\n"))
        .unwrap_or_default();
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{location_header}x-ratelimit-remaining: 4999\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

fn targeted_initial_issue_list_payload() -> &'static str {
    r#"[
      {
        "id": 1001,
        "node_id": "I_kwDOISSUE1",
        "number": 42,
        "title": "Cache sync bug",
        "body": "The BM25 issue body tracer must round-trip through get before citation.",
        "state": "open",
        "locked": false,
        "comments": 2,
        "html_url": "https://github.com/owner/repo/issues/42",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-02T03:04:05Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}, {"name": "mvp"}],
        "milestone": {"title": "MVP"},
        "assignees": [{"login": "alice"}]
      }
    ]"#
}

fn targeted_initial_with_transfer_target_payload() -> &'static str {
    r#"[
      {
        "id": 1001,
        "node_id": "I_kwDOISSUE1",
        "number": 42,
        "title": "Cache sync bug",
        "body": "The BM25 issue body tracer must round-trip through get before citation.",
        "state": "open",
        "locked": false,
        "comments": 2,
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
        "id": 1043,
        "node_id": "I_TARGET_TRANSFER",
        "number": 43,
        "title": "Transferred issue target",
        "body": "transferredtargetsentinel final issue body.",
        "state": "open",
        "locked": false,
        "comments": 1,
        "html_url": "https://github.com/owner/repo/issues/43",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-04T00:00:00Z",
        "closed_at": null,
        "user": {"login": "carol"},
        "labels": [],
        "milestone": null,
        "assignees": []
      }
    ]"#
}

fn targeted_initial_issue_object_payload() -> &'static str {
    r#"{
        "id": 1001,
        "node_id": "I_kwDOISSUE1",
        "number": 42,
        "title": "Cache sync bug",
        "body": "The BM25 issue body tracer must round-trip through get before citation.",
        "state": "open",
        "locked": false,
        "comments": 2,
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

fn targeted_refreshed_issue_object_payload() -> &'static str {
    r#"{
        "id": 1001,
        "node_id": "I_kwDOISSUE1",
        "number": 42,
        "title": "Cache sync bug refreshed",
        "body": "The targeted refresh issue body must replace stale local content.",
        "state": "open",
        "locked": false,
        "comments": 2,
        "html_url": "https://github.com/owner/repo/issues/42",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-05T00:00:00Z",
        "closed_at": null,
        "user": {"login": "bob"},
        "labels": [{"name": "bug"}, {"name": "mvp"}],
        "milestone": {"title": "MVP"},
        "assignees": [{"login": "alice"}]
    }"#
}

fn targeted_initial_comments_payload() -> &'static str {
    r#"[
      {
        "id": 7001,
        "node_id": "IC_TARGET_1",
        "body": "The stale targeted comment body should be updated.",
        "html_url": "https://github.com/owner/repo/issues/42#issuecomment-7001",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-03T04:05:06Z",
        "user": {"login": "carol"}
      },
      {
        "id": 7003,
        "node_id": "IC_TARGET_3",
        "body": "This deleteonlysentinel comment is initially indexed.",
        "html_url": "https://github.com/owner/repo/issues/42#issuecomment-7003",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-03T04:06:06Z",
        "user": {"login": "dave"}
      }
    ]"#
}

fn targeted_refreshed_comments_payload() -> &'static str {
    r#"[
      {
        "id": 7001,
        "node_id": "IC_TARGET_1",
        "body": "The targeted refresh updated comment should replace the stale body.",
        "html_url": "https://github.com/owner/repo/issues/42#issuecomment-7001",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-05T00:01:00Z",
        "user": {"login": "carol"}
      },
      {
        "id": 7002,
        "node_id": "IC_TARGET_2",
        "body": "The targeted refresh added comment should be indexed.",
        "html_url": "https://github.com/owner/repo/issues/42#issuecomment-7002",
        "created_at": "2026-01-05T00:02:00Z",
        "updated_at": "2026-01-05T00:02:00Z",
        "user": {"login": "erin"}
      }
    ]"#
}

fn targeted_initial_comment_one_object_payload() -> &'static str {
    r#"{
        "id": 7001,
        "node_id": "IC_TARGET_1",
        "body": "The stale targeted comment body should be updated.",
        "html_url": "https://github.com/owner/repo/issues/42#issuecomment-7001",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-03T04:05:06Z",
        "user": {"login": "carol"}
    }"#
}

fn targeted_refreshed_comment_one_object_payload() -> &'static str {
    r#"{
        "id": 7001,
        "node_id": "IC_TARGET_1",
        "body": "The targeted refresh updated comment should replace the stale body.",
        "html_url": "https://github.com/owner/repo/issues/42#issuecomment-7001",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-05T00:01:00Z",
        "user": {"login": "carol"}
    }"#
}

fn targeted_added_comment_object_payload() -> &'static str {
    r#"{
        "id": 7002,
        "node_id": "IC_TARGET_2",
        "body": "The targeted refresh added comment should be indexed.",
        "html_url": "https://github.com/owner/repo/issues/42#issuecomment-7002",
        "created_at": "2026-01-05T00:02:00Z",
        "updated_at": "2026-01-05T00:02:00Z",
        "user": {"login": "erin"}
    }"#
}

fn targeted_deleted_comment_object_payload() -> &'static str {
    r#"{
        "id": 7003,
        "node_id": "IC_TARGET_3",
        "body": "This deleteonlysentinel comment is initially indexed.",
        "html_url": "https://github.com/owner/repo/issues/42#issuecomment-7003",
        "created_at": "2026-01-03T00:00:00Z",
        "updated_at": "2026-01-03T04:06:06Z",
        "user": {"login": "dave"}
    }"#
}

fn targeted_transferred_issue_object_payload() -> &'static str {
    r#"{
        "id": 1043,
        "node_id": "I_TARGET_TRANSFER",
        "number": 43,
        "title": "Transferred target issue",
        "body": "The transferredtargetsentinel issue body should be indexed at the final alias.",
        "state": "open",
        "locked": false,
        "comments": 1,
        "html_url": "https://github.com/owner/repo/issues/43",
        "created_at": "2026-01-05T00:00:00Z",
        "updated_at": "2026-01-05T00:03:00Z",
        "closed_at": null,
        "user": {"login": "frank"},
        "labels": [{"name": "transfer"}],
        "milestone": null,
        "assignees": []
    }"#
}

fn targeted_transferred_comments_payload() -> &'static str {
    r#"[
      {
        "id": 7043,
        "node_id": "IC_TARGET_TRANSFER",
        "body": "The transferredtargetsentinel comment should also be indexed.",
        "html_url": "https://github.com/owner/repo/issues/43#issuecomment-7043",
        "created_at": "2026-01-05T00:04:00Z",
        "updated_at": "2026-01-05T00:04:00Z",
        "user": {"login": "gina"}
      }
    ]"#
}

fn targeted_transferred_comment_object_payload() -> &'static str {
    r#"{
        "id": 7043,
        "node_id": "IC_TARGET_TRANSFER",
        "body": "The transferredtargetsentinel comment should also be indexed.",
        "html_url": "https://github.com/owner/repo/issues/43#issuecomment-7043",
        "created_at": "2026-01-05T00:04:00Z",
        "updated_at": "2026-01-05T00:04:00Z",
        "user": {"login": "gina"}
    }"#
}
