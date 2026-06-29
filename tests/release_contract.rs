use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

#[test]
fn release_contract_artifacts_match_cli_help_and_mcp_surface() {
    let help = qgh(&["--help"]);
    assert_success(&help);
    let help_text = stdout_text(&help);
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
        "init output",
        "MCP read-only tools",
        "stdout cleanliness",
        "privacy no-egress",
        "DB/index permissions",
        "doctor output",
        "search eval result",
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
