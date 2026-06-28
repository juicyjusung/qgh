use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

const LABELER: &str = "qgh synthetic fixture maintainer";
const LABELING_RULE: &str =
    "Gold source_id is the single issue or issue comment whose fixture body answers the query.";
const AMBIGUOUS_EXCLUSION_RULE: &str =
    "Exclude ambiguous queries when more than one active source is a plausible gold answer.";
const WIKI_EXCLUDED_REASON: &str = "Wiki is post-MVP and excluded from the MVP eval fixture.";

#[test]
fn curated_search_quality_eval_gate_passes() {
    let fixture = EvalFixture::new("search-quality-eval");
    let server = EvalFakeGitHub::start();
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(&["sync", "--json"]));

    let mut report = EvalReport::default();
    for case in eval_cases() {
        assert!(!case.labeler.is_empty());
        assert!(!case.labeling_rule.is_empty());
        assert!(!case.ambiguous_exclusion_rule.is_empty());

        let query = fixture.qgh(&["query", case.query, "--limit", "5", "--json"]);
        assert_success(&query);
        let query_json = stdout_json(&query);
        let results = query_json["data"]["results"].as_array().unwrap();

        let passed = match case.class {
            QueryClass::Exact => results
                .first()
                .and_then(|result| result["source_id"].as_str())
                .is_some_and(|source_id| case.gold_source_ids.contains(&source_id)),
            QueryClass::Keyword | QueryClass::CjkMixed => results
                .iter()
                .take(5)
                .filter_map(|result| result["source_id"].as_str())
                .any(|source_id| case.gold_source_ids.contains(&source_id)),
            QueryClass::Negative => results.is_empty(),
        };
        report.record_case(case.class, passed);
        if !passed {
            report.top_failures.push(format!(
                "{} ({:?}) query `{}` returned {:?}, expected {:?}",
                case.name,
                case.class,
                case.query,
                results
                    .iter()
                    .take(5)
                    .filter_map(|result| result["source_id"].as_str())
                    .collect::<Vec<_>>(),
                case.gold_source_ids
            ));
        }

        for result in results.iter().take(5) {
            let source_id = result["source_id"].as_str().unwrap();
            assert_eq!(result["get_args"]["source_id"], source_id);
            let get = fixture.qgh(&["get", source_id, "--json"]);
            let get_ok = get.status.success()
                && stdout_json(&get)["data"]["source"]["source_id"] == source_id;
            report.record_round_trip(get_ok);
            if !get_ok {
                report
                    .top_failures
                    .push(format!("get round-trip failed for {source_id}"));
            }
        }
    }

    assert!(report.meets_targets(), "{}", report.summary());
    assert_eq!(
        WIKI_EXCLUDED_REASON,
        "Wiki is post-MVP and excluded from the MVP eval fixture."
    );

    let docs = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/search-quality-eval.md"),
    )
    .unwrap();
    for required in [
        "release/test harness",
        "not a user-facing CLI or MCP command",
        "Wiki is post-MVP",
        "Gold source_id",
        "ambiguous",
        "recalibration_requires_prd_adr_update",
    ] {
        assert!(docs.contains(required), "missing docs phrase: {required}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum QueryClass {
    Exact,
    Keyword,
    CjkMixed,
    Negative,
}

struct EvalCase {
    name: &'static str,
    class: QueryClass,
    query: &'static str,
    gold_source_ids: &'static [&'static str],
    labeler: &'static str,
    labeling_rule: &'static str,
    ambiguous_exclusion_rule: &'static str,
}

fn eval_cases() -> Vec<EvalCase> {
    vec![
        exact(
            "issue number 101",
            "101",
            &["qgh://github.com/issue/I_EVAL_101"],
        ),
        exact(
            "issue url 102",
            "https://github.com/owner/repo/issues/102",
            &["qgh://github.com/issue/I_EVAL_102"],
        ),
        exact(
            "title schema drift",
            "Release gate schema drift",
            &["qgh://github.com/issue/I_EVAL_103"],
        ),
        exact(
            "title token source",
            "Token source env fallback",
            &["qgh://github.com/issue/I_EVAL_104"],
        ),
        exact(
            "issue number 105",
            "#105",
            &["qgh://github.com/issue/I_EVAL_105"],
        ),
        exact(
            "issue url 106",
            "https://github.com/owner/repo/issues/106",
            &["qgh://github.com/issue/I_EVAL_106"],
        ),
        keyword(
            "pagination body",
            "pagination cursor duplicate etag",
            &["qgh://github.com/issue/I_EVAL_101"],
        ),
        keyword(
            "rate limit body",
            "retry-after secondary rate limit backoff",
            &["qgh://github.com/issue/I_EVAL_102"],
        ),
        keyword(
            "schema body",
            "schema envelope validation strict additionalProperties",
            &["qgh://github.com/issue/I_EVAL_103"],
        ),
        keyword(
            "token body",
            "credential store token source reference",
            &["qgh://github.com/issue/I_EVAL_104"],
        ),
        keyword(
            "comment rollback",
            "blue deploy rollback playbook",
            &["qgh://github.com/issue-comment/IC_EVAL_201"],
        ),
        keyword(
            "comment cache",
            "cache invalidation workaround shard map",
            &["qgh://github.com/issue-comment/IC_EVAL_202"],
        ),
        keyword(
            "comment race",
            "race condition reproduction clock skew",
            &["qgh://github.com/issue-comment/IC_EVAL_203"],
        ),
        keyword(
            "comment handoff",
            "operator handoff note stale generation",
            &["qgh://github.com/issue-comment/IC_EVAL_204"],
        ),
        cjk(
            "cjk auth token",
            "인증토큰",
            &["qgh://github.com/issue/I_EVAL_106"],
        ),
        cjk(
            "cjk login failure",
            "로그인실패",
            &["qgh://github.com/issue/I_EVAL_106"],
        ),
        cjk(
            "cjk index rebuild",
            "색인재빌드",
            &["qgh://github.com/issue/I_EVAL_107"],
        ),
        cjk(
            "cjk deploy error comment",
            "배포오류",
            &["qgh://github.com/issue-comment/IC_EVAL_205"],
        ),
        cjk(
            "mixed oauth callback",
            "OAuth콜백실패",
            &["qgh://github.com/issue/I_EVAL_108"],
        ),
        negative("negative billing", "invoice refund approval matrix"),
        negative("negative docker", "docker swarm overlay gossip"),
        negative("negative calendar", "calendar invite timezone lunch"),
        negative("negative frontend", "tailwind animation hero gradient"),
        negative("negative hardware", "gpu driver fan curve rpm"),
    ]
}

fn exact(
    name: &'static str,
    query: &'static str,
    gold_source_ids: &'static [&'static str],
) -> EvalCase {
    eval_case(name, QueryClass::Exact, query, gold_source_ids)
}

fn keyword(
    name: &'static str,
    query: &'static str,
    gold_source_ids: &'static [&'static str],
) -> EvalCase {
    eval_case(name, QueryClass::Keyword, query, gold_source_ids)
}

fn cjk(
    name: &'static str,
    query: &'static str,
    gold_source_ids: &'static [&'static str],
) -> EvalCase {
    eval_case(name, QueryClass::CjkMixed, query, gold_source_ids)
}

fn negative(name: &'static str, query: &'static str) -> EvalCase {
    eval_case(name, QueryClass::Negative, query, &[])
}

fn eval_case(
    name: &'static str,
    class: QueryClass,
    query: &'static str,
    gold_source_ids: &'static [&'static str],
) -> EvalCase {
    EvalCase {
        name,
        class,
        query,
        gold_source_ids,
        labeler: LABELER,
        labeling_rule: LABELING_RULE,
        ambiguous_exclusion_rule: AMBIGUOUS_EXCLUSION_RULE,
    }
}

#[derive(Default)]
struct EvalReport {
    class_totals: BTreeMap<QueryClass, usize>,
    class_hits: BTreeMap<QueryClass, usize>,
    round_trip_total: usize,
    round_trip_hits: usize,
    top_failures: Vec<String>,
}

impl EvalReport {
    fn record_case(&mut self, class: QueryClass, passed: bool) {
        *self.class_totals.entry(class).or_default() += 1;
        if passed {
            *self.class_hits.entry(class).or_default() += 1;
        }
    }

    fn record_round_trip(&mut self, passed: bool) {
        self.round_trip_total += 1;
        if passed {
            self.round_trip_hits += 1;
        }
    }

    fn meets_targets(&self) -> bool {
        self.class_rate(QueryClass::Exact) >= 0.95
            && self.class_rate(QueryClass::Keyword) >= 0.80
            && self.class_rate(QueryClass::CjkMixed) >= 0.70
            && self.class_rate(QueryClass::Negative) >= 0.80
            && self.round_trip_rate() >= 1.0
    }

    fn summary(&self) -> String {
        format!(
            "search quality eval failed\nexact_top1={:.2}\nkeyword_top5={:.2}\ncjk_top5={:.2}\nnegative_abstention={:.2}\nget_round_trip={:.2}\nrecalibration_requires_prd_adr_update={}\ntop_failures={:#?}",
            self.class_rate(QueryClass::Exact),
            self.class_rate(QueryClass::Keyword),
            self.class_rate(QueryClass::CjkMixed),
            self.class_rate(QueryClass::Negative),
            self.round_trip_rate(),
            !self.meets_targets(),
            self.top_failures
        )
    }

    fn class_rate(&self, class: QueryClass) -> f64 {
        let total = *self.class_totals.get(&class).unwrap_or(&0);
        if total == 0 {
            return 0.0;
        }
        *self.class_hits.get(&class).unwrap_or(&0) as f64 / total as f64
    }

    fn round_trip_rate(&self) -> f64 {
        if self.round_trip_total == 0 {
            return 1.0;
        }
        self.round_trip_hits as f64 / self.round_trip_total as f64
    }
}

struct EvalFixture {
    config_home: PathBuf,
    data_home: PathBuf,
    cache_home: PathBuf,
}

impl EvalFixture {
    fn new(name: &str) -> Self {
        let root = unique_temp_dir(name);
        let config_home = root.join("config");
        let data_home = root.join("data");
        let cache_home = root.join("cache");
        fs::create_dir_all(config_home.join("qgh")).unwrap();
        fs::create_dir_all(&data_home).unwrap();
        fs::create_dir_all(&cache_home).unwrap();
        Self {
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

    fn qgh(&self, args: &[&str]) -> Output {
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
            .env_remove("RUST_LOG")
            .args(["--profile", "work"])
            .args(args);
        cmd.output().unwrap()
    }
}

struct EvalFakeGitHub {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EvalFakeGitHub {
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
                    Ok(stream) => handle_eval_connection(stream),
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

impl Drop for EvalFakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_eval_connection(mut stream: TcpStream) {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().unwrap_or("");
    let path = request_line.split_whitespace().nth(1).unwrap_or("");

    let (status, body) = if path.starts_with("/repos/owner/repo/issues?") {
        ("200 OK", issue_payload())
    } else if path.starts_with("/repos/owner/repo/issues/comments/") {
        ("200 OK", "{}".to_string())
    } else if path.starts_with("/repos/owner/repo/issues/") && path.contains("/comments?") {
        ("200 OK", comments_payload(path))
    } else if path.starts_with("/repos/owner/repo/issues/") {
        ("200 OK", "{}".to_string())
    } else {
        ("404 Not Found", r#"{"message":"not found"}"#.to_string())
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\nx-ratelimit-remaining: 4999\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

fn issue_payload() -> String {
    let issues = eval_issues()
        .into_iter()
        .map(|issue| {
            json!({
                "id": issue.github_id,
                "node_id": issue.node_id,
                "number": issue.number,
                "title": issue.title,
                "body": issue.body,
                "state": "open",
                "locked": false,
                "comments": issue.comments.len(),
                "html_url": format!("https://github.com/owner/repo/issues/{}", issue.number),
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-02T00:00:00Z",
                "closed_at": null,
                "user": {"login": issue.author},
                "labels": issue.labels.into_iter().map(|label| json!({"name": label})).collect::<Vec<_>>(),
                "milestone": null,
                "assignees": []
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&issues).unwrap()
}

fn comments_payload(path: &str) -> String {
    let Some(number) = path
        .strip_prefix("/repos/owner/repo/issues/")
        .and_then(|rest| rest.split('/').next())
        .and_then(|number| number.parse::<i64>().ok())
    else {
        return "[]".to_string();
    };
    let comments = eval_issues()
        .into_iter()
        .find(|issue| issue.number == number)
        .map(|issue| {
            issue
                .comments
                .into_iter()
                .map(|comment| {
                    json!({
                        "id": comment.github_id,
                        "node_id": comment.node_id,
                        "body": comment.body,
                        "html_url": format!(
                            "https://github.com/owner/repo/issues/{}#issuecomment-{}",
                            number, comment.github_id
                        ),
                        "created_at": "2026-01-03T00:00:00Z",
                        "updated_at": "2026-01-03T00:00:00Z",
                        "user": {"login": comment.author}
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    serde_json::to_string(&comments).unwrap()
}

struct EvalIssue {
    number: i64,
    github_id: i64,
    node_id: &'static str,
    title: &'static str,
    body: &'static str,
    labels: Vec<&'static str>,
    author: &'static str,
    comments: Vec<EvalComment>,
}

struct EvalComment {
    github_id: i64,
    node_id: &'static str,
    body: &'static str,
    author: &'static str,
}

fn eval_issues() -> Vec<EvalIssue> {
    vec![
        issue(
            101,
            "I_EVAL_101",
            "Pagination cursor duplicate ETag",
            "Pagination cursor duplicate etag handling must avoid missing updated issues during incremental sync.",
            vec!["sync"],
            vec![comment(
                201,
                "IC_EVAL_201",
                "Blue deploy rollback playbook: pause workers, restore generation pointer, then rerun sync.",
            )],
        ),
        issue(
            102,
            "I_EVAL_102",
            "Secondary rate limit backoff",
            "Retry-after secondary rate limit backoff should be visible in status while local query remains available.",
            vec!["sync", "rate-limit"],
            vec![comment(
                202,
                "IC_EVAL_202",
                "Cache invalidation workaround updates the shard map before replaying dirty index tasks.",
            )],
        ),
        issue(
            103,
            "I_EVAL_103",
            "Release gate schema drift",
            "Schema envelope validation must keep strict additionalProperties false for query and get output.",
            vec!["schema"],
            vec![comment(
                203,
                "IC_EVAL_203",
                "Race condition reproduction uses clock skew between sync completion and Tantivy publish.",
            )],
        ),
        issue(
            104,
            "I_EVAL_104",
            "Token source env fallback",
            "Credential store token source reference must not persist literal tokens in config or logs.",
            vec!["privacy"],
            vec![comment(
                204,
                "IC_EVAL_204",
                "Operator handoff note: stale generation means query should keep using the previous active index.",
            )],
        ),
        issue(
            105,
            "I_EVAL_105",
            "Exact issue number locator",
            "Issue number locator lookup should bypass BM25 ambiguity when the allowlist has one repo.",
            vec!["lookup"],
            vec![],
        ),
        issue(
            106,
            "I_EVAL_106",
            "OAuth 인증 토큰 만료",
            "로그인 실패는 인증 토큰 갱신 누락 때문에 발생합니다. OAuth callback also reports a mixed Korean failure.",
            vec!["i18n"],
            vec![],
        ),
        issue(
            107,
            "I_EVAL_107",
            "검색 색인 재빌드 중 한글 댓글 누락",
            "색인 재빌드 동안 한글 검색 결과는 이전 active generation 또는 새 generation 중 하나로 유지되어야 합니다.",
            vec!["i18n", "index"],
            vec![comment(
                205,
                "IC_EVAL_205",
                "CJK mixed fallback field handles 배포 오류 without hosted model or vector provider.",
            )],
        ),
        issue(
            108,
            "I_EVAL_108",
            "Mixed OAuth callback 실패",
            "OAuth 콜백 실패 분석은 Korean mixed query에서 BM25 fallback field로 검색되어야 합니다.",
            vec!["i18n", "auth"],
            vec![],
        ),
    ]
}

fn issue(
    number: i64,
    node_id: &'static str,
    title: &'static str,
    body: &'static str,
    labels: Vec<&'static str>,
    comments: Vec<EvalComment>,
) -> EvalIssue {
    EvalIssue {
        number,
        github_id: 1000 + number,
        node_id,
        title,
        body,
        labels,
        author: "fixture-author",
        comments,
    }
}

fn comment(github_id: i64, node_id: &'static str, body: &'static str) -> EvalComment {
    EvalComment {
        github_id,
        node_id,
        body,
        author: "fixture-commenter",
    }
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
