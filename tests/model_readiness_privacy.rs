use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

const PROFILE_SECRET: &str = "QGH_PROFILE_SECRET_7d0df87c";
const POLICY_SECRET: &str = "QGH_POLICY_SECRET_418ee7ad";

#[test]
fn invalid_profile_toml_never_echoes_source_values() {
    let fixture = CliFixture::new("profile-secret");
    fixture.write_config(&format!(
        r#"schema_version = "qgh.config.v1"

[profiles.work]
host = ["{PROFILE_SECRET}"]
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#
    ));

    for json in [true, false] {
        let output = fixture.status(json);
        assert_eq!(output.status.code(), Some(2));
        assert_output_redacted(&output, PROFILE_SECRET);
    }
}

#[test]
fn invalid_repo_policy_toml_never_echoes_source_values() {
    let fixture = CliFixture::new("policy-secret");
    fixture.write_valid_config();
    fixture.init_git_worktree();
    fs::write(
        fixture.root.join(".qgh.toml"),
        format!(
            r#"schema_version = "qgh.repo.v1"

[repo]
github = ["{POLICY_SECRET}"]
"#
        ),
    )
    .unwrap();

    for json in [true, false] {
        let output = fixture.status(json);
        assert_eq!(output.status.code(), Some(2));
        assert_output_redacted(&output, POLICY_SECRET);
    }
}

#[test]
fn explicit_manifest_artifact_readiness_does_not_block_status_or_get() {
    let fixture = CliFixture::new("manifest-artifact-readiness");
    let model_root = fixture.root.join("prepared-model");
    fs::create_dir_all(&model_root).unwrap();
    let manifest_path = model_root.join("manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&explicit_manifest_fixture()).unwrap(),
    )
    .unwrap();
    fixture.write_config_with_manifest(&manifest_path);

    let missing = fixture.status(true);
    assert!(
        missing.status.success(),
        "{}",
        String::from_utf8_lossy(&missing.stdout)
    );
    let missing_json: serde_json::Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(missing_json["data"]["embedding"]["state"], "missing");

    fs::write(model_root.join("model.onnx"), b"x").unwrap();
    let truncated = fixture.status(true);
    assert!(truncated.status.success());
    let truncated_json: serde_json::Value = serde_json::from_slice(&truncated.stdout).unwrap();
    assert_eq!(truncated_json["data"]["embedding"]["state"], "missing");

    let get = fixture.get_missing_source();
    assert_eq!(get.status.code(), Some(4));
    let get_json: serde_json::Value = serde_json::from_slice(&get.stdout).unwrap();
    assert_eq!(get_json["error"]["code"], "source.not_found");
}

fn explicit_manifest_fixture() -> serde_json::Value {
    let artifact = |role: &str, relative_path: &str| {
        json!({
            "role": role,
            "relative_path": relative_path,
            "sha256": "0".repeat(64),
            "byte_size": 8
        })
    };
    json!({
        "schema_version": "qgh.model_manifest.v1",
        "preset_id": null,
        "provider": "fastembed",
        "model_source": {"type": "local", "declared_id": "fixture"},
        "artifacts": [
            artifact("onnx_model", "model.onnx"),
            artifact("tokenizer", "tokenizer.json"),
            artifact("config", "config.json"),
            artifact("special_tokens_map", "special_tokens_map.json"),
            artifact("tokenizer_config", "tokenizer_config.json")
        ],
        "tokenizer": "hf_tokenizer_json",
        "query_prefix": "",
        "document_prefix": "",
        "pooling": "cls",
        "normalization": "l2",
        "native_dimension": 4,
        "output_dimension": 4,
        "max_length": 32,
        "quantization": "none",
        "context_template_version": "qgh.context.v1"
    })
}

fn assert_output_redacted(output: &Output, marker: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains(marker), "stdout leaked marker: {stdout}");
    assert!(!stderr.contains(marker), "stderr leaked marker: {stderr}");
}

struct CliFixture {
    root: PathBuf,
    config_home: PathBuf,
    data_home: PathBuf,
    cache_home: PathBuf,
}

impl CliFixture {
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

    fn write_valid_config(&self) {
        self.write_config(
            r#"schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#,
        );
    }

    fn write_config_with_manifest(&self, manifest_path: &std::path::Path) {
        self.write_config(&format!(
            r#"schema_version = "qgh.config.v1"

[embedding]
provider = "local"
manifest_path = "{}"

[profiles.work]
host = "github.com"
api_base_url = "https://api.github.com"
web_base_url = "https://github.com"
repos = ["owner/repo"]

[profiles.work.token_source]
type = "env"
env = "QGH_TEST_TOKEN"
"#,
            manifest_path.display()
        ));
    }

    fn write_config(&self, text: &str) {
        fs::write(self.config_home.join("qgh/config.toml"), text).unwrap();
    }

    fn init_git_worktree(&self) {
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&self.root)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn status(&self, json: bool) -> Output {
        let mut command = self.base_command();
        command.args(["--profile", "work", "status"]);
        if json {
            command.arg("--json");
        }
        command.output().unwrap()
    }

    fn get_missing_source(&self) -> Output {
        let mut command = self.base_command();
        command.args([
            "--profile",
            "work",
            "get",
            "qgh://github.com/issue/I_missing",
            "--json",
        ]);
        command.output().unwrap()
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new(binary());
        command
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_DATA_HOME", &self.data_home)
            .env("XDG_CACHE_HOME", &self.cache_home)
            .env("QGH_TEST_TOKEN", "fixture-token")
            .env_remove("QGH_PROFILE")
            .env_remove("RUST_LOG")
            .current_dir(&self.root);
        command
    }
}

fn binary() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_qgh")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut path = std::env::current_exe().unwrap();
            path.pop();
            if path.ends_with("deps") {
                path.pop();
            }
            path.push("qgh");
            path
        })
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "qgh-model-readiness-{name}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}
