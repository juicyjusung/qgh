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
        "init", "sync", "embed", "query", "search", "get", "status", "doctor", "mcp",
    ] {
        assert!(
            help_text.contains(command),
            "missing top-level help command: {command}"
        );
    }
    for excluded in ["eval", "write", "delete", "update"] {
        assert!(
            !help_text.contains(&format!("  {excluded}")),
            "unexpected top-level help command: {excluded}"
        );
    }

    for args in [
        &["init", "--help"][..],
        &["init", "repo", "--help"][..],
        &["sync", "--help"][..],
        &["embed", "--help"][..],
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
        assert_eq!(
            tool["outputSchema"]["oneOf"][0]["required"],
            json!(["data"])
        );
        assert_eq!(
            tool["outputSchema"]["oneOf"][0]["properties"]["ok"]["const"],
            true
        );
        assert_eq!(
            tool["outputSchema"]["oneOf"][0]["not"]["required"],
            json!(["error"])
        );
        assert_eq!(
            tool["outputSchema"]["oneOf"][1]["required"],
            json!(["error"])
        );
        assert_eq!(
            tool["outputSchema"]["oneOf"][1]["properties"]["ok"]["const"],
            false
        );
        assert_eq!(
            tool["outputSchema"]["oneOf"][1]["not"]["required"],
            json!(["data"])
        );
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
        artifact["contract"]["cli_commands"],
        json!(["init", "sync", "embed", "query", "search", "get", "status", "doctor", "mcp"])
    );
    assert_eq!(
        artifact["contract"]["canonical_cli_commands"],
        json!(["init", "sync", "embed", "query", "get", "status", "doctor"])
    );
    assert_eq!(
        artifact["contract"]["cli_only_commands"],
        json!(["init", "sync", "embed", "doctor"])
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
        artifact["contract"]["schema_object_closure"],
        "released schema object shapes are closed by default; only envelope.data and error.details are documented extension points"
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
    assert_eq!(
        artifact["contract"]["primary_install_channel"]["command"],
        "brew install juicyjusung/tap/qgh"
    );
    assert_eq!(
        artifact["contract"]["primary_install_channel"]["tap_repository"],
        "juicyjusung/homebrew-tap"
    );
    assert_eq!(artifact["contract"]["release_automation"], "cargo-dist");
    assert_eq!(
        artifact["contract"]["release_targets"],
        json!([
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-unknown-linux-gnu"
        ])
    );
    assert_eq!(
        artifact["contract"]["release_integrity_gate"],
        json!([
            "artifact checksums",
            "Homebrew sha256",
            "GitHub Artifact Attestations",
            "actions/attest@v4 id-token/attestations/artifact-metadata permissions"
        ])
    );
    assert_eq!(
        artifact["contract"]["tap_publish_credential"]["secret_name"],
        "HOMEBREW_TAP_TOKEN"
    );
    assert_eq!(
        artifact["contract"]["release_workflow"]["cargo_dist_version"],
        "0.32.0"
    );
    assert_eq!(
        artifact["contract"]["release_workflow"]["config"],
        "dist-workspace.toml"
    );
    assert_eq!(
        artifact["contract"]["release_workflow"]["workflow"],
        ".github/workflows/release.yml"
    );
    assert_eq!(
        artifact["contract"]["release_workflow"]["homebrew_smoke_workflow"],
        ".github/workflows/homebrew-smoke.yml"
    );
    assert_eq!(
        artifact["contract"]["release_workflow"]["checksum"],
        "sha256"
    );
    assert_eq!(
        artifact["contract"]["release_workflow"]["ci_generated_config_policy"],
        "allow-dirty ci preserves the actions/attest@v4 artifact-metadata permission"
    );
    assert_eq!(
        artifact["contract"]["release_workflow"]["post_announce_jobs"],
        json!(["./homebrew-smoke"])
    );

    let cargo_manifest: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("Cargo.toml")).unwrap()).unwrap();
    assert_eq!(
        cargo_manifest["package"]["description"].as_str(),
        Some("Local-first GitHub Issues retrieval CLI")
    );
    assert_eq!(
        cargo_manifest["package"]["repository"].as_str(),
        Some("https://github.com/juicyjusung/qgh")
    );
    assert_eq!(
        cargo_manifest["package"]["metadata"]["dist"]["dist"].as_bool(),
        Some(true),
        "publish=false package must be explicitly enabled for cargo-dist"
    );
    assert_eq!(
        cargo_manifest["package"]["metadata"]["dist"]["formula"].as_str(),
        Some("qgh")
    );
    assert_eq!(
        cargo_manifest["profile"]["dist"]["inherits"].as_str(),
        Some("release")
    );

    let dist_workspace: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("dist-workspace.toml")).unwrap()).unwrap();
    assert_eq!(
        dist_workspace["dist"]["cargo-dist-version"].as_str(),
        Some("0.32.0")
    );
    assert_eq!(dist_workspace["dist"]["ci"].as_str(), Some("github"));
    assert_eq!(dist_workspace["dist"]["hosting"].as_str(), Some("github"));
    assert_eq!(
        toml_array_strings(&dist_workspace["dist"]["installers"]),
        vec!["homebrew"]
    );
    assert_eq!(
        toml_array_strings(&dist_workspace["dist"]["targets"]),
        vec![
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-unknown-linux-gnu",
        ]
    );
    assert_eq!(
        dist_workspace["dist"]["tap"].as_str(),
        Some("juicyjusung/homebrew-tap")
    );
    assert_eq!(
        toml_array_strings(&dist_workspace["dist"]["publish-jobs"]),
        vec!["homebrew"]
    );
    assert_eq!(
        toml_array_strings(&dist_workspace["dist"]["post-announce-jobs"]),
        vec!["./homebrew-smoke"]
    );
    assert_eq!(
        dist_workspace["dist"]["github-attestations"].as_bool(),
        Some(true)
    );
    assert_eq!(dist_workspace["dist"]["checksum"].as_str(), Some("sha256"));
    assert_eq!(
        toml_array_strings(&dist_workspace["dist"]["allow-dirty"]),
        vec!["ci"]
    );

    let release_workflow = fs::read_to_string(root.join(".github/workflows/release.yml")).unwrap();
    for required in [
        "This file was autogenerated by dist",
        "cargo-dist/releases/download/v0.32.0/cargo-dist-installer.sh",
        "dist build ${{ needs.plan.outputs.tag-flag }} --print=linkage --output-format=json ${{ matrix.dist_args }}",
        "dist build ${{ needs.plan.outputs.tag-flag }} --output-format=json \"--artifacts=global\"",
        "gh release create",
        "actions/attest@v4",
        "\"artifact-metadata\": \"write\"",
        "\"attestations\": \"write\"",
        "\"id-token\": \"write\"",
        "repository: \"juicyjusung/homebrew-tap\"",
        "token: ${{ secrets.HOMEBREW_TAP_TOKEN }}",
        "publish-homebrew-formula",
        "custom-homebrew-smoke",
        "uses: ./.github/workflows/homebrew-smoke.yml",
    ] {
        assert!(
            release_workflow.contains(required),
            "missing release workflow phrase: {required}"
        );
    }

    let smoke_workflow =
        fs::read_to_string(root.join(".github/workflows/homebrew-smoke.yml")).unwrap();
    let formula_url_regex = r#"^ *url "https://github\.com/juicyjusung/qgh/releases/download/v[0-9]+\.[0-9]+\.[0-9]+[^"]*/qgh-[^"]+\.(tar\.gz|tar\.xz|zip)"$"#;
    for required in [
        "workflow_call:",
        "repository: juicyjusung/homebrew-tap",
        formula_url_regex,
        r#"^ *sha256 "[0-9a-f]{64}"$"#,
        "brew install --formula homebrew-tap/Formula/qgh.rb",
        "qgh --version",
        "qgh help",
    ] {
        assert!(
            smoke_workflow.contains(required),
            "missing Homebrew smoke workflow phrase: {required}"
        );
    }
    assert_grep_regex_matches(
        formula_url_regex,
        r#"  url "https://github.com/juicyjusung/qgh/releases/download/v0.1.0/qgh-aarch64-apple-darwin.tar.gz""#,
    );
    assert_grep_regex_matches(
        formula_url_regex,
        r#"    url "https://github.com/juicyjusung/qgh/releases/download/v0.1.0/qgh-x86_64-unknown-linux-gnu.tar.xz""#,
    );
    assert_eq!(
        artifact["verification"],
        json!([
            "Tantivy BM25-only path",
            "strict schema/envelope",
            "human CLI summaries",
            "init output",
            "get batch output",
            "MCP adapter parity smoke",
            "stdout cleanliness",
            "privacy no-egress",
            "DB/index permissions",
            "doctor output",
            "search eval result",
            "one-command Homebrew install",
            "cargo-dist plan/build",
            "generated Homebrew formula smoke",
            "release integrity attestations"
        ])
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
    assert_eq!(
        artifact["schema_snapshots"],
        json!([
            "docs/schemas/envelope.schema.json",
            "docs/schemas/error.schema.json",
            "docs/schemas/init-output.schema.json",
            "docs/schemas/sync-output.schema.json",
            "docs/schemas/query-result.schema.json",
            "docs/schemas/get-output.schema.json",
            "docs/schemas/status-output.schema.json",
            "docs/schemas/doctor-output.schema.json"
        ])
    );
    for path in artifact["schema_snapshots"].as_array().unwrap() {
        let path = path.as_str().unwrap();
        assert!(root.join(path).exists(), "missing schema snapshot: {path}");
        let schema: Value =
            serde_json::from_str(&fs::read_to_string(root.join(path)).unwrap()).unwrap();
        assert_released_schema_objects_are_closed_or_documented(path, &schema);
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
    let query_schema: Value = serde_json::from_str(
        &fs::read_to_string(root.join("docs/schemas/query-result.schema.json")).unwrap(),
    )
    .unwrap();
    let get_schema: Value = serde_json::from_str(
        &fs::read_to_string(root.join("docs/schemas/get-output.schema.json")).unwrap(),
    )
    .unwrap();
    let doctor_schema: Value = serde_json::from_str(
        &fs::read_to_string(root.join("docs/schemas/doctor-output.schema.json")).unwrap(),
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
    assert_eq!(sync_schema["properties"]["sync"]["$ref"], "#/$defs/sync");
    let sync_details = &sync_schema["$defs"]["sync"];
    assert_eq!(sync_details["required"], json!(["last_successful_sync"]));
    assert_eq!(sync_details["additionalProperties"], false);
    assert_eq!(
        schema_property_names(sync_details),
        BTreeSet::from([
            "last_successful_sync".to_string(),
            "max_age_seconds".to_string(),
            "scheduler".to_string(),
            "snapshot_age_seconds".to_string(),
        ])
    );
    assert_eq!(
        sync_details["properties"]["last_successful_sync"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        sync_details["properties"]["snapshot_age_seconds"]["minimum"],
        0
    );
    assert_eq!(sync_details["properties"]["max_age_seconds"]["minimum"], 1);
    assert_eq!(
        sync_details["properties"]["scheduler"]["$ref"],
        "#/$defs/scheduler"
    );
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
        sync_schema["properties"]["comments"]["$ref"],
        "#/$defs/comments"
    );
    let sync_comments = &sync_schema["$defs"]["comments"];
    assert_eq!(sync_comments["required"], json!(["fetched", "upserted"]));
    assert_eq!(sync_comments["additionalProperties"], false);
    assert_eq!(
        schema_property_names(sync_comments),
        BTreeSet::from([
            "added".to_string(),
            "deleted".to_string(),
            "fetched".to_string(),
            "tombstoned".to_string(),
            "updated".to_string(),
            "upserted".to_string(),
        ])
    );
    for field in [
        "fetched",
        "upserted",
        "added",
        "updated",
        "deleted",
        "tombstoned",
    ] {
        assert_eq!(sync_comments["properties"][field]["minimum"], 0);
    }
    assert_eq!(
        sync_schema["properties"]["comment_listing"]["$ref"],
        "#/$defs/comment_listing"
    );
    let sync_comment_listing = &sync_schema["$defs"]["comment_listing"];
    assert_eq!(sync_comment_listing["required"], json!(["mode"]));
    assert_eq!(sync_comment_listing["additionalProperties"], false);
    assert_eq!(
        sync_comment_listing["properties"]["mode"]["enum"],
        json!(["per_issue", "repo_listing"])
    );
    assert_eq!(
        sync_comment_listing["properties"]["skipped_pr_comments"]["minimum"],
        0
    );
    assert_eq!(
        sync_comment_listing["properties"]["deferred_comments"]["minimum"],
        0
    );
    assert_eq!(
        sync_schema["properties"]["reconciliation"]["$ref"],
        "#/$defs/reconciliation"
    );
    let sync_reconciliation = &sync_schema["$defs"]["reconciliation"];
    assert_eq!(sync_reconciliation["required"], json!(["mode"]));
    assert_eq!(sync_reconciliation["additionalProperties"], false);
    assert_eq!(
        sync_reconciliation["properties"]["mode"]["enum"],
        json!(["none", "full", "recent", "targeted_issue"])
    );
    assert_eq!(
        sync_reconciliation["properties"]["checked_sources"]["minimum"],
        0
    );
    assert_eq!(
        sync_reconciliation["properties"]["tombstoned_sources"]["minimum"],
        0
    );
    assert_eq!(
        sync_reconciliation["properties"]["estimated_api_cost_class"]["$ref"],
        "#/$defs/api_cost_class"
    );
    assert_eq!(
        sync_schema["$defs"]["api_cost_class"]["enum"],
        json!(["none", "low", "medium", "high"])
    );
    assert_eq!(
        sync_schema["properties"]["backfill"]["$ref"],
        "#/$defs/backfill"
    );
    let sync_backfill = &sync_schema["$defs"]["backfill"];
    assert_eq!(
        sync_backfill["required"],
        json!([
            "issues",
            "comments",
            "skipped_pull_requests",
            "reached_end",
            "history_cursor",
            "historical_backfill_complete"
        ])
    );
    assert_eq!(sync_backfill["additionalProperties"], false);
    for field in ["issues", "comments", "skipped_pull_requests"] {
        assert_eq!(sync_backfill["properties"][field]["minimum"], 0);
    }
    assert_eq!(
        sync_backfill["properties"]["reached_end"]["type"],
        "boolean"
    );
    assert_eq!(
        sync_backfill["properties"]["history_cursor"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        sync_backfill["properties"]["historical_backfill_complete"]["type"],
        "boolean"
    );
    assert_eq!(
        sync_schema["properties"]["backoff"]["$ref"],
        "#/$defs/backoff"
    );
    let sync_backoff = &sync_schema["$defs"]["backoff"];
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
    assert_eq!(
        sync_schema["properties"]["cursors"]["$ref"],
        "#/$defs/cursors"
    );
    let sync_cursors = &sync_schema["$defs"]["cursors"];
    assert_eq!(
        sync_cursors["required"],
        json!(["updated", "not_modified_endpoints", "watermarks"])
    );
    assert_eq!(sync_cursors["additionalProperties"], false);
    assert_eq!(
        schema_property_names(sync_cursors),
        BTreeSet::from([
            "not_modified_endpoints".to_string(),
            "updated".to_string(),
            "watermarks".to_string(),
        ])
    );
    assert_eq!(sync_cursors["properties"]["updated"]["minimum"], 0);
    assert_eq!(
        sync_cursors["properties"]["not_modified_endpoints"]["minimum"],
        0
    );
    assert_eq!(
        sync_cursors["properties"]["watermarks"]["additionalProperties"]["type"],
        json!(["string", "null"])
    );
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
    assert_eq!(
        status_schema["properties"]["freshness"]["$ref"],
        "#/$defs/freshness"
    );
    assert_eq!(
        status_schema["properties"]["embedding"]["$ref"],
        "#/$defs/embedding"
    );
    let status_freshness = &status_schema["$defs"]["freshness"];
    assert_eq!(
        status_freshness["required"],
        json!([
            "decision",
            "remote_checked",
            "snapshot_age_seconds",
            "max_age_seconds"
        ])
    );
    assert_eq!(status_freshness["additionalProperties"], false);
    assert_eq!(
        schema_property_names(status_freshness),
        BTreeSet::from([
            "decision".to_string(),
            "max_age_seconds".to_string(),
            "remote_checked".to_string(),
            "snapshot_age_seconds".to_string(),
        ])
    );
    assert_eq!(
        status_freshness["properties"]["decision"]["enum"],
        json!(["fresh", "stale_warn", "stale_fail", "never_synced"])
    );
    assert_eq!(
        status_freshness["properties"]["remote_checked"]["const"],
        false
    );
    assert_eq!(
        status_freshness["properties"]["snapshot_age_seconds"]["type"],
        json!(["integer", "null"])
    );
    assert_eq!(
        status_freshness["properties"]["snapshot_age_seconds"]["minimum"],
        0
    );
    assert_eq!(
        status_freshness["properties"]["max_age_seconds"]["type"],
        "integer"
    );
    assert_eq!(
        status_freshness["properties"]["max_age_seconds"]["minimum"],
        1
    );
    assert_eq!(
        status_schema["properties"]["coverage"]["$ref"],
        "#/$defs/coverage"
    );
    let status_coverage = &status_schema["$defs"]["coverage"];
    assert_eq!(
        status_coverage["required"],
        json!([
            "mode",
            "open_cursor",
            "history_cursor",
            "open_backfill_complete",
            "historical_backfill_complete",
            "oldest_synced_updated_at",
            "recent_bootstrap_floor",
            "next_backfill_window_hint"
        ])
    );
    assert_eq!(status_coverage["additionalProperties"], false);
    assert_eq!(
        schema_property_names(status_coverage),
        BTreeSet::from([
            "historical_backfill_complete".to_string(),
            "history_cursor".to_string(),
            "mode".to_string(),
            "next_backfill_window_hint".to_string(),
            "oldest_synced_updated_at".to_string(),
            "open_backfill_complete".to_string(),
            "open_cursor".to_string(),
            "recent_bootstrap_floor".to_string(),
        ])
    );
    assert_eq!(
        status_coverage["properties"]["mode"]["enum"],
        json!(["partial", "complete"])
    );
    assert!(status_coverage["properties"]["mode"]["description"]
        .as_str()
        .unwrap()
        .contains("Derived from the completion flags"));
    for field in [
        "open_cursor",
        "history_cursor",
        "oldest_synced_updated_at",
        "recent_bootstrap_floor",
        "next_backfill_window_hint",
    ] {
        assert_eq!(
            status_coverage["properties"][field]["type"],
            json!(["string", "null"])
        );
    }
    for field in ["open_backfill_complete", "historical_backfill_complete"] {
        assert_eq!(status_coverage["properties"][field]["type"], "boolean");
    }
    assert!(
        status_coverage["properties"]["recent_bootstrap_floor"]["description"]
            .as_str()
            .unwrap()
            .contains("Recent lookback is acceleration")
    );
    let status_embedding = &status_schema["$defs"]["embedding"];
    assert_eq!(
        status_embedding["required"],
        json!(["state", "coverage", "configured_model", "fingerprint"])
    );
    assert_eq!(status_embedding["additionalProperties"], false);
    assert_eq!(
        schema_property_names(status_embedding),
        BTreeSet::from([
            "configured_model".to_string(),
            "coverage".to_string(),
            "fingerprint".to_string(),
            "state".to_string(),
        ])
    );
    assert_eq!(
        status_embedding["properties"]["state"]["enum"],
        json!(["missing", "partial", "complete", "fingerprint_mismatch"])
    );
    assert_eq!(
        status_embedding["properties"]["coverage"]["$ref"],
        "#/$defs/embedding_coverage"
    );
    assert_eq!(
        status_embedding["properties"]["configured_model"]["$ref"],
        "#/$defs/embedding_configured_model"
    );
    assert_eq!(
        status_embedding["properties"]["fingerprint"]["anyOf"][0]["$ref"],
        "#/$defs/embedding_fingerprint"
    );
    assert_eq!(
        status_embedding["properties"]["fingerprint"]["anyOf"][1]["type"],
        "null"
    );
    let embedding_coverage = &status_schema["$defs"]["embedding_coverage"];
    assert_eq!(
        embedding_coverage["required"],
        json!([
            "total_chunks",
            "completed_chunks",
            "missing_chunks",
            "mismatched_chunks"
        ])
    );
    assert_eq!(embedding_coverage["additionalProperties"], false);
    assert_eq!(
        schema_property_names(embedding_coverage),
        BTreeSet::from([
            "completed_chunks".to_string(),
            "mismatched_chunks".to_string(),
            "missing_chunks".to_string(),
            "total_chunks".to_string(),
        ])
    );
    for field in [
        "total_chunks",
        "completed_chunks",
        "missing_chunks",
        "mismatched_chunks",
    ] {
        assert_eq!(embedding_coverage["properties"][field]["minimum"], 0);
    }
    let embedding_configured_model = &status_schema["$defs"]["embedding_configured_model"];
    assert_eq!(
        embedding_configured_model["required"],
        json!([
            "provider",
            "model",
            "model_id",
            "model_revision",
            "model_path"
        ])
    );
    assert_eq!(embedding_configured_model["additionalProperties"], false);
    assert_eq!(
        schema_property_names(embedding_configured_model),
        BTreeSet::from([
            "model".to_string(),
            "model_id".to_string(),
            "model_path".to_string(),
            "model_revision".to_string(),
            "provider".to_string(),
        ])
    );
    assert_eq!(
        embedding_configured_model["properties"]["provider"]["enum"],
        json!(["local"])
    );
    for field in ["model", "model_revision", "model_path"] {
        assert_eq!(
            embedding_configured_model["properties"][field]["type"],
            json!(["string", "null"])
        );
    }
    assert_eq!(
        embedding_configured_model["properties"]["model_id"]["type"],
        "string"
    );
    let embedding_fingerprint = &status_schema["$defs"]["embedding_fingerprint"];
    assert_eq!(
        embedding_fingerprint["required"],
        json!([
            "hash",
            "schema_version",
            "provider",
            "model_id",
            "model_revision",
            "dimension",
            "pooling",
            "query_prefix",
            "chunker_version",
            "source_schema_version",
            "matches_config"
        ])
    );
    assert_eq!(embedding_fingerprint["additionalProperties"], false);
    assert_eq!(
        schema_property_names(embedding_fingerprint),
        BTreeSet::from([
            "chunker_version".to_string(),
            "dimension".to_string(),
            "hash".to_string(),
            "matches_config".to_string(),
            "model_id".to_string(),
            "model_revision".to_string(),
            "pooling".to_string(),
            "provider".to_string(),
            "query_prefix".to_string(),
            "schema_version".to_string(),
            "source_schema_version".to_string(),
        ])
    );
    assert_eq!(
        embedding_fingerprint["properties"]["hash"]["pattern"],
        "^[0-9a-f]{64}$"
    );
    assert_eq!(
        embedding_fingerprint["properties"]["schema_version"]["const"],
        "qgh.embedding_fingerprint.v1"
    );
    assert_eq!(
        embedding_fingerprint["properties"]["provider"]["enum"],
        json!(["local"])
    );
    assert_eq!(
        embedding_fingerprint["properties"]["dimension"]["minimum"],
        1
    );
    assert_eq!(
        embedding_fingerprint["properties"]["pooling"]["enum"],
        json!(["cls", "mean"])
    );
    assert_eq!(
        embedding_fingerprint["properties"]["matches_config"]["type"],
        "boolean"
    );
    assert_eq!(
        query_schema["properties"]["freshness"]["$ref"],
        "#/$defs/freshness"
    );
    assert_eq!(
        query_schema["$defs"]["freshness"],
        status_schema["$defs"]["freshness"]
    );
    assert_eq!(
        query_schema["properties"]["coverage"]["$ref"],
        "#/$defs/coverage"
    );
    assert_eq!(
        query_schema["$defs"]["coverage"],
        status_schema["$defs"]["coverage"]
    );
    assert_eq!(
        query_schema["required"],
        json!([
            "profile_id",
            "freshness",
            "coverage",
            "result_filtering",
            "results"
        ])
    );
    assert_eq!(query_schema["additionalProperties"], false);
    assert_eq!(
        schema_property_names(&query_schema),
        BTreeSet::from([
            "coverage".to_string(),
            "freshness".to_string(),
            "profile_id".to_string(),
            "result_filtering".to_string(),
            "results".to_string(),
        ])
    );
    let query_filtering = &query_schema["properties"]["result_filtering"];
    assert_eq!(query_filtering["required"], json!(["unresolvable_hits"]));
    assert_eq!(query_filtering["additionalProperties"], false);
    assert_eq!(
        query_filtering["properties"]["unresolvable_hits"]["minimum"],
        0
    );
    assert_eq!(
        query_schema["properties"]["results"]["items"]["$ref"],
        "#/$defs/query_result"
    );
    let query_result = &query_schema["$defs"]["query_result"];
    assert_eq!(
        query_result["required"],
        json!([
            "source_id",
            "entity_type",
            "repo",
            "issue_number",
            "canonical_url",
            "snippet",
            "get_args",
            "parent_issue",
            "source_version",
            "ranking"
        ])
    );
    assert_eq!(query_result["additionalProperties"], false);
    assert_eq!(
        schema_property_names(query_result),
        BTreeSet::from([
            "author".to_string(),
            "canonical_url".to_string(),
            "entity_type".to_string(),
            "get_args".to_string(),
            "issue_number".to_string(),
            "parent_issue".to_string(),
            "ranking".to_string(),
            "repo".to_string(),
            "snippet".to_string(),
            "source_id".to_string(),
            "source_version".to_string(),
            "title".to_string(),
        ])
    );
    assert_eq!(
        query_result["properties"]["source_id"]["pattern"],
        "^qgh://[^/]+/(issue|issue-comment)/"
    );
    assert_eq!(
        query_result["properties"]["entity_type"]["enum"],
        json!(["issue", "issue_comment"])
    );
    assert_eq!(
        query_result["properties"]["repo"]["pattern"],
        "^[^/]+/[^/]+$"
    );
    assert_eq!(query_result["properties"]["issue_number"]["minimum"], 1);
    assert_eq!(query_result["properties"]["canonical_url"]["format"], "uri");
    assert!(query_result["properties"]["snippet"]["description"]
        .as_str()
        .unwrap()
        .contains("not citation evidence"));
    let query_get_args = &query_result["properties"]["get_args"];
    assert_eq!(
        query_get_args["required"],
        json!(["source_id", "profile_id"])
    );
    assert_eq!(query_get_args["additionalProperties"], false);
    assert_eq!(query_get_args["properties"]["source_id"]["type"], "string");
    assert_eq!(query_get_args["properties"]["profile_id"]["type"], "string");
    assert_eq!(
        query_result["properties"]["parent_issue"]["oneOf"][0]["type"],
        "null"
    );
    assert_eq!(
        query_result["properties"]["parent_issue"]["oneOf"][1]["$ref"],
        "#/$defs/parent_issue"
    );
    assert_eq!(
        query_result["properties"]["source_version"]["$ref"],
        "#/$defs/source_version"
    );
    assert_eq!(
        query_result["properties"]["ranking"]["$ref"],
        "#/$defs/ranking"
    );
    let query_parent_issue = &query_schema["$defs"]["parent_issue"];
    assert_eq!(
        query_parent_issue["required"],
        json!(["source_id", "repo", "number", "title", "canonical_url"])
    );
    assert_eq!(query_parent_issue["additionalProperties"], false);
    assert_eq!(
        schema_property_names(query_parent_issue),
        BTreeSet::from([
            "canonical_url".to_string(),
            "number".to_string(),
            "repo".to_string(),
            "source_id".to_string(),
            "title".to_string(),
        ])
    );
    assert_eq!(
        query_parent_issue["properties"]["canonical_url"]["format"],
        "uri"
    );
    assert_eq!(
        query_parent_issue["properties"]["source_id"]["pattern"],
        "^qgh://[^/]+/issue/"
    );
    assert_eq!(
        query_parent_issue["properties"]["repo"]["pattern"],
        "^[^/]+/[^/]+$"
    );
    assert_eq!(query_parent_issue["properties"]["number"]["minimum"], 1);
    assert_eq!(
        query_schema["$defs"]["source_version"],
        get_schema["$defs"]["source_version"]
    );
    let query_ranking = &query_schema["$defs"]["ranking"];
    assert_eq!(query_ranking["required"], json!(["kind", "lexical_score"]));
    assert_eq!(query_ranking["additionalProperties"], false);
    assert_eq!(
        schema_property_names(query_ranking),
        BTreeSet::from(["kind".to_string(), "lexical_score".to_string()])
    );
    assert_eq!(
        query_ranking["properties"]["kind"]["enum"],
        json!(["bm25", "exact"])
    );
    assert_eq!(
        query_ranking["properties"]["lexical_score"]["type"],
        json!(["number", "null"])
    );
    assert!(query_ranking["properties"]["lexical_score"]["description"]
        .as_str()
        .unwrap()
        .contains("not confidence or probability"));
    assert_eq!(
        status_schema["properties"]["resolution"]["$ref"],
        "#/$defs/resolution"
    );
    let status_resolution = &status_schema["$defs"]["resolution"];
    assert_eq!(
        status_resolution["required"],
        json!([
            "profile_id",
            "profile_source",
            "effective_repo_scope",
            "repo_source",
            "repo_policy_path",
            "allowlist_match_count"
        ])
    );
    assert_eq!(status_resolution["additionalProperties"], false);
    assert_eq!(
        schema_property_names(status_resolution),
        BTreeSet::from([
            "allowlist_match_count".to_string(),
            "effective_repo_scope".to_string(),
            "profile_id".to_string(),
            "profile_source".to_string(),
            "repo_policy_path".to_string(),
            "repo_source".to_string(),
        ])
    );
    assert_eq!(
        status_resolution["properties"]["profile_source"]["enum"],
        json!(["cli", "env", "single_match"])
    );
    assert_eq!(
        status_resolution["properties"]["effective_repo_scope"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        status_resolution["properties"]["effective_repo_scope"]["pattern"],
        "^[^/]+/[^/]+$"
    );
    assert_eq!(
        status_resolution["properties"]["repo_source"]["enum"],
        json!(["cli", "repo_policy", "git_remote", "command", null])
    );
    assert_eq!(
        status_resolution["properties"]["repo_policy_path"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        status_resolution["properties"]["allowlist_match_count"]["type"],
        json!(["integer", "null"])
    );
    assert_eq!(
        status_resolution["properties"]["allowlist_match_count"]["minimum"],
        0
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
    let get_batch = &get_schema["oneOf"][1];
    assert_eq!(
        get_batch["required"],
        json!(["profile_id", "summary", "lifecycle_check_policy", "items"])
    );
    assert_eq!(get_batch["additionalProperties"], false);
    assert_eq!(
        schema_property_names(get_batch),
        BTreeSet::from([
            "items".to_string(),
            "lifecycle_check_policy".to_string(),
            "profile_id".to_string(),
            "summary".to_string(),
        ])
    );
    let get_batch_summary = &get_batch["properties"]["summary"];
    assert_eq!(
        get_batch_summary["required"],
        json!(["requested", "returned", "failed", "batch_size_cap"])
    );
    assert_eq!(get_batch_summary["additionalProperties"], false);
    assert_eq!(
        schema_property_names(get_batch_summary),
        BTreeSet::from([
            "batch_size_cap".to_string(),
            "failed".to_string(),
            "requested".to_string(),
            "returned".to_string(),
        ])
    );
    assert_eq!(get_batch_summary["properties"]["requested"]["minimum"], 2);
    assert_eq!(get_batch_summary["properties"]["returned"]["minimum"], 0);
    assert_eq!(get_batch_summary["properties"]["failed"]["minimum"], 0);
    assert_eq!(
        get_batch_summary["properties"]["batch_size_cap"]["const"],
        20
    );
    let get_lifecycle_policy = &get_batch["properties"]["lifecycle_check_policy"];
    assert_eq!(
        get_lifecycle_policy["required"],
        json!([
            "verify_lifecycle",
            "mode",
            "max_in_flight_requests",
            "profile_max_in_flight_requests",
            "hard_cap"
        ])
    );
    assert_eq!(get_lifecycle_policy["additionalProperties"], false);
    assert_eq!(
        schema_property_names(get_lifecycle_policy),
        BTreeSet::from([
            "hard_cap".to_string(),
            "max_in_flight_requests".to_string(),
            "mode".to_string(),
            "profile_max_in_flight_requests".to_string(),
            "verify_lifecycle".to_string(),
        ])
    );
    assert_eq!(
        get_lifecycle_policy["properties"]["max_in_flight_requests"]["minimum"],
        0
    );
    assert_eq!(
        get_lifecycle_policy["properties"]["max_in_flight_requests"]["maximum"],
        1
    );
    assert_eq!(
        get_lifecycle_policy["properties"]["profile_max_in_flight_requests"]["minimum"],
        1
    );
    assert_eq!(get_lifecycle_policy["properties"]["hard_cap"]["const"], 16);
    assert_eq!(
        get_schema["oneOf"][1]["properties"]["items"]["maxItems"],
        20
    );
    let get_batch_items = &get_batch["properties"]["items"];
    assert_eq!(get_batch_items["minItems"], 2);
    assert_eq!(get_batch_items["maxItems"], 20);
    let get_batch_success_item = &get_batch_items["items"]["oneOf"][0];
    assert_eq!(
        get_batch_success_item["required"],
        json!(["input_index", "source_id", "ok", "source"])
    );
    assert_eq!(get_batch_success_item["additionalProperties"], false);
    assert_eq!(
        get_batch_success_item["properties"]["input_index"]["minimum"],
        0
    );
    assert_eq!(get_batch_success_item["properties"]["ok"]["const"], true);
    assert_eq!(
        get_batch_success_item["properties"]["source"]["$ref"],
        "#/$defs/source"
    );
    let get_batch_error_item = &get_batch_items["items"]["oneOf"][1];
    assert_eq!(
        get_batch_error_item["required"],
        json!(["input_index", "source_id", "ok", "error"])
    );
    assert_eq!(get_batch_error_item["additionalProperties"], false);
    assert_eq!(
        get_batch_error_item["properties"]["input_index"]["minimum"],
        0
    );
    assert_eq!(get_batch_error_item["properties"]["ok"]["const"], false);
    assert_eq!(
        get_batch_error_item["properties"]["error"]["$ref"],
        "error.schema.json"
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
    assert_eq!(
        get_parent_issue["properties"]["source_id"]["pattern"],
        "^qgh://[^/]+/issue/"
    );
    assert_eq!(
        get_parent_issue["properties"]["repo"]["pattern"],
        "^[^/]+/[^/]+$"
    );
    assert_eq!(get_parent_issue["properties"]["number"]["minimum"], 1);
    assert_eq!(
        get_parent_issue["properties"]["canonical_url"]["format"],
        "uri"
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
    assert_eq!(
        doctor_schema["properties"]["checks"]["items"]["$ref"],
        "#/$defs/check"
    );
    let doctor_check = &doctor_schema["$defs"]["check"];
    assert_eq!(doctor_check["required"], json!(["name", "ok"]));
    assert_eq!(doctor_check["additionalProperties"], false);
    assert_eq!(
        schema_property_names(doctor_check),
        BTreeSet::from([
            "allowlist_match_count".to_string(),
            "headers".to_string(),
            "name".to_string(),
            "ok".to_string(),
            "path".to_string(),
            "profile_id".to_string(),
            "profile_source".to_string(),
            "repo".to_string(),
        ])
    );
    assert_eq!(
        doctor_check["properties"]["name"]["enum"],
        json!([
            "config",
            "file_permissions",
            "sqlite",
            "tantivy",
            "github_auth_reachability",
            "rate_limit_headers",
            "repo_policy",
            "profile_resolution"
        ])
    );
    assert_eq!(
        doctor_check["properties"]["headers"]["$ref"],
        "#/$defs/rate_limit_headers"
    );
    let doctor_rate_limit_headers = &doctor_schema["$defs"]["rate_limit_headers"];
    assert_eq!(
        doctor_rate_limit_headers["required"],
        json!(["x-ratelimit-remaining", "x-ratelimit-reset"])
    );
    assert_eq!(doctor_rate_limit_headers["additionalProperties"], false);
    assert_eq!(
        schema_property_names(doctor_rate_limit_headers),
        BTreeSet::from([
            "x-ratelimit-remaining".to_string(),
            "x-ratelimit-reset".to_string()
        ])
    );
    for header in ["x-ratelimit-remaining", "x-ratelimit-reset"] {
        assert_eq!(
            doctor_rate_limit_headers["properties"][header]["type"],
            json!(["string", "null"])
        );
    }
    assert_eq!(
        doctor_check["properties"]["repo"]["pattern"],
        "^[^/]+/[^/]+$"
    );
    assert_eq!(
        doctor_check["properties"]["profile_source"]["enum"],
        json!(["cli", "env", "single_match"])
    );
    assert_eq!(
        doctor_check["properties"]["allowlist_match_count"]["type"],
        json!(["integer", "null"])
    );
    assert_eq!(
        doctor_check["properties"]["allowlist_match_count"]["minimum"],
        0
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
        "released schema object shapes are closed",
        "documented envelope `data` and error `details` extension points",
        "human CLI summaries",
        "get batch output",
        "init output",
        "MCP adapter parity smoke",
        "stdout cleanliness",
        "privacy no-egress",
        "DB/index permissions",
        "doctor output",
        "search eval result",
        "brew install juicyjusung/tap/qgh",
        "juicyjusung/homebrew-tap",
        "cargo-dist",
        "HOMEBREW_TAP_TOKEN",
        "Homebrew formula smoke",
        "GitHub Artifact Attestations",
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

    let readme = fs::read_to_string(root.join("README.md")).unwrap();
    for required in [
        "brew install juicyjusung/tap/qgh",
        "qgh init -y",
        "qgh sync",
        "qgh query",
        "qgh get",
        "qgh --version",
        "qgh doctor",
    ] {
        assert!(
            readme.contains(required),
            "missing README phrase: {required}"
        );
    }

    let cli_json_contract = fs::read_to_string(root.join("docs/cli-json-contract.md")).unwrap();
    for required in [
        "Released command payload schemas are closed by default",
        "additionalProperties: false",
        "bounded map value schema",
        "error-code-specific diagnostic",
    ] {
        assert!(
            cli_json_contract.contains(required),
            "missing CLI JSON contract phrase: {required}"
        );
    }
}

#[test]
fn bm25_only_build_keeps_embedding_runtime_optional_and_links_sqlite_vec() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let manifest_toml: toml::Value = toml::from_str(&manifest).unwrap();
    let features = manifest_toml["features"].as_table().unwrap();
    assert_eq!(
        features["default"].as_array().unwrap(),
        &Vec::<toml::Value>::new(),
        "default BM25-only build must not enable embedding runtime features"
    );
    let fastembed_provider = features["fastembed-provider"].as_array().unwrap();
    for feature in ["dep:fastembed", "dep:hf-hub"] {
        assert!(
            fastembed_provider
                .iter()
                .any(|value| value.as_str() == Some(feature)),
            "fastembed-provider feature must opt into {feature}"
        );
    }

    let dependencies = manifest_toml["dependencies"].as_table().unwrap();
    for crate_name in ["fastembed", "hf-hub"] {
        assert_eq!(
            dependencies[crate_name]["optional"].as_bool(),
            Some(true),
            "BM25-only build must not require embedding runtime crate `{crate_name}`"
        );
    }
    assert_eq!(
        dependencies["sqlite-vec"].as_str(),
        Some("=0.1.9"),
        "sqlite-vec must stay pinned to the stable static-link crate"
    );
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

fn toml_array_strings(value: &toml::Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap())
        .collect()
}

fn assert_grep_regex_matches(regex: &str, sample: &str) {
    let mut cmd = Command::new("grep");
    cmd.args(["-Eq", regex])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "{sample}").unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert_success(&output);
}

fn assert_released_schema_objects_are_closed_or_documented(path: &str, schema: &Value) {
    let mut violations = Vec::new();
    collect_schema_object_closure_violations(path, schema, &mut Vec::new(), &mut violations);
    assert!(
        violations.is_empty(),
        "released schema object shapes must be closed or documented extension points:\n{}",
        violations.join("\n")
    );
}

fn collect_schema_object_closure_violations(
    schema_path: &str,
    value: &Value,
    json_path: &mut Vec<String>,
    violations: &mut Vec<String>,
) {
    match value {
        Value::Object(object) => {
            let schema_location = format_schema_location(schema_path, json_path);
            if object.get("additionalProperties") == Some(&Value::Bool(true)) {
                violations.push(format!(
                    "{schema_location} explicitly allows additionalProperties: true"
                ));
            }
            if matches!(object.get("type"), Some(Value::String(kind)) if kind == "object")
                && !object.contains_key("additionalProperties")
                && !is_documented_schema_extension_point(schema_path, json_path)
            {
                violations.push(format!(
                    "{schema_location} is an object schema without additionalProperties"
                ));
            }
            for (key, child) in object {
                json_path.push(key.clone());
                collect_schema_object_closure_violations(schema_path, child, json_path, violations);
                json_path.pop();
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                json_path.push(index.to_string());
                collect_schema_object_closure_violations(schema_path, child, json_path, violations);
                json_path.pop();
            }
        }
        _ => {}
    }
}

fn is_documented_schema_extension_point(schema_path: &str, json_path: &[String]) -> bool {
    matches!(
        (schema_path, json_path),
        ("docs/schemas/envelope.schema.json", [data, field])
            if data == "properties" && field == "data"
    ) || matches!(
        (schema_path, json_path),
        ("docs/schemas/error.schema.json", [data, field])
            if data == "properties" && field == "details"
    )
}

fn format_schema_location(schema_path: &str, json_path: &[String]) -> String {
    if json_path.is_empty() {
        format!("{schema_path}:<root>")
    } else {
        format!("{schema_path}:{}", json_path.join("."))
    }
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
