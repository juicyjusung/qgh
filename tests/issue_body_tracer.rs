use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{
    atomic::{AtomicBool, Ordering},
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
    assert_eq!(sync_json["data"]["index"]["dirty_task_count"], 0);
    fixture.assert_sqlite_issue_metadata();

    let status = fixture.qgh(["status", "--json"]);
    assert_success(&status);
    let status_json = stdout_json(&status);
    assert_eq!(status_json["ok"], true);
    assert_eq!(status_json["data"]["profile_id"], "work");
    assert_eq!(status_json["data"]["sources"]["issue_count"], 1);
    assert_eq!(
        status_json["data"]["database"]["schema_version"],
        "qgh.db.v1"
    );
    assert_eq!(status_json["data"]["index"]["active_generation"], 1);
    assert_eq!(status_json["data"]["index"]["dirty_task_count"], 0);
    assert!(status_json["data"]["sync"]["last_sync_at"]
        .as_str()
        .is_some());
    assert_eq!(server.request_count(), 1, "status must be local-only");

    let query = fixture.qgh(["query", "BM25 tracer", "--json"]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    let result = &query_json["data"]["results"][0];
    let source_id = "qgh://github.com/issue/I_kwDOISSUE1";
    assert_eq!(result["source_id"], source_id);
    assert_eq!(result["entity_type"], "issue");
    assert_eq!(
        result["canonical_url"],
        "https://github.com/owner/repo/issues/42"
    );
    assert_eq!(result["get_args"]["source_id"], source_id);
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

    let get = fixture.qgh(["get", source_id, "--json"]);
    assert_success(&get);
    let get_json = stdout_json(&get);
    let source = &get_json["data"]["source"];
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
        "comments": 0,
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
    } else {
        r#"{"message":"not found"}"#
    };
    let status = if body == issue_payload {
        "200 OK"
    } else {
        "404 Not Found"
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nx-ratelimit-remaining: 4999\r\n\r\n{body}",
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

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
