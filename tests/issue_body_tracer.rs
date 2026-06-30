use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
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
    assert_eq!(query_json["meta"]["profile_id"], "work");
    assert_eq!(query_json["meta"]["profile_source"], "cli");
    assert_eq!(query_json["meta"]["repo"], Value::Null);
    assert_eq!(query_json["meta"]["repo_source"], Value::Null);
    assert_eq!(query_json["meta"]["repo_policy_path"], Value::Null);
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
    assert!(stdout.contains("backoff: none"));
    assert!(stdout.contains("active index generation: 1"));
    assert!(stdout.contains("next: qgh query <terms> --profile work"));
    let stderr = stderr_text(&sync);
    assert!(stderr.contains("qgh sync: fetching GitHub issues/comments repos=1"));
    assert!(stderr.contains("qgh sync: fetching repo=owner/repo"));
    assert!(stderr.contains("qgh sync: received issue page repo=owner/repo items=2"));
    assert!(stderr.contains("qgh sync: received comment page repo=owner/repo issue=#42 items=1"));
    assert!(stderr.contains("qgh sync: complete sync_run_id="));

    let quiet = fixture.qgh(["sync", "--quiet"]);
    assert_success(&quiet);
    assert!(stderr_text(&quiet).is_empty());
    assert!(stdout_text(&quiet).contains("qgh sync complete"));
    assert!(!stdout_text(&quiet).starts_with('{'));

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
    assert!(query_stdout.contains("qgh query results"));
    assert!(query_stdout.contains("These are source candidates, not answers"));
    assert!(query_stdout.contains("Snippets are previews, not citation evidence"));
    assert!(
        query_stdout.contains("get: qgh get qgh://github.com/issue/I_kwDOISSUE1 --profile-id work")
    );

    let search = fixture.qgh(["search", "BM25 tracer"]);
    assert_success(&search);
    assert!(stdout_text(&search).contains("qgh query results"));

    let get = fixture.qgh(["get", "qgh://github.com/issue/I_kwDOISSUE1"]);
    assert_success(&get);
    let get_stdout = stdout_text(&get);
    assert!(get_stdout.contains("qgh source"));
    assert!(get_stdout.contains("canonical URL: https://github.com/owner/repo/issues/42"));
    assert!(get_stdout.contains("source version: body_hash="));
    assert!(get_stdout.contains("staleness metadata: github_updated_at=2026-01-02T03:04:05Z"));
    assert!(get_stdout.contains("lifecycle check: active"));
    assert!(get_stdout
        .contains("The BM25 issue body tracer must round-trip through get before citation."));

    let status = fixture.qgh(["status"]);
    assert_success(&status);
    let status_stdout = stdout_text(&status);
    assert!(status_stdout.contains("qgh status"));
    assert!(status_stdout.contains("selected profile: work"));
    assert!(status_stdout.contains("effective repo scope: all profile repos"));
    assert!(status_stdout.contains("DB path:"));
    assert!(status_stdout.contains("Tantivy index path:"));
    assert!(status_stdout.contains("default sync scope: all repos in the selected profile"));
    assert!(status_stdout.contains("qgh sync --all"));

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
    assert_eq!(stdout_json(&json_query)["schema_version"], "qgh.v1");
    assert!(stderr_text(&json_query).is_empty());
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
        "sequential"
    );
    assert_eq!(
        batch_json["data"]["lifecycle_check_policy"]["max_in_flight_requests"],
        1
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
fn get_batch_records_tombstone_as_item_error() {
    let fixture = TestFixture::new("get-batch-tombstone-item-error");
    let server = LifecycleFakeGitHub::start();
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(["sync", "--json"]));

    server.set_mode(LIFECYCLE_DELETED_COMMENT);
    let issue_id = "qgh://github.com/issue/I_kwDOISSUE1";
    let comment_id = "qgh://github.com/issue-comment/IC_kwDOCOMMENT1";
    let batch = fixture.qgh(["get", issue_id, comment_id, "--json"]);
    assert_success(&batch);
    let batch_json = stdout_json(&batch);
    assert_eq!(batch_json["data"]["summary"]["returned"], 1);
    assert_eq!(batch_json["data"]["summary"]["failed"], 1);
    let items = batch_json["data"]["items"].as_array().unwrap();
    assert_eq!(items[0]["source_id"], issue_id);
    assert_eq!(items[0]["ok"], true);
    assert_eq!(items[1]["source_id"], comment_id);
    assert_eq!(items[1]["ok"], false);
    assert_eq!(items[1]["error"]["code"], "source.tombstoned");
    assert_eq!(items[1]["error"]["details"]["reason"], "not_found");
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
    assert_eq!(init_json["schema_version"], "qgh.v1");
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
    assert_eq!(init_json["data"]["repo_allowlist_action"], "added");
    assert_eq!(init_json["data"]["repo_policy_action"], "created");
    assert_eq!(init_json["data"]["token_source"]["kind"], "env");
    assert_eq!(
        init_json["data"]["next_steps"],
        json!(["qgh sync", "qgh query <terms>"])
    );

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
    assert!(stderr.contains("profile id: work"));
    assert!(stderr.contains("token source: github_cli"));
    assert!(stderr.contains("config path:"));
    assert!(stderr.contains("repo policy path:"));
    assert!(stderr.contains("db path:"));
    assert!(stderr.contains("Use these defaults? [Y/n]"));

    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["profile_id"], "work");
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
    assert_eq!(init_json["data"]["profile_id"], "work");
    assert_eq!(init_json["data"]["repo"], "owner/repo");
    assert_eq!(init_json["data"]["token_source"]["kind"], "github_cli");
    assert_eq!(init_json["data"]["repo_policy_action"], "created");
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
    assert!(stdout.contains("profile: work (created)"));
    assert!(stdout.contains("repo: owner/repo (allowlist added)"));
    assert!(stdout.contains("token source: github_cli"));
    assert!(stdout.contains("config:"));
    assert!(stdout.contains("repo policy: created at"));
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
fn init_short_yes_alias_applies_inferred_preset() {
    let fixture = TestFixture::new("init-short-yes");
    let nested_worktree_dir =
        fixture.init_git_worktree_with_origin("git@ghe.internal.example:team/repo.git");

    let init = fixture.qgh_without_profile_in(&nested_worktree_dir, ["init", "-y", "--json"]);
    assert_success(&init);
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["profile_id"], "work");
    assert_eq!(init_json["data"]["repo"], "team/repo");
    assert_eq!(init_json["data"]["token_source"]["kind"], "github_cli");

    let config_text = fs::read_to_string(fixture.config_home.join("qgh/config.toml")).unwrap();
    assert!(config_text.contains(r#"host = "ghe.internal.example""#));
    assert!(config_text.contains(r#"api_base_url = "https://ghe.internal.example/api/v3""#));
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
fn init_yes_reports_existing_profile_token_source_when_not_changed() {
    let fixture = TestFixture::new("init-existing-token-source");
    fixture.write_config("https://api.github.com");
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
    assert_success(&init);
    let init_json = stdout_json(&init);
    assert_eq!(init_json["data"]["profile_action"], "updated");
    assert_eq!(init_json["data"]["token_source"]["kind"], "env");

    let config_text = fs::read_to_string(fixture.config_home.join("qgh/config.toml")).unwrap();
    assert!(config_text.contains(r#"type = "env""#));
    assert!(!config_text.contains(r#"type = "github_cli""#));
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
        assert_eq!(
            init_json["data"]["profile_validation"]["status"],
            "not_checked"
        );
        assert_eq!(
            init_json["warnings"][0]["code"],
            "config.profile_not_checked"
        );
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

    for (case, remote) in [("malformed", "not a github remote")] {
        let fixture = TestFixture::new(&format!("init-bad-origin-{case}"));
        let nested = fixture.init_git_worktree_with_origin(remote);
        let output = fixture.qgh_without_profile_in(&nested, ["init", "repo", "--json"]);
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            stdout_json(&output)["error"]["code"],
            "config.unsupported_git_remote"
        );
        assert!(!fixture.root.join(".qgh.toml").exists());
    }
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
fn sync_resumes_from_last_committed_issue_page_after_mid_pagination_backoff() {
    let fixture = TestFixture::new("paginated-backoff-resume");
    let server = PaginatedBackoffFakeGitHub::start();
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
        limited_json["data"]["backoff"]["last_successful_sync"],
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
        resumed_run_id
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

    fn write_config_with_repos(&self, api_base_url: &str, repos: &[&str]) {
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

    fn qgh<const N: usize>(&self, args: [&str; N]) -> Output {
        let mut cmd = self.base_command();
        cmd.args(["--profile", "work"]).args(args);
        cmd.output().unwrap()
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

struct MultiRepoFakeGitHub {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MultiRepoFakeGitHub {
    fn start() -> Self {
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
                    Ok(stream) => handle_multi_repo_connection(stream, &thread_requests),
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

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn clear_requests(&self) {
        self.requests.lock().unwrap().clear();
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

fn handle_multi_repo_connection(mut stream: TcpStream, requests: &Arc<Mutex<Vec<String>>>) {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().unwrap_or("").to_string();
    requests.lock().unwrap().push(request_line.clone());

    let (status, body) = if request_line.starts_with("GET /repos/owner/repo/issues?")
        && request_line.contains("state=all")
    {
        ("200 OK", multi_repo_owner_issue_payload())
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42/comments?") {
        ("200 OK", "[]")
    } else if request_line.starts_with("GET /repos/owner/repo/issues/42 ") {
        ("200 OK", multi_repo_owner_issue_object_payload())
    } else if request_line.starts_with("GET /repos/other/repo/issues?")
        && request_line.contains("state=all")
    {
        ("200 OK", multi_repo_other_issue_payload())
    } else if request_line.starts_with("GET /repos/other/repo/issues/7/comments?") {
        ("200 OK", "[]")
    } else if request_line.starts_with("GET /repos/other/repo/issues/7 ") {
        ("200 OK", multi_repo_other_issue_object_payload())
    } else {
        ("404 Not Found", r#"{"message":"not found"}"#)
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\nx-ratelimit-remaining: 4999\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
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
        || body == issue_comments_payload()
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
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\nx-ratelimit-remaining: 4999\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
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
        && lower.contains("accept: application/vnd.github+json");

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
    } else if request_line.starts_with("GET /repos/owner/repo/issues/1/comments?")
        && request_line.contains("per_page=100")
    {
        ("200 OK", "[]", String::new())
    } else if request_line.starts_with("GET /repos/owner/repo/issues/2/comments?")
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
