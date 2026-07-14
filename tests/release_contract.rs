use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

#[test]
fn released_error_schema_covers_all_stable_externally_emitted_error_codes() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let error_schema: Value = serde_json::from_str(
        &fs::read_to_string(root.join("docs/schemas/error.schema.json")).unwrap(),
    )
    .unwrap();
    let published = error_schema["$defs"]["error_code"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|code| code.as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    let published_stable = published
        .iter()
        .filter(|code| has_stable_error_prefix(code))
        .cloned()
        .collect::<BTreeSet<_>>();
    assert!(
        !published.contains("embedding.vector_integrity_failed"),
        "content-free BM25 fallback warnings must not be published as error envelope codes"
    );
    let mut expected_contract = stable_external_error_codes_from_source(&root);
    // Reserved as the stable fail-safe identity for an unexpected envelope
    // boundary failure even though no ordinary command path emits it today.
    expected_contract.insert("internal.failure".to_string());
    assert_eq!(
        published_stable, expected_contract,
        "released error schema codes must exactly match stable externally emitted and reserved codes"
    );
    assert_eq!(error_schema["additionalProperties"], false);
}

#[test]
fn error_code_docs_describe_publication_snapshot_failures() {
    let docs =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/error-codes.md"))
            .unwrap();
    for code in [
        "publication.source_snapshot_incomplete",
        "publication.source_snapshot_changed",
        "publication.source_inventory_mismatch",
        "publication.embedding_snapshot_mismatch",
    ] {
        assert!(docs.contains(code), "error code docs must describe {code}");
    }
    assert!(
        !docs.contains("embedding.source_snapshot_missing"),
        "error code docs must not advertise the removed embedding.source_snapshot_missing code"
    );
}

#[cfg(not(feature = "vector-search"))]
#[test]
fn bm25_binary_emits_published_vector_capability_error_envelope() {
    let root = std::env::temp_dir().join(format!(
        "qgh-release-contract-vector-capability-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let config_home = root.join("config");
    let data_home = root.join("data");
    fs::create_dir_all(config_home.join("qgh")).unwrap();
    fs::create_dir_all(&data_home).unwrap();
    fs::write(
        config_home.join("qgh/config.toml"),
        r#"schema_version = "qgh.config.v1"

[embedding]
provider = "local"
model_path = "/not-opened-before-vector-capability-check"
file = "onnx/model.onnx"
pooling = "cls"
query_prefix = "query: "
quantization = "none"

[profiles.work]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["juicyjusung/qgh"]

[profiles.work.token_source]
type = "env"
env = "QGH_RELEASE_CONTRACT_TOKEN"
"#,
    )
    .unwrap();

    let output = Command::new(binary())
        .args(["--profile", "work", "embed", "--force", "--json"])
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("QGH_RELEASE_CONTRACT_TOKEN", "not-a-real-token")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr_text(&output).is_empty());
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        envelope["error"]["code"],
        "embedding.vector_capability_unavailable"
    );
    assert_eq!(envelope["error"]["retryable"], false);
    assert_eq!(envelope["error"]["exit_code"], 2);
    let schema: Value = serde_json::from_str(
        &fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/schemas/error.schema.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(schema["$defs"]["error_code"]["enum"]
        .as_array()
        .unwrap()
        .contains(&envelope["error"]["code"]));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cli_publication_error_envelope_matches_released_schema() {
    let root = std::env::temp_dir().join(format!(
        "qgh-release-contract-publication-error-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let config_home = root.join("config");
    let data_home = root.join("data");
    fs::create_dir_all(config_home.join("qgh")).unwrap();
    fs::create_dir_all(&data_home).unwrap();
    fs::write(
        config_home.join("qgh/config.toml"),
        r#"schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["juicyjusung/qgh"]

[profiles.work.token_source]
type = "env"
env = "QGH_RELEASE_CONTRACT_TOKEN"
"#,
    )
    .unwrap();

    let initialized = Command::new(binary())
        .args(["--profile", "work", "sync", "--json"])
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &data_home)
        .env_remove("QGH_RELEASE_CONTRACT_TOKEN")
        .output()
        .unwrap();
    assert_eq!(initialized.status.code(), Some(3));
    assert!(stderr_text(&initialized).is_empty());
    let initialization_error: Value = serde_json::from_slice(&initialized.stdout).unwrap();
    assert_eq!(
        initialization_error["error"]["code"],
        "auth.token_unavailable"
    );
    let connection = Connection::open(data_home.join("qgh/profiles/work/qgh.sqlite3")).unwrap();
    connection
        .execute_batch(
            r#"
            INSERT INTO repositories (repo, host, owner, name)
            VALUES ('juicyjusung/qgh', 'github.com', 'juicyjusung', 'qgh');

            INSERT INTO source_entities
                (source_id, entity_type, host, repo, node_id, github_id,
                 lifecycle_state, created_at, updated_at, last_seen_at)
            VALUES ('github:issue:release-contract', 'issue', 'github.com',
                    'juicyjusung/qgh', 'I_release_contract_fixture', 47,
                    'active', '2026-07-11T00:00:00Z',
                    '2026-07-11T00:00:00Z', '2026-07-11T00:00:00Z');

            INSERT INTO source_versions
                (source_id, body_hash, github_updated_at, indexed_at, sync_run_id, lifecycle_state)
            VALUES ('github:issue:release-contract',
                    '0000000000000000000000000000000000000000000000000000000000000047',
                    '2026-07-11T00:00:00Z', '2026-07-11T00:00:00Z',
                    'incomplete-release-contract-snapshot', 'active');

            INSERT INTO issue_metadata
                (source_id, repo, issue_number, title, body, state, labels_json,
                 milestone, assignees_json, author, created_at, updated_at,
                 closed_at, canonical_url, latest_version_id)
            VALUES ('github:issue:release-contract', 'juicyjusung/qgh', 47,
                    'Release contract fixture',
                    'Fixture body for publication error envelope validation.',
                    'open', '[]', NULL, '[]', 'fixture',
                    '2026-07-11T00:00:00Z', '2026-07-11T00:00:00Z', NULL,
                    'https://github.com/juicyjusung/qgh/issues/47',
                    (SELECT id FROM source_versions
                     WHERE source_id = 'github:issue:release-contract'));
            "#,
        )
        .unwrap();
    drop(connection);

    let output = Command::new(binary())
        .args(["--profile", "work", "query", "release contract", "--json"])
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_DATA_HOME", &data_home)
        .env("QGH_RELEASE_CONTRACT_TOKEN", "not-a-real-token")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(6));
    assert!(stderr_text(&output).is_empty());
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["schema_version"], "qgh.v1");
    assert_eq!(envelope["ok"], false);
    assert!(envelope.get("data").is_none());
    assert_eq!(
        envelope["error"]["code"],
        "publication.source_snapshot_incomplete"
    );
    let error_schema: Value = serde_json::from_str(
        &fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/schemas/error.schema.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let envelope_schema: Value = serde_json::from_str(
        &fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/schemas/envelope.schema.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_error_envelope_matches_released_schemas(&envelope, &envelope_schema, &error_schema);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn release_contract_artifacts_match_cli_help_and_mcp_surface() {
    let help = qgh(&["--help"]);
    assert_success(&help);
    let help_text = stdout_text(&help);
    assert!(help_text.contains("human output by default"));
    assert!(help_text.contains("use --json for qgh.v1 envelopes"));
    for command in [
        "init", "sync", "embed", "model", "query", "search", "get", "status", "doctor", "mcp",
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
        &["model", "--help"][..],
        &["model", "install", "--help"][..],
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
        let warning_action =
            &tool["outputSchema"]["properties"]["warnings"]["items"]["properties"]["action"];
        assert_eq!(warning_action["additionalProperties"], false);
        assert_eq!(
            warning_action["required"],
            json!(["reason", "command", "json_command"])
        );
        assert!(
            !schema_contains_ref(&tool["outputSchema"], "command-action.schema.json"),
            "MCP outputSchema must inline command actions for offline clients"
        );
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
                        "rerank".to_string(),
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
                    tool["inputSchema"]["properties"]["rerank"]["type"],
                    "boolean"
                );
                assert_eq!(
                    tool["inputSchema"]["properties"]["repo"]["pattern"],
                    "^[^/]+/[^/]+$"
                );
                let query_output_data = &tool["outputSchema"]["properties"]["data"];
                assert_eq!(
                    query_output_data["$id"],
                    "https://github.com/juicyjusung/qgh/raw/main/docs/schemas/query-result.schema.json"
                );
                let mcp_query_ranking = &query_output_data["$defs"]["ranking"];
                assert_eq!(
                    ranking_variant(mcp_query_ranking, "hybrid")["required"],
                    json!([
                        "kind",
                        "lexical_score",
                        "vector_distance",
                        "rrf_rank_score",
                        "final_order_score"
                    ])
                );
                assert_eq!(
                    ranking_variant(mcp_query_ranking, "bm25")["required"],
                    json!(["kind", "lexical_score", "vector_distance"])
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
        json!([
            "init", "sync", "embed", "model", "query", "search", "get", "status", "doctor", "mcp"
        ])
    );
    assert_eq!(
        artifact["contract"]["canonical_cli_commands"],
        json!(["init", "sync", "embed", "model", "query", "get", "status", "doctor"])
    );
    assert_eq!(
        artifact["contract"]["cli_only_commands"],
        json!(["init", "sync", "embed", "model", "doctor"])
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
        json!(["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"])
    );
    assert_eq!(
        artifact["contract"]["optional_semantic"]["new_config_default_preset"],
        "qwen3-embedding-0.6b"
    );
    assert_eq!(
        artifact["contract"]["optional_semantic"]["fusion_profile"],
        "lexical_guard_v1"
    );
    assert_eq!(
        artifact["contract"]["optional_semantic"]["weights_bundled"],
        false
    );
    assert_eq!(
        artifact["contract"]["optional_semantic"]["reranker_default"],
        "off"
    );
    assert_eq!(
        artifact["contract"]["release_integrity_gate"],
        json!([
            "artifact checksums",
            "Homebrew sha256",
            "GitHub Artifact Attestations"
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
        artifact["contract"]["release_workflow"]["cargo_features"],
        json!(["fastembed-provider"])
    );
    assert_eq!(
        artifact["contract"]["release_workflow"]["checksum"],
        "sha256"
    );
    assert_eq!(
        artifact["contract"]["release_workflow"]["ci_generated_config_policy"],
        "allow-dirty ci preserves the hand-edited vX.Y.Z tag trigger and Homebrew smoke workflow"
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
    assert_eq!(
        toml_array_strings(&dist_workspace["dist"]["features"]),
        vec!["fastembed-provider"],
        "release binaries must include the local embedding provider feature"
    );
    assert_eq!(dist_workspace["dist"]["ci"].as_str(), Some("github"));
    assert_eq!(dist_workspace["dist"]["hosting"].as_str(), Some("github"));
    assert_eq!(
        toml_array_strings(&dist_workspace["dist"]["installers"]),
        vec!["homebrew"]
    );
    assert_eq!(
        toml_array_strings(&dist_workspace["dist"]["targets"]),
        vec!["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"]
    );
    assert_eq!(
        dist_workspace["dist"]["github-custom-runners"]["x86_64-unknown-linux-gnu"].as_str(),
        Some("ubuntu-24.04"),
        "ort prebuilt Linux binaries need a glibc 2.38+ runner"
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
        "repository: \"juicyjusung/homebrew-tap\"",
        "token: ${{ secrets.HOMEBREW_TAP_TOKEN }}",
        "Formula/${filename} unchanged",
        "actions/attest@v4",
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
        "Library/Taps/juicyjusung/homebrew-tap",
        "ln -s \"$PWD/homebrew-tap\" \"$tap_dir\"",
        "brew install juicyjusung/tap/qgh",
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
            "optional Qwen hybrid path",
            "optional bounded reranker",
            "fail-closed purge and publication snapshots",
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
            "release integrity checksums"
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
    assert!(artifact["contract"]["not_exposed_to_mcp"]
        .as_array()
        .unwrap()
        .iter()
        .any(|command| command == "model"));
    assert_eq!(
        artifact["schema_snapshots"],
        json!([
            "docs/schemas/envelope.schema.json",
            "docs/schemas/error.schema.json",
            "docs/schemas/init-output.schema.json",
            "docs/schemas/sync-output.schema.json",
            "docs/schemas/embed-output.schema.json",
            "docs/schemas/command-action.schema.json",
            "docs/schemas/model-output.schema.json",
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
    let embed_schema: Value = serde_json::from_str(
        &fs::read_to_string(root.join("docs/schemas/embed-output.schema.json")).unwrap(),
    )
    .unwrap();
    let command_action_schema: Value = serde_json::from_str(
        &fs::read_to_string(root.join("docs/schemas/command-action.schema.json")).unwrap(),
    )
    .unwrap();
    let model_schema: Value = serde_json::from_str(
        &fs::read_to_string(root.join("docs/schemas/model-output.schema.json")).unwrap(),
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
    assert_eq!(model_schema["additionalProperties"], false);
    assert_eq!(embed_schema["additionalProperties"], false);
    assert_eq!(command_action_schema["additionalProperties"], false);
    assert_eq!(
        command_action_schema["required"],
        json!(["reason", "command", "json_command"])
    );
    assert_eq!(
        embed_schema["required"],
        json!(["profile_id", "embedding_state", "chunks"])
    );
    assert_eq!(
        embed_schema["properties"]["embedding_state"]["const"],
        "refreshed"
    );
    assert_eq!(
        embed_schema["properties"]["chunks"]["required"],
        json!(["refreshed", "embedded"])
    );
    assert_eq!(
        model_schema["required"],
        json!([
            "model",
            "purpose",
            "model_id",
            "resolved_revision",
            "action",
            "artifact_count",
            "verified_bytes",
            "manifest_hash",
            "weights_bundled"
        ])
    );
    assert_eq!(
        model_schema["properties"]["weights_bundled"]["const"],
        false
    );
    assert_eq!(
        model_schema["properties"]["manifest_hash"]["pattern"],
        "^[0-9a-f]{64}$"
    );
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
        "validation.max_age_requires_if_stale",
        "validation.repo_required",
        "validation.lifecycle_failed",
        "github.invalid_issue_json",
        "github.invalid_comment_json",
        "github.confirmed_lifecycle_requires_typed_handling",
        "sync.commit_page_failed",
        "sync.backoff",
        "sync.transfer_cycle",
        "sync.transfer_chain_too_long",
        "embedding.source_snapshot_incomplete",
        "purge.failed",
        "purge.retry_failed",
        "purge.read_fenced",
        "purge.write_fenced",
        "purge.successor_blocked",
        "purge.successor_repair_required",
        "purge.successor_snapshot_pending",
        "publication.successor_snapshot_required",
        "publication.tantivy_artifact_not_ready",
    ] {
        assert!(
            error_codes.contains(&json!(code)),
            "released error schema must include {code}"
        );
    }
    assert_eq!(
        sync_schema["properties"]["sync_state"]["enum"],
        json!(["ok", "skipped_fresh"])
    );
    assert_eq!(sync_schema["additionalProperties"], false);
    assert_eq!(
        schema_property_names(&sync_schema),
        BTreeSet::from([
            "backfill".to_string(),
            "comment_listing".to_string(),
            "comments".to_string(),
            "coverage".to_string(),
            "cursors".to_string(),
            "index".to_string(),
            "issues".to_string(),
            "lifecycle".to_string(),
            "profile_id".to_string(),
            "reconciliation".to_string(),
            "scheduler".to_string(),
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
            "open_backfill_complete",
            "historical_backfill_complete",
            "next_action"
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
        sync_backfill["properties"]["open_backfill_complete"]["type"],
        "boolean"
    );
    assert_eq!(
        sync_backfill["properties"]["next_action"]["anyOf"][0]["$ref"],
        "command-action.schema.json"
    );
    assert_eq!(
        sync_backfill["properties"]["next_action"]["anyOf"][1]["type"],
        "null"
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
        status_schema["properties"]["purge"]["$ref"],
        "#/$defs/purge"
    );
    let status_purge = &status_schema["$defs"]["purge"];
    assert_eq!(status_purge["additionalProperties"], false);
    assert_eq!(
        status_purge["required"],
        json!([
            "pending_count",
            "successor_repair_required",
            "retrieval_blocked",
            "target_kinds",
            "triggers",
            "current_stages",
            "failure_stages"
        ])
    );
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
            "next_action",
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
            "next_action".to_string(),
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
    assert_eq!(
        status_coverage["properties"]["next_action"]["anyOf"][0]["$ref"],
        "command-action.schema.json"
    );
    assert_eq!(
        status_coverage["properties"]["next_action"]["anyOf"][1]["type"],
        "null"
    );
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
        json!([
            "state",
            "coverage",
            "configured_model",
            "fingerprint",
            "repair_action"
        ])
    );
    assert_eq!(status_embedding["additionalProperties"], false);
    assert_eq!(
        schema_property_names(status_embedding),
        BTreeSet::from([
            "configured_model".to_string(),
            "coverage".to_string(),
            "fingerprint".to_string(),
            "repair_action".to_string(),
            "state".to_string(),
        ])
    );
    assert_eq!(
        status_embedding["properties"]["state"]["enum"],
        json!([
            "missing",
            "partial",
            "complete",
            "fingerprint_mismatch",
            "corrupt"
        ])
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
    assert_eq!(
        status_embedding["properties"]["repair_action"]["anyOf"][0]["$ref"],
        "command-action.schema.json"
    );
    assert_eq!(
        status_embedding["properties"]["repair_action"]["anyOf"][1]["type"],
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
            "device".to_string(),
            "model_id".to_string(),
            "model_path".to_string(),
            "model_revision".to_string(),
            "provider".to_string(),
            "runtime_profile".to_string(),
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
        json!(["cls", "mean", "last_token"])
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
        sync_schema["properties"]["coverage"]["$ref"],
        "#/$defs/coverage"
    );
    assert_eq!(
        sync_schema["$defs"]["coverage"],
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
            "rerank".to_string(),
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
    let query_rerank = &query_schema["$defs"]["rerank"];
    assert_eq!(query_rerank["oneOf"].as_array().unwrap().len(), 2);
    assert_eq!(
        query_rerank["oneOf"][0]["properties"]["runtime_profile"]["enum"],
        json!(["metal_f32", "cpu_f32"])
    );
    assert_eq!(
        query_rerank["oneOf"][1]["properties"]["repair_action"]["$ref"],
        "command-action.schema.json"
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
    let ranking_variants = query_ranking["oneOf"].as_array().unwrap();
    assert_eq!(ranking_variants.len(), 4);

    let bm25_ranking = ranking_variant(query_ranking, "bm25");
    assert_eq!(
        bm25_ranking["required"],
        json!(["kind", "lexical_score", "vector_distance"])
    );
    assert_eq!(bm25_ranking["additionalProperties"], false);
    assert_eq!(
        schema_property_names(bm25_ranking),
        BTreeSet::from([
            "kind".to_string(),
            "lexical_score".to_string(),
            "pre_rerank_rank".to_string(),
            "rerank_score".to_string(),
            "vector_distance".to_string()
        ])
    );
    assert_eq!(bm25_ranking["properties"]["kind"]["const"], "bm25");
    assert_eq!(
        bm25_ranking["properties"]["lexical_score"]["type"],
        "number"
    );
    assert_eq!(
        bm25_ranking["properties"]["vector_distance"]["type"],
        "null"
    );
    assert!(bm25_ranking["properties"]["lexical_score"]["description"]
        .as_str()
        .unwrap()
        .contains("not confidence or probability"));
    assert_eq!(
        bm25_ranking["dependentRequired"]["rerank_score"],
        json!(["pre_rerank_rank"])
    );

    let vector_ranking = ranking_variant(query_ranking, "vector");
    assert_eq!(
        vector_ranking["required"],
        json!(["kind", "lexical_score", "vector_distance"])
    );
    assert_eq!(vector_ranking["additionalProperties"], false);
    assert_eq!(
        schema_property_names(vector_ranking),
        BTreeSet::from([
            "kind".to_string(),
            "lexical_score".to_string(),
            "pre_rerank_rank".to_string(),
            "rerank_score".to_string(),
            "vector_distance".to_string(),
        ])
    );
    assert_eq!(vector_ranking["properties"]["kind"]["const"], "vector");
    assert_eq!(
        vector_ranking["properties"]["lexical_score"]["type"],
        "null"
    );
    assert_eq!(
        vector_ranking["properties"]["vector_distance"]["type"],
        "number"
    );
    assert!(
        vector_ranking["properties"]["vector_distance"]["description"]
            .as_str()
            .unwrap()
            .contains("not confidence or probability")
    );

    let hybrid_ranking = ranking_variant(query_ranking, "hybrid");
    assert_eq!(
        hybrid_ranking["required"],
        json!([
            "kind",
            "lexical_score",
            "vector_distance",
            "rrf_rank_score",
            "final_order_score"
        ])
    );
    assert_eq!(hybrid_ranking["additionalProperties"], false);
    assert_eq!(
        schema_property_names(hybrid_ranking),
        BTreeSet::from([
            "final_order_score".to_string(),
            "kind".to_string(),
            "lexical_score".to_string(),
            "pre_rerank_rank".to_string(),
            "rerank_score".to_string(),
            "rrf_rank_score".to_string(),
            "vector_distance".to_string(),
        ])
    );
    assert_eq!(hybrid_ranking["properties"]["kind"]["const"], "hybrid");
    assert_eq!(
        hybrid_ranking["properties"]["lexical_score"]["type"],
        json!(["number", "null"])
    );
    assert_eq!(
        hybrid_ranking["properties"]["vector_distance"]["type"],
        json!(["number", "null"])
    );
    assert_eq!(
        hybrid_ranking["properties"]["rrf_rank_score"]["type"],
        "number"
    );
    assert_eq!(
        hybrid_ranking["properties"]["final_order_score"]["type"],
        "number"
    );
    assert!(
        hybrid_ranking["properties"]["final_order_score"]["description"]
            .as_str()
            .unwrap()
            .contains("before optional bounded reranking")
    );
    for field in [
        "lexical_score",
        "vector_distance",
        "rrf_rank_score",
        "final_order_score",
    ] {
        assert!(hybrid_ranking["properties"][field]["description"]
            .as_str()
            .unwrap()
            .contains("not confidence or probability"));
    }
    assert!(hybrid_ranking["properties"]["rerank_score"]["description"]
        .as_str()
        .unwrap()
        .contains("not confidence or probability"));
    assert_eq!(
        hybrid_ranking["dependentRequired"]["pre_rerank_rank"],
        json!(["rerank_score"])
    );

    let exact_ranking = ranking_variant(query_ranking, "exact");
    assert_eq!(
        exact_ranking["required"],
        json!(["kind", "lexical_score", "vector_distance"])
    );
    assert_eq!(exact_ranking["additionalProperties"], false);
    assert_eq!(exact_ranking["properties"]["kind"]["const"], "exact");
    assert_eq!(exact_ranking["properties"]["lexical_score"]["type"], "null");
    assert_eq!(
        exact_ranking["properties"]["vector_distance"]["type"],
        "null"
    );
    assert!(exact_ranking["properties"].get("rerank_score").is_none());
    assert!(exact_ranking["properties"].get("pre_rerank_rank").is_none());
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
            "retry_command",
            "retry_action",
            "retry_after_seconds",
            "reset_at",
            "observed_at",
            "last_successful_sync"
        ])
    );
    assert_eq!(sync_backoff["additionalProperties"], false);
    assert_eq!(
        sync_backoff["properties"]["retry_command"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        sync_backoff["properties"]["retry_action"]["oneOf"][0]["$ref"],
        "command-action.schema.json"
    );
    assert_eq!(
        sync_backoff["properties"]["retry_action"]["oneOf"][1]["type"],
        "null"
    );
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
            "embedding_artifacts",
            "embedding_runtime",
            "embedding_generation",
            "github_auth_reachability",
            "rate_limit_headers",
            "purge",
            "repo_policy",
            "profile_resolution"
        ])
    );
    assert_eq!(
        doctor_schema["properties"]["purge"]["$ref"],
        "#/$defs/purge"
    );
    let doctor_purge = &doctor_schema["$defs"]["purge"];
    assert_eq!(doctor_purge["additionalProperties"], false);
    assert_eq!(
        doctor_purge["properties"]["unmanaged_filesystem_backups"]["const"],
        "not_deleted_by_qgh"
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
        "without a GitHub token",
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
        "repository and release assets to remain public",
    ] {
        assert!(
            checklist.contains(required),
            "missing release checklist phrase: {required}"
        );
    }
    assert!(checklist.contains("credential_store"));
    assert!(checklist.contains("validation.invalid_token_source"));
    assert!(checklist.contains("macOS Apple Silicon and Linux x86_64"));
    assert!(!checklist.contains("macOS Apple Silicon, macOS Intel"));
    assert!(checklist.contains("qwen3-embedding-0.6b"));
    assert!(checklist.contains("lexical_guard_v1"));

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
fn readme_onboarding_matches_released_cli_and_mcp_contracts() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let readme = fs::read_to_string(root.join("README.md")).unwrap();

    for required in [
        "local-first, read-only CLI and MCP",
        "GitHub Issues and issue comments",
        "macOS Apple Silicon",
        "Linux x86_64",
        "brew install juicyjusung/tap/qgh",
        "gh auth status",
        "qgh init -y",
        "qgh sync",
        "qgh sync --all",
        "qgh sync --backfill",
        "coverage: partial",
        "open coverage",
        "historical coverage",
        "coverage.next_action",
        "qgh query",
        "qgh get '<source_id>' --profile-id '<profile_id>'",
        "query -> get -> cite",
        "source candidates, not answers",
        "canonical_url",
        "snippet alone",
        "explicit repository scope",
        "organization-wide scope",
        "no GitHub write-back",
        "qgh.v1",
        "qgh status",
        "qgh doctor",
        "qgh mcp",
        "`query`, `get`, and `status`",
        "npx skills add juicyjusung/qgh --skill qgh --agent codex",
        "Always pass `--skill`",
        "repo-scoped `#N`",
        "`gh issue` task can invoke",
        "qgh's local read-only retrieval/citation layer",
        "`gh`, which is the path for live GitHub truth",
        "brew upgrade juicyjusung/tap/qgh",
        "qgh --version",
    ] {
        assert!(
            readme.contains(required),
            "README must teach released onboarding contract: {required}"
        );
    }

    for relative_path in [
        "docs/cli-json-contract.md",
        "docs/privacy.md",
        "docs/release-checklist.md",
        "docs/local-qwen-models.md",
        "docs/error-codes.md",
        "docs/agent-skills.md",
    ] {
        assert!(
            readme.contains(&format!("]({relative_path})")),
            "README must link to {relative_path}"
        );
        assert!(
            root.join(relative_path).is_file(),
            "README-linked file must exist: {relative_path}"
        );
    }

    let mut remainder = readme.as_str();
    while let Some(link_start) = remainder.find("](") {
        let after_start = &remainder[link_start + 2..];
        let link_end = after_start
            .find(')')
            .expect("README Markdown link must have a closing parenthesis");
        let target = &after_start[..link_end];
        let local_target = target.split('#').next().unwrap();
        if !local_target.is_empty()
            && !local_target.starts_with('#')
            && !local_target.contains("://")
        {
            assert!(
                root.join(local_target).exists(),
                "README relative link target must exist: {target}"
            );
        }
        remainder = &after_start[link_end + 1..];
    }

    for line in readme
        .lines()
        .filter(|line| line.contains("npx skills add juicyjusung/qgh"))
    {
        assert!(
            line.contains("--skill") || line.contains("--list"),
            "README must never advertise a bare repository skill install: {line}"
        );
    }
}

#[test]
fn public_agent_skills_are_discoverable_safe_and_evaluated() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut public_skill_names = fs::read_dir(root.join("skills"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("SKILL.md").is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    public_skill_names.sort();
    assert_eq!(
        public_skill_names,
        ["qgh"],
        "the public Agent Skills catalog must expose one qgh workflow"
    );

    let name = "qgh";
    let skill_dir = root.join("skills").join(name);
    let skill_path = skill_dir.join("SKILL.md");
    let skill = fs::read_to_string(&skill_path)
        .unwrap_or_else(|error| panic!("missing public skill {name}: {error}"));

    assert!(skill.starts_with("---\n"), "qgh must use YAML frontmatter");
    assert!(
        skill.lines().any(|line| line == "name: qgh"),
        "qgh frontmatter name must match its directory"
    );
    assert!(
        skill
            .lines()
            .any(|line| line.starts_with("description: ") && line.len() > 13),
        "qgh must have a non-empty trigger description"
    );
    assert!(
        skill.lines().count() < 500,
        "qgh SKILL.md should stay within progressive-disclosure guidance"
    );
    assert!(
        !skill.contains("metadata:\n  internal: true"),
        "qgh must remain public and discoverable"
    );

    let references = [
        "references/retrieval.md",
        "references/setup-and-recovery.md",
        "references/evidence-research.md",
    ];
    let mut packaged_contract = skill.clone();
    for reference in references {
        let reference_path = skill_dir.join(reference);
        assert!(
            reference_path.is_file(),
            "qgh must ship its routed contract: {reference}"
        );
        assert!(
            skill.contains(&format!("]({reference})")),
            "qgh must link to its routed contract: {reference}"
        );
        packaged_contract.push_str(&fs::read_to_string(reference_path).unwrap());
    }

    for phrase in [
        "query -> get -> cite",
        "get_args.source_id",
        "canonical URL",
        "source version",
        "explicit authorization",
        "token source references",
        "BM25 remains a complete path",
        "Never cite a query snippet as evidence",
        "Use `gh` separately for live GitHub state and authorized mutations",
        "Facts, inference, contradictions, and unknowns are separated",
        "freshness",
        "coverage",
        "qgh sync",
        "qgh doctor",
        "qgh model install",
        "local databases, search indexes",
        "Do not copy or share those artifacts",
        "Issue-Aware Triggering",
        "repository-scoped `#N`",
        "`gh issue`",
        "live-only state",
        "Check `ok` before reading `data.results`",
        "`entity_type`",
        "batch `get`",
        "one to 20",
        "multiple Issue relationships",
    ] {
        assert!(
            packaged_contract.contains(phrase),
            "qgh missing routed safety contract: {phrase}"
        );
    }

    let eval_path = skill_dir.join("evals/evals.json");
    let eval_content = fs::read_to_string(&eval_path)
        .unwrap_or_else(|error| panic!("missing evals for qgh: {error}"));
    let evals: Value = serde_json::from_str(&eval_content)
        .unwrap_or_else(|error| panic!("invalid eval JSON for qgh: {error}"));
    assert_eq!(evals["skill_name"], "qgh");
    let cases = evals["evals"]
        .as_array()
        .unwrap_or_else(|| panic!("qgh evals must be an array"));
    assert_eq!(
        cases.len(),
        8,
        "qgh must cover retrieval, research, multi-Issue batching, setup, recovery, missing binary, and live Issue read/write routing"
    );
    assert!(cases.iter().all(|case| {
        case["prompt"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
            && case["expected_output"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
            && case["expectations"]
                .as_array()
                .is_some_and(|value| !value.is_empty())
            && case["files"].as_array().is_some()
    }));
    assert!(cases.iter().all(|case| {
        let expectations = case["expectations"].as_array().unwrap();
        ["[GATE: route]", "[GATE: authorization]", "[GATE: privacy]"]
            .iter()
            .all(|gate| {
                expectations
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|expectation| expectation.starts_with(gate))
            })
    }));
    let multi_issue_cases = cases
        .iter()
        .filter(|case| case["id"] == 8)
        .collect::<Vec<_>>();
    assert_eq!(
        multi_issue_cases.len(),
        1,
        "qgh must ship exactly one multi-Issue regression eval with id 8"
    );
    let multi_issue_expectations = multi_issue_cases[0]["expectations"]
        .as_array()
        .expect("qgh multi-Issue regression expectations must be an array")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let multi_issue_route_gate = multi_issue_expectations
        .iter()
        .find(|expectation| expectation.starts_with("[GATE: route]"))
        .expect("qgh multi-Issue eval must have a route gate");
    for phrase in ["--issue N", "without constructing a GitHub or GHES URL"] {
        assert!(
            multi_issue_route_gate.contains(phrase),
            "qgh multi-Issue route gate missing locator regression: {phrase}"
        );
    }
    let multi_issue_evidence_gate = multi_issue_expectations
        .iter()
        .find(|expectation| expectation.starts_with("[GATE: evidence]"))
        .expect("qgh multi-Issue eval must have an evidence gate");
    for phrase in ["ok", "data.results", "entity_type", "batch get"] {
        assert!(
            multi_issue_evidence_gate.contains(phrase),
            "qgh multi-Issue evidence gate missing retrieval regression: {phrase}"
        );
    }
    let eval_contract = fs::read_to_string(skill_dir.join("evals/README.md")).unwrap();
    for phrase in [
        "fails when any applicable expectation",
        "regardless of its average expectation score",
        "Check output artifacts directly",
        "routing decision, not proof that a qgh command should run",
        "check_hard_gates.py",
        "run_benchmark.py",
        "both stock workspace layouts",
        "before aggregation",
    ] {
        assert!(
            eval_contract.contains(phrase),
            "qgh eval contract missing hard-gate rule: {phrase}"
        );
    }
    let gate_checker = fs::read_to_string(skill_dir.join("evals/check_hard_gates.py"))
        .expect("qgh must ship an executable hard-gate postprocessor");
    for phrase in [
        "GATE_PREFIX = \"[GATE:\"",
        "load_expected_gates",
        "target_configuration in path.relative_to(workspace).parts",
        "missing = [text for text in expected if text not in graded]",
        "missing_evals",
        "type(eval_id) is not int",
        "malformed hard gate",
        "evidence.strip()",
        "return 0 if report[\"ok\"] else 1",
        "report_path(grading_path, workspace)",
    ] {
        assert!(
            gate_checker.contains(phrase),
            "qgh hard-gate postprocessor missing contract: {phrase}"
        );
    }
    let benchmark_wrapper = fs::read_to_string(skill_dir.join("evals/run_benchmark.py"))
        .expect("qgh must ship a gated benchmark wrapper");
    for phrase in [
        "check_workspace",
        "target_configuration",
        "hard gates failed; benchmark aggregation blocked",
        "scripts/aggregate_benchmark.py",
        "benchmark[\"hard_gates\"]",
        "skill_path = \"skills/qgh\"",
        "find_artifact_privacy_leaks",
        "evaluation_output_paths",
        "baseline_configuration",
        "artifact privacy check failed",
    ] {
        assert!(
            benchmark_wrapper.contains(phrase),
            "qgh benchmark wrapper missing contract: {phrase}"
        );
    }
    assert!(
        skill_dir.join("evals/test_hard_gates.py").is_file(),
        "qgh must ship executable regressions for its benchmark gates"
    );
    let gate_tests = fs::read_to_string(skill_dir.join("evals/test_hard_gates.py"))
        .expect("qgh must ship readable benchmark gate regressions");

    let trigger_path = skill_dir.join("evals/trigger-evals.json");
    let trigger_content = fs::read_to_string(&trigger_path)
        .unwrap_or_else(|error| panic!("missing trigger evals for qgh: {error}"));
    let trigger_cases: Value = serde_json::from_str(&trigger_content)
        .unwrap_or_else(|error| panic!("invalid trigger eval JSON for qgh: {error}"));
    let trigger_cases = trigger_cases
        .as_array()
        .unwrap_or_else(|| panic!("qgh trigger evals must be an array"));
    let positives = trigger_cases
        .iter()
        .filter(|case| case["should_trigger"] == true)
        .count();
    let negatives = trigger_cases
        .iter()
        .filter(|case| case["should_trigger"] == false)
        .count();
    assert_eq!(trigger_cases.len(), 30);
    assert_eq!(positives, 15);
    assert_eq!(negatives, 15);
    assert!(trigger_cases.iter().all(|case| {
        case["query"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
            && case["should_trigger"].is_boolean()
    }));

    for private_marker in [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "Authorization: Bearer",
    ] {
        assert!(
            !packaged_contract.contains(private_marker)
                && !eval_content.contains(private_marker)
                && !trigger_content.contains(private_marker)
                && !eval_contract.contains(private_marker)
                && !gate_checker.contains(private_marker)
                && !benchmark_wrapper.contains(private_marker)
                && !gate_tests.contains(private_marker),
            "qgh must not contain token-like fixture content"
        );
    }
    for local_path_marker in ["/Users/", "/home/", "/private/", "/var/folders/"] {
        assert!(
            !packaged_contract.contains(local_path_marker)
                && !eval_content.contains(local_path_marker)
                && !trigger_content.contains(local_path_marker)
                && !eval_contract.contains(local_path_marker)
                && !gate_checker.contains(local_path_marker)
                && !benchmark_wrapper.contains(local_path_marker)
                && !gate_tests.contains(local_path_marker),
            "qgh must not contain user-local path fixture content"
        );
    }

    let skills_lock: Value = serde_json::from_str(
        &fs::read_to_string(root.join("skills-lock.json")).unwrap_or_else(|_| "{}".into()),
    )
    .unwrap();
    for public_skill in [
        "qgh",
        "using-qgh-context",
        "setting-up-qgh",
        "researching-with-qgh",
    ] {
        assert!(
            skills_lock["skills"].get(public_skill).is_none(),
            "repo-owned public skill must not be recorded as an installed dependency: {public_skill}"
        );
    }

    for internal_skill in [
        ".agents/skills/grill-drive/SKILL.md",
        ".agents/skills/issue-grill-with-docs/SKILL.md",
        ".codex/skills/issue-triage/SKILL.md",
        ".codex/skills/loop-budget/SKILL.md",
        ".codex/skills/loop-constraints/SKILL.md",
        ".codex/skills/loop-triage/SKILL.md",
    ] {
        let content = fs::read_to_string(root.join(internal_skill)).unwrap();
        assert!(
            content.contains("metadata:\n  internal: true"),
            "maintainer-only skill must stay hidden from default public discovery: {internal_skill}"
        );
    }
}

#[test]
fn cli_help_teaches_workflow_and_side_effect_boundaries() {
    let top_level = stdout_text(&qgh(&["--help"]));
    for workflow_step in [
        "qgh init",
        "qgh sync",
        "qgh query",
        "qgh get",
        "cite",
        "qgh status",
        "--json",
    ] {
        assert!(
            top_level.contains(workflow_step),
            "top-level help must teach workflow step {workflow_step}:\n{top_level}"
        );
    }
    for description in [
        "Sync GitHub Issues/comments and refresh local search",
        "Rebuild all local vector embeddings",
        "Search the local snapshot for source candidates",
        "Open authoritative local sources before citing them",
        "Inspect local search readiness without network access",
        "Probe GitHub connectivity and local model health",
        "Serve the read-only query/get/status MCP tools over stdio",
    ] {
        assert!(
            top_level.contains(description),
            "top-level help must teach: {description}\n{top_level}"
        );
    }

    let sync = stdout_text(&qgh(&["sync", "--help"]));
    assert!(sync.contains("incremental embeddings"));
    assert!(sync.contains("Sync one explicit owner/repo"));
    assert!(sync.contains("This command contacts GitHub"));
    assert!(sync.contains("may purge qgh-managed local data"));
    assert!(sync.contains("transient failures do not"));
    assert!(sync.contains("one budgeted historical pass"));
    assert!(sync.contains("repeat until coverage is complete"));
    assert!(sync.contains("default 7d"));
    assert!(sync.contains("confirmed unavailable sources may be purged locally"));
    assert!(sync.contains("Hide progress on stderr"));
    assert!(sync.contains("keep the final human summary plain"));

    let embed = stdout_text(&qgh(&["embed", "--help"]));
    assert!(embed.contains("advanced full rebuild"));
    assert!(embed.contains("Normal sync updates embeddings incrementally"));
    assert!(embed.contains("keep the final human summary plain"));

    let model = stdout_text(&qgh(&["model", "--help"]));
    assert!(model.contains("global local model store"));
    assert!(model.contains("--profile is not valid"));
    let install = stdout_text(&qgh(&["model", "install", "--help"]));
    assert!(install.contains("Download and verify"));
    assert!(install.contains("repository content is never sent"));

    let init = stdout_text(&qgh(&["init", "--help"]));
    for explanation in [
        "owner/repo",
        "Accept inferred defaults",
        "GitHub host",
        "GitHub REST API base URL",
        "GitHub web base URL",
        "token source reference",
        "environment variable name",
        "Overwrite an existing .qgh.toml repository policy",
    ] {
        assert!(
            init.contains(explanation),
            "missing init help: {explanation}"
        );
    }
    let init_repo = stdout_text(&qgh(&["init", "repo", "--help"]));
    assert!(init_repo.contains("repository policy only"));
    assert!(init_repo.contains("Overwrite an existing .qgh.toml"));

    let sync_issue = stdout_text(&qgh(&["sync", "issue", "--help"]));
    assert!(sync_issue.contains("Refresh one issue and its comments"));
    assert!(sync_issue.contains("confirmed lifecycle changes"));

    let get = stdout_text(&qgh(&["get", "--help"]));
    assert!(get.contains("contacts GitHub"));
    assert!(get.contains("purges confirmed unavailable local content"));

    let query = stdout_text(&qgh(&["query", "--help"]));
    assert!(!query.contains("--wiki"));

    let status = stdout_text(&qgh(&["status", "--help"]));
    assert!(status.contains("without network access"));
    let doctor = stdout_text(&qgh(&["doctor", "--help"]));
    assert!(doctor.contains("contacts GitHub"));
    assert!(doctor.contains("loads the configured local model runtime"));

    let profile_with_model = qgh(&[
        "--profile",
        "work",
        "model",
        "install",
        "qwen3-embedding-0.6b",
        "--json",
    ]);
    assert_eq!(profile_with_model.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&profile_with_model.stdout).unwrap();
    assert_eq!(error["error"]["code"], "validation.cli");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("global model store"));
}

#[test]
fn mcp_surface_teaches_query_get_cite_without_write_tools() {
    let output = mcp([
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "qgh-agent-ux-test", "version": "0"}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    ]);
    assert_success(&output);
    let messages = stdout_json_lines(&output);
    let instructions = messages[0]["result"]["instructions"].as_str().unwrap();
    for required in [
        "query -> get -> cite",
        "source candidates, not answers",
        "local snapshot",
        "query, get, and status are local-only",
        "does not write to GitHub",
        "does not expose sync, embed, model, or doctor tools",
    ] {
        assert!(
            instructions.contains(required),
            "MCP instructions must explain {required}: {instructions}"
        );
    }

    let tools = messages[1]["result"]["tools"].as_array().unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["query", "get", "status"]
    );
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let description = tool["description"].as_str().unwrap();
        match name {
            "query" => {
                assert!(description.contains("source candidates"));
                assert!(description.contains("get_args"));
            }
            "get" => {
                assert!(description.contains("authoritative full source"));
                assert!(description.contains("canonical URL"));
                assert!(description.contains("does not contact GitHub"));
            }
            "status" => {
                assert!(description.contains("local-only"));
                assert!(description.contains("does not contact GitHub"));
            }
            _ => unreachable!(),
        }
        for (property, schema) in tool["inputSchema"]["properties"].as_object().unwrap() {
            assert!(
                schema["description"]
                    .as_str()
                    .is_some_and(|description| !description.is_empty()),
                "MCP {name}.{property} must be self-describing"
            );
        }
        assert_eq!(
            tool["outputSchema"]["properties"]["error"]["$id"],
            "https://github.com/juicyjusung/qgh/raw/main/docs/schemas/error.schema.json"
        );
        let expected_data_schema = match name {
            "query" => "query-result.schema.json",
            "get" => "get-output.schema.json",
            "status" => "status-output.schema.json",
            _ => unreachable!(),
        };
        assert!(tool["outputSchema"]["properties"]["data"]["$id"]
            .as_str()
            .unwrap()
            .ends_with(expected_data_schema));
    }
}

#[test]
fn bm25_only_build_excludes_vector_runtime_dependencies() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let manifest_toml: toml::Value = toml::from_str(&manifest).unwrap();
    let features = manifest_toml["features"].as_table().unwrap();
    assert_eq!(
        features["default"].as_array().unwrap(),
        &Vec::<toml::Value>::new(),
        "default BM25-only build must not enable embedding runtime features"
    );
    let vector_search = features["vector-search"].as_array().unwrap();
    assert_eq!(
        vector_search,
        &[toml::Value::String("dep:sqlite-vec".to_string())],
        "vector-search must own the optional sqlite-vec dependency"
    );
    let fastembed_provider = features["fastembed-provider"].as_array().unwrap();
    for feature in ["vector-search", "dep:fastembed", "dep:hf-hub"] {
        assert!(
            fastembed_provider
                .iter()
                .any(|value| value.as_str() == Some(feature)),
            "fastembed-provider feature must opt into {feature}"
        );
    }

    let dependencies = manifest_toml["dependencies"].as_table().unwrap();
    for crate_name in ["fastembed", "hf-hub", "sqlite-vec"] {
        assert_eq!(
            dependencies[crate_name]["optional"].as_bool(),
            Some(true),
            "BM25-only build must not require vector runtime crate `{crate_name}`"
        );
    }

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let tree = Command::new(cargo)
        .current_dir(&root)
        .args([
            "tree",
            "--no-default-features",
            "--edges",
            "normal",
            "--prefix",
            "none",
        ])
        .output()
        .unwrap();
    assert_success(&tree);
    let tree = stdout_text(&tree);
    for excluded in ["sqlite-vec", "fastembed", "hf-hub", "ort ", "ort-sys"] {
        assert!(
            !tree.contains(excluded),
            "BM25-only dependency graph unexpectedly contains `{excluded}`:\n{tree}"
        );
    }
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

fn ranking_variant<'a>(ranking_schema: &'a Value, kind: &str) -> &'a Value {
    ranking_schema["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .find(|variant| variant["properties"]["kind"]["const"] == kind)
        .unwrap_or_else(|| panic!("missing ranking variant: {kind}"))
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

fn schema_contains_ref(schema: &Value, reference: &str) -> bool {
    match schema {
        Value::Object(object) => {
            object.get("$ref").and_then(Value::as_str) == Some(reference)
                || object
                    .values()
                    .any(|child| schema_contains_ref(child, reference))
        }
        Value::Array(items) => items
            .iter()
            .any(|child| schema_contains_ref(child, reference)),
        _ => false,
    }
}

fn assert_error_envelope_matches_released_schemas(
    envelope: &Value,
    envelope_schema: &Value,
    error_schema: &Value,
) {
    assert_eq!(envelope_schema["type"], "object");
    assert_eq!(envelope_schema["additionalProperties"], false);
    let object = envelope
        .as_object()
        .expect("CLI error envelope must be an object");
    let actual_fields = object.keys().cloned().collect::<BTreeSet<_>>();
    let mut required_fields = envelope_schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| field.as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    required_fields.insert("error".to_string());
    assert_eq!(
        actual_fields, required_fields,
        "CLI error envelope must have exactly the released strict schema fields"
    );
    assert_eq!(
        envelope["schema_version"],
        envelope_schema["properties"]["schema_version"]["const"]
    );
    assert_eq!(envelope["ok"], false);
    assert!(envelope["warnings"].is_array());
    assert!(envelope["meta"].is_object());
    assert_error_matches_released_schema(&envelope["error"], error_schema);
}

fn assert_error_matches_released_schema(error: &Value, schema: &Value) {
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    let object = error.as_object().expect("CLI error must be an object");
    let actual_fields = object.keys().cloned().collect::<BTreeSet<_>>();
    let required_fields = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| field.as_str().unwrap().to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        actual_fields, required_fields,
        "CLI error object must have exactly the released strict schema fields"
    );
    assert!(error["message"].is_string());
    assert!(error["details"].is_object());
    assert!(error["hint"].is_null() || error["hint"].is_string());
    assert!(error["retryable"].is_boolean());
    assert!(error["exit_code"].is_i64());
    assert!(schema["$defs"]["error_code"]["enum"]
        .as_array()
        .unwrap()
        .contains(&error["code"]));
    assert!(schema["properties"]["exit_code"]["enum"]
        .as_array()
        .unwrap()
        .contains(&error["exit_code"]));
}

fn stable_external_error_codes_from_source(root: &std::path::Path) -> BTreeSet<String> {
    const WARNING_CODES: &[&str] = &[
        "embedding.artifact_corrupt",
        "embedding.coverage_missing",
        "embedding.coverage_partial",
        "embedding.fingerprint_mismatch",
        "embedding.generation_cleanup_failed",
        "embedding.query_dimension_mismatch",
        "embedding.query_encoding_failed",
        "embedding.runtime_unavailable",
        "embedding.sync_chunking_failed",
        "embedding.sync_tokenizer_failed",
        "embedding.sync_vector_init_failed",
        "embedding.tombstone_cleanup_failed",
        "embedding.vector_init_failed",
        "embedding.vector_integrity_failed",
        "embedding.vector_search_failed",
        "config.duplicate_repo_allowlist",
        "config.profile_not_checked",
        "freshness.active_issue_snapshot_stale",
        "freshness.never_synced",
        "freshness.query_snapshot_stale",
        "publication.activation_failed",
        "publication.incomplete_snapshot_deferred",
    ];
    const DEBUG_OR_TEST_ONLY_CODES: &[&str] = &["embedding.generation_cleanup_injected_failure"];
    const NON_ERROR_LITERALS: &[&str] = &[
        "config.json",
        "config.toml",
        "github.com",
        "model.onnx",
        "model.onnx_data",
        "model.safetensors",
    ];
    let warning_codes = WARNING_CODES.iter().copied().collect::<BTreeSet<_>>();
    let debug_or_test_only_codes = DEBUG_OR_TEST_ONLY_CODES
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let non_error_literals = NON_ERROR_LITERALS.iter().copied().collect::<BTreeSet<_>>();
    let mut codes = BTreeSet::new();
    for entry in fs::read_dir(root.join("src")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
            continue;
        }
        let source = fs::read_to_string(path).unwrap();
        // Ignore only the top-level test module, never inline `#[cfg(test)]`
        // fields or blocks. In particular, store.rs contains production error
        // constructors thousands of lines after its first inline test cfg.
        let production = production_source_before_top_level_test_module(&source);
        for prefix in stable_error_prefixes() {
            let needle = format!("\"{prefix}");
            let mut rest = production;
            while let Some(offset) = rest.find(&needle) {
                rest = &rest[offset + 1..];
                let end = rest.find('"').expect("stable error code literal is closed");
                let code = &rest[..end];
                if is_error_code_literal(code)
                    && !code.contains(".test_")
                    && !warning_codes.contains(code)
                    && !debug_or_test_only_codes.contains(code)
                    && !non_error_literals.contains(code)
                {
                    codes.insert(code.to_string());
                }
                rest = &rest[end + 1..];
            }
        }
    }
    codes
}

fn is_error_code_literal(code: &str) -> bool {
    let Some((_, name)) = code.split_once('.') else {
        return false;
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn has_stable_error_prefix(code: &str) -> bool {
    stable_error_prefixes()
        .iter()
        .any(|prefix| code.starts_with(prefix))
}

fn stable_error_prefixes() -> &'static [&'static str] {
    &[
        "auth.",
        "config.",
        "embedding.",
        "freshness.",
        "github.",
        "index.",
        "internal.",
        "model.",
        "publication.",
        "purge.",
        "source.",
        "storage.",
        "sync.",
        "validation.",
    ]
}

fn production_source_before_top_level_test_module(source: &str) -> &str {
    let mut offset = 0;
    let lines = source.split_inclusive('\n').collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        if line.trim_end() == "#[cfg(test)]"
            && lines
                .get(index + 1)
                .is_some_and(|next| next.trim_start().starts_with("mod "))
        {
            return &source[..offset];
        }
        offset += line.len();
    }
    source
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
