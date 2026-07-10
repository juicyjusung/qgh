#![cfg(feature = "vector-search")]

use qgh::embedding::{
    EmbeddingFingerprintSeed, PoolingKind, DEFAULT_HF_MODEL_ID, DEFAULT_HF_MODEL_REVISION,
    DEFAULT_QUERY_PREFIX,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::raw::{c_char, c_int};
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
const TEST_EMBEDDING_QUERY_VECTORS_ENV: &str = "QGH_TEST_EMBEDDING_QUERY_VECTORS";
const TEST_EMBEDDING_DOCUMENT_VECTORS_ENV: &str = "QGH_TEST_EMBEDDING_DOCUMENT_VECTORS";
const CHUNK_EMBEDDING_VECTORS_TABLE: &str = "chunk_embedding_vectors";
const EVAL_VECTOR_DIMENSION: usize = 32;
const SEMANTIC_TOP5_TARGET: f64 = 0.70;
const CROSS_LANGUAGE_TOP5_TARGET: f64 = 0.60;
const DRAGONKUE_KO_MODEL_ID: &str = "dragonkue/snowflake-arctic-embed-l-v2.0-ko";
const GTE_MODERNBERT_BASE_MODEL_ID: &str = "Alibaba-NLP/gte-modernbert-base";
const AXIS_PAGINATION: usize = 0;
const AXIS_RATE_LIMIT: usize = 1;
const AXIS_SCHEMA: usize = 2;
const AXIS_TOKEN_PRIVACY: usize = 3;
const AXIS_DIRECT_LOCATOR: usize = 4;
const AXIS_OAUTH_LOGIN: usize = 5;
const AXIS_INDEX_REBUILD: usize = 6;
const AXIS_CALLBACK: usize = 7;
const AXIS_DEPLOY_ROLLBACK: usize = 8;
const AXIS_CACHE_REPLAY: usize = 9;
const AXIS_PUBLISH_RACE: usize = 10;
const AXIS_PREVIOUS_INDEX: usize = 11;
const AXIS_NO_HOSTED_VECTOR: usize = 12;
const AXIS_KOREAN: usize = 13;
const AXIS_AUTH: usize = 14;
const AXIS_SYNC: usize = 15;
const AXIS_STATUS: usize = 16;
const AXIS_OUTPUT_SCHEMA: usize = 17;
const AXIS_MODEL_ARCTIC: usize = 18;
const AXIS_MODEL_DRAGONKUE: usize = 19;
const AXIS_MODEL_GTE: usize = 20;

const MODEL_AB_CANDIDATES: [EvalModelCandidate; 3] = [
    EvalModelCandidate {
        name: "arctic-embed-l-v2.0",
        model_id: DEFAULT_HF_MODEL_ID,
    },
    EvalModelCandidate {
        name: "dragonkue-ko",
        model_id: DRAGONKUE_KO_MODEL_ID,
    },
    EvalModelCandidate {
        name: "gte-modernbert-base",
        model_id: GTE_MODERNBERT_BASE_MODEL_ID,
    },
];

#[test]
fn curated_search_quality_eval_gate_passes() {
    let fixture = EvalFixture::new("search-quality-eval");
    let server = EvalFakeGitHub::start();
    fixture.write_config(&server.base_url);
    assert_success(&fixture.qgh(&["sync", "--json"]));

    let regression_cases = bm25_regression_cases();
    let semantic_cases = semantic_eval_cases();
    let bm25_regression = run_eval_cases(&fixture, EvalMode::Bm25Only, &regression_cases, None);
    assert!(
        bm25_regression.meets_bm25_regression_targets() && bm25_regression.meets_hard_gates(),
        "{}",
        bm25_regression.summary("bm25 regression")
    );

    fixture.write_config(&server.base_url);
    let semantic_bm25 = run_eval_cases(&fixture, EvalMode::Bm25Only, &semantic_cases, None);
    assert!(
        semantic_bm25.meets_hard_gates(),
        "{}",
        semantic_bm25.summary("semantic bm25-only")
    );

    let default_model_vectors = eval_model_vectors(MODEL_AB_CANDIDATES[0]);
    fixture.write_config_with_embedding_model(&server.base_url, MODEL_AB_CANDIDATES[0].model_id);
    assert_success(&fixture.qgh(&["query", "prepare vector schema", "--json"]));
    fixture.seed_eval_chunks(&default_model_vectors.source_vectors);

    let mut model_reports = Vec::new();
    let mut previous_fingerprint_hash = None;
    let mut fingerprint_reembedding_checks = 0;
    for candidate in MODEL_AB_CANDIDATES {
        fixture.write_config_with_embedding_model(&server.base_url, candidate.model_id);
        if previous_fingerprint_hash.is_some() {
            assert_fingerprint_mismatch_falls_back_to_bm25(&fixture);
        }
        let model_vectors = eval_model_vectors(candidate);
        let query_vectors_json = eval_query_vectors_json(
            regression_cases.iter().chain(semantic_cases.iter()),
            &model_vectors.query_vectors,
        );
        let fingerprint_hash = fixture.embed_eval_vectors(candidate, &model_vectors.source_vectors);
        if let Some(previous) = previous_fingerprint_hash.as_ref() {
            assert_ne!(
                previous, &fingerprint_hash,
                "model A/B must replace the active fingerprint through qgh embed --force when model id changes"
            );
            fingerprint_reembedding_checks += 1;
        }
        previous_fingerprint_hash = Some(fingerprint_hash.clone());
        fixture.assert_active_eval_fingerprint(
            candidate,
            &fingerprint_hash,
            model_vectors.source_vectors.len(),
        );
        assert_eq!(
            fixture.active_eval_vector_table_count(),
            model_vectors.source_vectors.len(),
            "qgh embed --force must materialize every eval vector for the active fingerprint"
        );

        let hybrid_regression = run_eval_cases(
            &fixture,
            EvalMode::Hybrid,
            &regression_cases,
            Some(&query_vectors_json),
        );
        assert!(
            hybrid_regression.meets_bm25_regression_targets()
                && hybrid_regression.meets_hard_gates()
                && hybrid_regression.meets_hybrid_path_gate(),
            "{}",
            hybrid_regression.summary("hybrid regression")
        );

        let semantic_hybrid = run_eval_cases(
            &fixture,
            EvalMode::Hybrid,
            &semantic_cases,
            Some(&query_vectors_json),
        );
        assert!(
            semantic_hybrid.meets_hard_gates() && semantic_hybrid.meets_hybrid_path_gate(),
            "{}",
            semantic_hybrid.summary("semantic hybrid")
        );
        model_reports.push(ModelEvalReport {
            candidate,
            fingerprint_hash,
            hybrid_regression,
            semantic_hybrid,
        });
    }
    assert_candidate_vectors_are_distinct();
    assert_candidate_metrics_are_distinct(&model_reports);
    assert_eq!(MODEL_AB_CANDIDATES[0].model_id, DEFAULT_HF_MODEL_ID);
    assert_eq!(
        fingerprint_reembedding_checks,
        MODEL_AB_CANDIDATES.len() - 1,
        "A/B must verify fingerprint replacement between model candidates"
    );
    let default_model_report = model_reports
        .iter()
        .find(|report| report.candidate.model_id == DEFAULT_HF_MODEL_ID)
        .expect("default model report");
    eprintln!(
        "{}",
        ab_summary(
            &bm25_regression,
            &default_model_report.hybrid_regression,
            &semantic_bm25,
            &default_model_report.semantic_hybrid,
        )
    );
    eprintln!(
        "{}",
        model_ab_summary(
            &bm25_regression,
            &semantic_bm25,
            &model_reports,
            fingerprint_reembedding_checks,
        )
    );
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
        "BM25-only vs hybrid",
        "semantic/paraphrase",
        "cross-language",
        "section_8_3_triggers",
        "model_ab_report",
        "dragonkue/snowflake-arctic-embed-l-v2.0-ko",
        "Alibaba-NLP/gte-modernbert-base",
        "default model remains",
        "candidate-specific deterministic source and query vectors",
        "qgh embed --force --json",
        "fingerprint_reembedding_checks",
    ] {
        assert!(docs.contains(required), "missing docs phrase: {required}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EvalModelCandidate {
    name: &'static str,
    model_id: &'static str,
}

struct ModelEvalReport {
    candidate: EvalModelCandidate,
    fingerprint_hash: String,
    hybrid_regression: EvalReport,
    semantic_hybrid: EvalReport,
}

struct EvalModelVectors {
    source_vectors: BTreeMap<&'static str, Vec<f32>>,
    query_vectors: BTreeMap<&'static str, Vec<f32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum QueryClass {
    Exact,
    Keyword,
    CjkMixed,
    Semantic,
    CrossLanguage,
    Negative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvalMode {
    Bm25Only,
    Hybrid,
}

impl EvalMode {
    fn as_str(self) -> &'static str {
        match self {
            EvalMode::Bm25Only => "bm25-only",
            EvalMode::Hybrid => "hybrid",
        }
    }
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

fn bm25_regression_cases() -> Vec<EvalCase> {
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
            "env token source reference",
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

fn semantic_eval_cases() -> Vec<EvalCase> {
    vec![
        semantic(
            "paraphrase pagination duplicates",
            "sync misses changed issues when pages repeat",
            &["qgh://github.com/issue/I_EVAL_101"],
        ),
        semantic(
            "natural rollback recovery",
            "how should workers recover after a bad blue deployment",
            &["qgh://github.com/issue-comment/IC_EVAL_201"],
        ),
        semantic(
            "natural throttling status",
            "where is secondary API throttling surfaced during local search",
            &["qgh://github.com/issue/I_EVAL_102"],
        ),
        semantic(
            "symptom schema extra fields",
            "why did JSON output reject an extra envelope field",
            &["qgh://github.com/issue/I_EVAL_103"],
        ),
        semantic(
            "privacy token persistence",
            "what prevents saved secrets from leaking in logs",
            &["qgh://github.com/issue/I_EVAL_104"],
        ),
        semantic(
            "cause direct locator",
            "why should issue number lookup avoid ambiguous text ranking",
            &["qgh://github.com/issue/I_EVAL_105"],
        ),
        semantic(
            "symptom login failure",
            "why did login fail after the OAuth flow",
            &["qgh://github.com/issue/I_EVAL_106"],
        ),
        semantic(
            "index rebuild continuity",
            "how are Korean search results preserved during an index rebuild",
            &["qgh://github.com/issue/I_EVAL_107"],
        ),
        semantic(
            "mixed callback analysis",
            "which callback failure mixes Korean and English auth text",
            &["qgh://github.com/issue/I_EVAL_108"],
        ),
        semantic(
            "cache replay workaround",
            "what fixes dirty index task replay after shard mapping changes",
            &["qgh://github.com/issue-comment/IC_EVAL_202"],
        ),
        semantic(
            "publish race reproduction",
            "which note explains stale generation after a publish race",
            &["qgh://github.com/issue-comment/IC_EVAL_203"],
        ),
        semantic(
            "operator previous index",
            "which handoff says to keep using the previous index generation",
            &["qgh://github.com/issue-comment/IC_EVAL_204"],
        ),
        cross_language(
            "ko to en pagination",
            "페이지 반복 때 변경된 이슈 누락 방지",
            &["qgh://github.com/issue/I_EVAL_101"],
        ),
        cross_language(
            "ko to en rate limit",
            "보조 rate limit 대기 상태는 어디에 보이나",
            &["qgh://github.com/issue/I_EVAL_102"],
        ),
        cross_language(
            "ko to en schema",
            "스키마 출력에 추가 필드를 금지해야 하나",
            &["qgh://github.com/issue/I_EVAL_103"],
        ),
        cross_language(
            "ko to en token",
            "토큰을 설정 파일이나 로그에 저장하지 않는 규칙",
            &["qgh://github.com/issue/I_EVAL_104"],
        ),
        cross_language(
            "en to ko rebuild",
            "Korean comments disappear during index rebuild",
            &["qgh://github.com/issue/I_EVAL_107"],
        ),
        cross_language(
            "en to ko login cause",
            "OAuth token refresh missing causes login failure",
            &["qgh://github.com/issue/I_EVAL_106"],
        ),
        cross_language(
            "en to ko deploy error",
            "deployment error without hosted vector provider",
            &["qgh://github.com/issue-comment/IC_EVAL_205"],
        ),
        cross_language(
            "en to ko callback",
            "Korean callback failure analysis",
            &["qgh://github.com/issue/I_EVAL_108"],
        ),
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

fn semantic(
    name: &'static str,
    query: &'static str,
    gold_source_ids: &'static [&'static str],
) -> EvalCase {
    eval_case(name, QueryClass::Semantic, query, gold_source_ids)
}

fn cross_language(
    name: &'static str,
    query: &'static str,
    gold_source_ids: &'static [&'static str],
) -> EvalCase {
    eval_case(name, QueryClass::CrossLanguage, query, gold_source_ids)
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
    hard_filter_violations: usize,
    hybrid_ranked_results: usize,
    hybrid_path_query_total: usize,
    hybrid_path_query_hits: usize,
    top_failures: Vec<String>,
}

impl EvalReport {
    fn record_case(&mut self, class: QueryClass, passed: bool) {
        *self.class_totals.entry(class).or_default() += 1;
        if passed {
            *self.class_hits.entry(class).or_default() += 1;
        }
    }

    fn record_hard_filter_violation(&mut self, message: String) {
        self.hard_filter_violations += 1;
        self.top_failures.push(message);
    }

    fn record_hybrid_result(&mut self) {
        self.hybrid_ranked_results += 1;
    }

    fn record_hybrid_path_query(&mut self, passed: bool) {
        self.hybrid_path_query_total += 1;
        if passed {
            self.hybrid_path_query_hits += 1;
        }
    }

    fn record_round_trip(&mut self, passed: bool) {
        self.round_trip_total += 1;
        if passed {
            self.round_trip_hits += 1;
        }
    }

    fn meets_bm25_regression_targets(&self) -> bool {
        self.class_rate(QueryClass::Exact) >= 0.95
            && self.class_rate(QueryClass::Keyword) >= 0.80
            && self.class_rate(QueryClass::CjkMixed) >= 0.70
            && self.class_rate(QueryClass::Negative) >= 0.80
    }

    fn meets_hard_gates(&self) -> bool {
        self.hard_filter_violations == 0 && self.round_trip_rate() >= 1.0
    }

    fn meets_hybrid_path_gate(&self) -> bool {
        self.hybrid_path_query_total > 0
            && self.hybrid_path_query_hits == self.hybrid_path_query_total
    }

    fn summary(&self, name: &str) -> String {
        format!(
            "{name} search quality eval failed\nexact_top1={:.2}\nkeyword_top5={:.2}\ncjk_top5={:.2}\nsemantic_top5={:.2}\ncross_language_top5={:.2}\nnegative_abstention={:.2}\nhard_filter_violations={}\nget_round_trip={:.2}\nhybrid_ranked_results={}\nhybrid_path_queries={}/{}\nrecalibration_requires_prd_adr_update={}\ntop_failures={:#?}",
            self.class_rate(QueryClass::Exact),
            self.class_rate(QueryClass::Keyword),
            self.class_rate(QueryClass::CjkMixed),
            self.class_rate(QueryClass::Semantic),
            self.class_rate(QueryClass::CrossLanguage),
            self.class_rate(QueryClass::Negative),
            self.hard_filter_violations,
            self.round_trip_rate(),
            self.hybrid_ranked_results,
            self.hybrid_path_query_hits,
            self.hybrid_path_query_total,
            !self.meets_bm25_regression_targets()
                || !self.meets_hard_gates()
                || (self.hybrid_path_query_total > 0 && !self.meets_hybrid_path_gate()),
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

fn run_eval_cases(
    fixture: &EvalFixture,
    mode: EvalMode,
    cases: &[EvalCase],
    query_vectors_json: Option<&str>,
) -> EvalReport {
    let mut report = EvalReport::default();
    for case in cases {
        assert!(!case.labeler.is_empty());
        assert!(!case.labeling_rule.is_empty());
        assert!(!case.ambiguous_exclusion_rule.is_empty());

        let query = fixture.qgh_with_query_vectors(
            &["query", case.query, "--limit", "5", "--json"],
            query_vectors_json,
        );
        assert_success(&query);
        let query_json = stdout_json(&query);
        let results = query_json["data"]["results"].as_array().unwrap();

        let passed = case_passed(case, results);
        report.record_case(case.class, passed);
        if !passed {
            report.top_failures.push(format!(
                "{} {} ({:?}) query `{}` returned {:?}, expected {:?}",
                mode.as_str(),
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

        let hybrid_ranked_results_before = report.hybrid_ranked_results;
        for result in results.iter().take(5) {
            if result["ranking"]["kind"] == "hybrid" {
                report.record_hybrid_result();
            }
            if let Some(message) = hard_filter_violation(result) {
                report.record_hard_filter_violation(format!(
                    "{} query `{}` hard filter violation: {message}",
                    mode.as_str(),
                    case.query
                ));
            }
            let source_id = result["source_id"].as_str().unwrap();
            assert_eq!(result["get_args"]["source_id"], source_id);
            let get = fixture.qgh(&["get", source_id, "--json"]);
            let get_ok = get.status.success()
                && stdout_json(&get)["data"]["source"]["source_id"] == source_id;
            report.record_round_trip(get_ok);
            if !get_ok {
                report.top_failures.push(format!(
                    "{} get round-trip failed for {source_id}",
                    mode.as_str()
                ));
            }
        }
        if mode == EvalMode::Hybrid && case.requires_hybrid_path() {
            let passed = report.hybrid_ranked_results > hybrid_ranked_results_before;
            report.record_hybrid_path_query(passed);
            if !passed {
                report.top_failures.push(format!(
                    "{} {} ({:?}) query `{}` did not return a hybrid-ranked result",
                    mode.as_str(),
                    case.name,
                    case.class,
                    case.query
                ));
            }
        }
    }
    report
}

impl EvalCase {
    fn requires_hybrid_path(&self) -> bool {
        match self.class {
            QueryClass::Negative => false,
            QueryClass::Exact => !is_exact_locator_query(self.query),
            QueryClass::Keyword
            | QueryClass::CjkMixed
            | QueryClass::Semantic
            | QueryClass::CrossLanguage => true,
        }
    }
}

fn is_exact_locator_query(query: &str) -> bool {
    query.starts_with("https://github.com/")
        || query
            .strip_prefix('#')
            .unwrap_or(query)
            .parse::<i64>()
            .is_ok()
}

fn case_passed(case: &EvalCase, results: &[Value]) -> bool {
    match case.class {
        QueryClass::Exact => results
            .first()
            .and_then(|result| result["source_id"].as_str())
            .is_some_and(|source_id| case.gold_source_ids.contains(&source_id)),
        QueryClass::Keyword
        | QueryClass::CjkMixed
        | QueryClass::Semantic
        | QueryClass::CrossLanguage => results
            .iter()
            .take(5)
            .filter_map(|result| result["source_id"].as_str())
            .any(|source_id| case.gold_source_ids.contains(&source_id)),
        QueryClass::Negative => results.is_empty(),
    }
}

fn hard_filter_violation(result: &Value) -> Option<String> {
    let Some(repo) = result["repo"].as_str() else {
        return Some("repo=<missing>".to_string());
    };
    if repo != "owner/repo" {
        return Some(format!("repo={repo}"));
    }
    let Some(entity_type) = result["entity_type"].as_str() else {
        return Some("entity_type=<missing>".to_string());
    };
    if !matches!(entity_type, "issue" | "issue_comment") {
        return Some(format!("entity_type={entity_type}"));
    }
    let Some(source_id) = result["source_id"].as_str() else {
        return Some("source_id=<missing>".to_string());
    };
    if !(source_id.starts_with("qgh://github.com/issue/I_EVAL_")
        || source_id.starts_with("qgh://github.com/issue-comment/IC_EVAL_"))
    {
        return Some(format!("source_id={source_id}"));
    }
    None
}

fn assert_fingerprint_mismatch_falls_back_to_bm25(fixture: &EvalFixture) {
    let query = fixture.qgh(&[
        "query",
        "pagination cursor duplicate etag",
        "--limit",
        "5",
        "--json",
    ]);
    assert_success(&query);
    let query_json = stdout_json(&query);
    let warnings = query_json["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|warning| warning["code"] == "embedding.fingerprint_mismatch"),
        "model switch must warn before reembedding: {query_json}"
    );
    let results = query_json["data"]["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "BM25 fallback should still return fixture results on mismatch"
    );
    assert!(
        results
            .iter()
            .all(|result| result["ranking"]["kind"] == "bm25"),
        "fingerprint mismatch must disable hybrid until reembedding: {query_json}"
    );
}

fn ab_summary(
    bm25_regression: &EvalReport,
    hybrid_regression: &EvalReport,
    semantic_bm25: &EvalReport,
    semantic_hybrid: &EvalReport,
) -> String {
    let semantic_hybrid_rate = semantic_hybrid.class_rate(QueryClass::Semantic);
    let cross_language_hybrid_rate = semantic_hybrid.class_rate(QueryClass::CrossLanguage);
    let section_8_3_triggers =
        section_8_3_triggers(semantic_hybrid_rate, cross_language_hybrid_rate);
    format!(
        "search_quality_eval_ab_report\nbm25_regression_exact_top1={:.2}\nbm25_regression_keyword_top5={:.2}\nbm25_regression_cjk_top5={:.2}\nbm25_regression_negative_abstention={:.2}\nhybrid_regression_exact_top1={:.2}\nhybrid_regression_keyword_top5={:.2}\nhybrid_regression_cjk_top5={:.2}\nhybrid_regression_negative_abstention={:.2}\nhybrid_regression_path_queries={}/{}\nsemantic_bm25_top5={:.2}\nsemantic_hybrid_top5={:.2}\nsemantic_hybrid_delta={:.2}\nsemantic_hybrid_target={:.2}\nsemantic_hybrid_path_queries={}/{}\ncross_language_bm25_top5={:.2}\ncross_language_hybrid_top5={:.2}\ncross_language_hybrid_delta={:.2}\ncross_language_hybrid_target={:.2}\nhard_filter_violations={}\nget_round_trip={:.2}\nsection_8_3_triggers={:?}",
        bm25_regression.class_rate(QueryClass::Exact),
        bm25_regression.class_rate(QueryClass::Keyword),
        bm25_regression.class_rate(QueryClass::CjkMixed),
        bm25_regression.class_rate(QueryClass::Negative),
        hybrid_regression.class_rate(QueryClass::Exact),
        hybrid_regression.class_rate(QueryClass::Keyword),
        hybrid_regression.class_rate(QueryClass::CjkMixed),
        hybrid_regression.class_rate(QueryClass::Negative),
        hybrid_regression.hybrid_path_query_hits,
        hybrid_regression.hybrid_path_query_total,
        semantic_bm25.class_rate(QueryClass::Semantic),
        semantic_hybrid_rate,
        semantic_hybrid_rate - semantic_bm25.class_rate(QueryClass::Semantic),
        SEMANTIC_TOP5_TARGET,
        semantic_hybrid.hybrid_path_query_hits,
        semantic_hybrid.hybrid_path_query_total,
        semantic_bm25.class_rate(QueryClass::CrossLanguage),
        cross_language_hybrid_rate,
        cross_language_hybrid_rate - semantic_bm25.class_rate(QueryClass::CrossLanguage),
        CROSS_LANGUAGE_TOP5_TARGET,
        bm25_regression.hard_filter_violations
            + hybrid_regression.hard_filter_violations
            + semantic_bm25.hard_filter_violations
            + semantic_hybrid.hard_filter_violations,
        combined_round_trip_rate(&[
            bm25_regression,
            hybrid_regression,
            semantic_bm25,
            semantic_hybrid,
        ]),
        section_8_3_triggers
    )
}

fn model_ab_summary(
    bm25_regression: &EvalReport,
    semantic_bm25: &EvalReport,
    model_reports: &[ModelEvalReport],
    fingerprint_reembedding_checks: usize,
) -> String {
    let mut reports = vec![bm25_regression, semantic_bm25];
    for report in model_reports {
        reports.push(&report.hybrid_regression);
        reports.push(&report.semantic_hybrid);
    }
    let hard_filter_violations = reports
        .iter()
        .map(|report| report.hard_filter_violations)
        .sum::<usize>();
    let recalibration_requires_prd_adr_update = !bm25_regression.meets_bm25_regression_targets()
        || !bm25_regression.meets_hard_gates()
        || !semantic_bm25.meets_hard_gates()
        || model_reports.iter().any(|report| {
            let semantic_rate = report.semantic_hybrid.class_rate(QueryClass::Semantic);
            let cross_language_rate = report.semantic_hybrid.class_rate(QueryClass::CrossLanguage);
            !report.hybrid_regression.meets_bm25_regression_targets()
                || !report.hybrid_regression.meets_hard_gates()
                || !report.hybrid_regression.meets_hybrid_path_gate()
                || !report.semantic_hybrid.meets_hard_gates()
                || !report.semantic_hybrid.meets_hybrid_path_gate()
                || !section_8_3_triggers(semantic_rate, cross_language_rate).is_empty()
        });
    let mut lines = vec![
        "model_ab_report".to_string(),
        format!(
            "fixture=search-quality-eval protocol=H4a candidates={}",
            model_reports.len()
        ),
        format!(
            "fingerprint_reembedding_checks={}/{}",
            fingerprint_reembedding_checks,
            model_reports.len().saturating_sub(1)
        ),
        format!(
            "bm25_semantic_top5={:.2}",
            semantic_bm25.class_rate(QueryClass::Semantic)
        ),
        format!(
            "bm25_cross_language_top5={:.2}",
            semantic_bm25.class_rate(QueryClass::CrossLanguage)
        ),
        format!(
            "combined_get_round_trip={:.2}",
            combined_round_trip_rate(&reports)
        ),
        format!("hard_filter_violations={hard_filter_violations}"),
        format!("recalibration_requires_prd_adr_update={recalibration_requires_prd_adr_update}"),
    ];
    for report in model_reports {
        let semantic_rate = report.semantic_hybrid.class_rate(QueryClass::Semantic);
        let cross_language_rate = report.semantic_hybrid.class_rate(QueryClass::CrossLanguage);
        lines.push(format!(
            "model={} model_id={} fingerprint={} regression_path_queries={}/{} semantic_hybrid_top5={:.2} semantic_delta={:.2} cross_language_hybrid_top5={:.2} cross_language_delta={:.2} section_8_3_triggers={:?}",
            report.candidate.name,
            report.candidate.model_id,
            fingerprint_hash_prefix(&report.fingerprint_hash),
            report.hybrid_regression.hybrid_path_query_hits,
            report.hybrid_regression.hybrid_path_query_total,
            semantic_rate,
            semantic_rate - semantic_bm25.class_rate(QueryClass::Semantic),
            cross_language_rate,
            cross_language_rate - semantic_bm25.class_rate(QueryClass::CrossLanguage),
            section_8_3_triggers(semantic_rate, cross_language_rate)
        ));
    }
    lines.join("\n")
}

fn assert_candidate_vectors_are_distinct() {
    let arctic = eval_model_vectors(MODEL_AB_CANDIDATES[0]);
    let dragonkue = eval_model_vectors(MODEL_AB_CANDIDATES[1]);
    let gte = eval_model_vectors(MODEL_AB_CANDIDATES[2]);
    assert_ne!(
        arctic.source_vectors, dragonkue.source_vectors,
        "model A/B source vectors must be candidate-specific"
    );
    assert_ne!(
        arctic.source_vectors, gte.source_vectors,
        "model A/B source vectors must be candidate-specific"
    );
    assert_ne!(
        arctic.query_vectors, gte.query_vectors,
        "model A/B query vectors must be candidate-specific"
    );
}

fn assert_candidate_metrics_are_distinct(model_reports: &[ModelEvalReport]) {
    let metric_signatures = model_reports
        .iter()
        .map(|report| {
            (
                report.semantic_hybrid.class_rate(QueryClass::Semantic),
                report.semantic_hybrid.class_rate(QueryClass::CrossLanguage),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        metric_signatures
            .windows(2)
            .any(|window| window[0] != window[1]),
        "model A/B report must not collapse to structurally identical semantic/cross-language metrics: {metric_signatures:?}"
    );
}

fn fingerprint_hash_prefix(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

fn section_8_3_triggers(
    semantic_hybrid_rate: f64,
    cross_language_hybrid_rate: f64,
) -> Vec<&'static str> {
    let mut triggers = Vec::new();
    if semantic_hybrid_rate < SEMANTIC_TOP5_TARGET {
        triggers.push("semantic_rerank_review");
    }
    if cross_language_hybrid_rate < CROSS_LANGUAGE_TOP5_TARGET {
        triggers.push("cross_language_rerank_review");
    }
    triggers
}

fn combined_round_trip_rate(reports: &[&EvalReport]) -> f64 {
    let total = reports
        .iter()
        .map(|report| report.round_trip_total)
        .sum::<usize>();
    if total == 0 {
        return 1.0;
    }
    let hits = reports
        .iter()
        .map(|report| report.round_trip_hits)
        .sum::<usize>();
    hits as f64 / total as f64
}

fn eval_model_vectors(candidate: EvalModelCandidate) -> EvalModelVectors {
    let mut source_vectors = eval_base_source_vectors();
    let mut query_vectors = eval_base_query_vectors();
    let model_axis = match candidate.name {
        "arctic-embed-l-v2.0" => AXIS_MODEL_ARCTIC,
        "dragonkue-ko" => AXIS_MODEL_DRAGONKUE,
        "gte-modernbert-base" => {
            query_vectors.insert(
                "페이지 반복 때 변경된 이슈 누락 방지",
                topic_vector(&[(AXIS_SCHEMA, 10.0)]),
            );
            query_vectors.insert(
                "스키마 출력에 추가 필드를 금지해야 하나",
                topic_vector(&[(AXIS_DIRECT_LOCATOR, 10.0)]),
            );
            query_vectors.insert(
                "토큰을 설정 파일이나 로그에 저장하지 않는 규칙",
                topic_vector(&[(AXIS_PAGINATION, 10.0)]),
            );
            AXIS_MODEL_GTE
        }
        other => panic!("missing eval vector fixture for model candidate {other}"),
    };
    add_model_axis(&mut source_vectors, model_axis);
    add_model_axis(&mut query_vectors, model_axis);
    EvalModelVectors {
        source_vectors,
        query_vectors,
    }
}

fn eval_base_source_vectors() -> BTreeMap<&'static str, Vec<f32>> {
    [
        (
            "qgh://github.com/issue/I_EVAL_101",
            topic_vector(&[(AXIS_PAGINATION, 1.0), (AXIS_SYNC, 0.7)]),
        ),
        (
            "qgh://github.com/issue/I_EVAL_102",
            topic_vector(&[(AXIS_RATE_LIMIT, 1.0), (AXIS_SYNC, 0.5), (AXIS_STATUS, 0.8)]),
        ),
        (
            "qgh://github.com/issue/I_EVAL_103",
            topic_vector(&[(AXIS_SCHEMA, 1.0), (AXIS_OUTPUT_SCHEMA, 0.9)]),
        ),
        (
            "qgh://github.com/issue/I_EVAL_104",
            topic_vector(&[(AXIS_TOKEN_PRIVACY, 1.0)]),
        ),
        (
            "qgh://github.com/issue/I_EVAL_105",
            topic_vector(&[(AXIS_DIRECT_LOCATOR, 1.0)]),
        ),
        (
            "qgh://github.com/issue/I_EVAL_106",
            topic_vector(&[
                (AXIS_OAUTH_LOGIN, 1.0),
                (AXIS_AUTH, 0.8),
                (AXIS_KOREAN, 0.5),
            ]),
        ),
        (
            "qgh://github.com/issue/I_EVAL_107",
            topic_vector(&[(AXIS_INDEX_REBUILD, 1.0), (AXIS_KOREAN, 0.8)]),
        ),
        (
            "qgh://github.com/issue/I_EVAL_108",
            topic_vector(&[(AXIS_CALLBACK, 1.0), (AXIS_AUTH, 0.7), (AXIS_KOREAN, 0.7)]),
        ),
        (
            "qgh://github.com/issue-comment/IC_EVAL_201",
            topic_vector(&[
                (AXIS_DEPLOY_ROLLBACK, 1.0),
                (AXIS_SYNC, 0.4),
                (AXIS_PREVIOUS_INDEX, 0.3),
            ]),
        ),
        (
            "qgh://github.com/issue-comment/IC_EVAL_202",
            topic_vector(&[(AXIS_CACHE_REPLAY, 1.0), (AXIS_INDEX_REBUILD, 0.5)]),
        ),
        (
            "qgh://github.com/issue-comment/IC_EVAL_203",
            topic_vector(&[(AXIS_PUBLISH_RACE, 1.0), (AXIS_INDEX_REBUILD, 0.4)]),
        ),
        (
            "qgh://github.com/issue-comment/IC_EVAL_204",
            topic_vector(&[(AXIS_PREVIOUS_INDEX, 1.0), (AXIS_INDEX_REBUILD, 0.8)]),
        ),
        (
            "qgh://github.com/issue-comment/IC_EVAL_205",
            topic_vector(&[
                (AXIS_NO_HOSTED_VECTOR, 1.0),
                (AXIS_KOREAN, 0.7),
                (AXIS_DEPLOY_ROLLBACK, 0.3),
            ]),
        ),
    ]
    .into_iter()
    .collect()
}

fn eval_query_vectors_json<'a>(
    cases: impl Iterator<Item = &'a EvalCase>,
    authored_query_vectors: &BTreeMap<&'static str, Vec<f32>>,
) -> String {
    let mut query_vectors = BTreeMap::<&str, Vec<f32>>::new();
    for case in cases {
        if let Some(vector) = authored_query_vectors.get(case.query) {
            query_vectors.insert(case.query, vector.clone());
        } else {
            assert!(
                !case.requires_hybrid_path(),
                "missing deterministic eval query vector for `{}`",
                case.query
            );
        }
    }
    serde_json::to_string(&query_vectors).unwrap()
}

fn eval_document_vectors_json(source_vectors: &BTreeMap<&'static str, Vec<f32>>) -> String {
    let document_vectors = source_vectors
        .iter()
        .map(|(source_id, vector)| {
            (
                format!("eval embedding chunk for {source_id}"),
                vector.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    serde_json::to_string(&document_vectors).unwrap()
}

fn eval_base_query_vectors() -> BTreeMap<&'static str, Vec<f32>> {
    [
        (
            "Release gate schema drift",
            topic_vector(&[(AXIS_SCHEMA, 1.0), (AXIS_OUTPUT_SCHEMA, 0.9)]),
        ),
        (
            "Token source env fallback",
            topic_vector(&[(AXIS_TOKEN_PRIVACY, 1.0)]),
        ),
        (
            "pagination cursor duplicate etag",
            topic_vector(&[(AXIS_PAGINATION, 1.0), (AXIS_SYNC, 0.7)]),
        ),
        (
            "retry-after secondary rate limit backoff",
            topic_vector(&[(AXIS_RATE_LIMIT, 1.0), (AXIS_SYNC, 0.5), (AXIS_STATUS, 0.8)]),
        ),
        (
            "schema envelope validation strict additionalProperties",
            topic_vector(&[(AXIS_SCHEMA, 1.0), (AXIS_OUTPUT_SCHEMA, 0.9)]),
        ),
        (
            "env token source reference",
            topic_vector(&[(AXIS_TOKEN_PRIVACY, 1.0)]),
        ),
        (
            "blue deploy rollback playbook",
            topic_vector(&[
                (AXIS_DEPLOY_ROLLBACK, 1.0),
                (AXIS_SYNC, 0.4),
                (AXIS_PREVIOUS_INDEX, 0.3),
            ]),
        ),
        (
            "cache invalidation workaround shard map",
            topic_vector(&[(AXIS_CACHE_REPLAY, 1.0), (AXIS_INDEX_REBUILD, 0.5)]),
        ),
        (
            "race condition reproduction clock skew",
            topic_vector(&[(AXIS_PUBLISH_RACE, 1.0), (AXIS_INDEX_REBUILD, 0.4)]),
        ),
        (
            "operator handoff note stale generation",
            topic_vector(&[(AXIS_PREVIOUS_INDEX, 1.0), (AXIS_INDEX_REBUILD, 0.8)]),
        ),
        (
            "인증토큰",
            topic_vector(&[
                (AXIS_OAUTH_LOGIN, 1.0),
                (AXIS_AUTH, 0.8),
                (AXIS_KOREAN, 0.5),
            ]),
        ),
        (
            "로그인실패",
            topic_vector(&[
                (AXIS_OAUTH_LOGIN, 1.0),
                (AXIS_AUTH, 0.8),
                (AXIS_KOREAN, 0.5),
            ]),
        ),
        (
            "색인재빌드",
            topic_vector(&[(AXIS_INDEX_REBUILD, 1.0), (AXIS_KOREAN, 0.8)]),
        ),
        (
            "배포오류",
            topic_vector(&[
                (AXIS_NO_HOSTED_VECTOR, 1.0),
                (AXIS_KOREAN, 0.7),
                (AXIS_DEPLOY_ROLLBACK, 0.3),
            ]),
        ),
        (
            "OAuth콜백실패",
            topic_vector(&[(AXIS_CALLBACK, 1.0), (AXIS_AUTH, 0.7), (AXIS_KOREAN, 0.7)]),
        ),
        (
            "sync misses changed issues when pages repeat",
            topic_vector(&[(AXIS_PAGINATION, 1.0), (AXIS_SYNC, 0.7)]),
        ),
        (
            "how should workers recover after a bad blue deployment",
            topic_vector(&[
                (AXIS_DEPLOY_ROLLBACK, 1.0),
                (AXIS_SYNC, 0.4),
                (AXIS_PREVIOUS_INDEX, 0.3),
            ]),
        ),
        (
            "where is secondary API throttling surfaced during local search",
            topic_vector(&[(AXIS_RATE_LIMIT, 1.0), (AXIS_SYNC, 0.5), (AXIS_STATUS, 0.8)]),
        ),
        (
            "why did JSON output reject an extra envelope field",
            topic_vector(&[(AXIS_SCHEMA, 1.0), (AXIS_OUTPUT_SCHEMA, 0.9)]),
        ),
        (
            "what prevents saved secrets from leaking in logs",
            topic_vector(&[(AXIS_TOKEN_PRIVACY, 1.0)]),
        ),
        (
            "why should issue number lookup avoid ambiguous text ranking",
            topic_vector(&[(AXIS_DIRECT_LOCATOR, 1.0)]),
        ),
        (
            "why did login fail after the OAuth flow",
            topic_vector(&[
                (AXIS_OAUTH_LOGIN, 1.0),
                (AXIS_AUTH, 0.8),
                (AXIS_KOREAN, 0.5),
            ]),
        ),
        (
            "how are Korean search results preserved during an index rebuild",
            topic_vector(&[(AXIS_INDEX_REBUILD, 1.0), (AXIS_KOREAN, 0.8)]),
        ),
        (
            "which callback failure mixes Korean and English auth text",
            topic_vector(&[(AXIS_CALLBACK, 1.0), (AXIS_AUTH, 0.7), (AXIS_KOREAN, 0.7)]),
        ),
        (
            "what fixes dirty index task replay after shard mapping changes",
            topic_vector(&[(AXIS_CACHE_REPLAY, 1.0), (AXIS_INDEX_REBUILD, 0.5)]),
        ),
        (
            "which note explains stale generation after a publish race",
            topic_vector(&[(AXIS_PUBLISH_RACE, 1.0), (AXIS_INDEX_REBUILD, 0.4)]),
        ),
        (
            "which handoff says to keep using the previous index generation",
            topic_vector(&[(AXIS_PREVIOUS_INDEX, 1.0), (AXIS_INDEX_REBUILD, 0.8)]),
        ),
        (
            "페이지 반복 때 변경된 이슈 누락 방지",
            topic_vector(&[(AXIS_PAGINATION, 1.0), (AXIS_SYNC, 0.7)]),
        ),
        (
            "보조 rate limit 대기 상태는 어디에 보이나",
            topic_vector(&[(AXIS_RATE_LIMIT, 1.0), (AXIS_SYNC, 0.5), (AXIS_STATUS, 0.8)]),
        ),
        (
            "스키마 출력에 추가 필드를 금지해야 하나",
            topic_vector(&[(AXIS_SCHEMA, 1.0), (AXIS_OUTPUT_SCHEMA, 0.9)]),
        ),
        (
            "토큰을 설정 파일이나 로그에 저장하지 않는 규칙",
            topic_vector(&[(AXIS_TOKEN_PRIVACY, 1.0)]),
        ),
        (
            "Korean comments disappear during index rebuild",
            topic_vector(&[(AXIS_INDEX_REBUILD, 1.0), (AXIS_KOREAN, 0.8)]),
        ),
        (
            "OAuth token refresh missing causes login failure",
            topic_vector(&[
                (AXIS_OAUTH_LOGIN, 1.0),
                (AXIS_AUTH, 0.8),
                (AXIS_KOREAN, 0.5),
            ]),
        ),
        (
            "deployment error without hosted vector provider",
            topic_vector(&[
                (AXIS_NO_HOSTED_VECTOR, 1.0),
                (AXIS_KOREAN, 0.7),
                (AXIS_DEPLOY_ROLLBACK, 0.3),
            ]),
        ),
        (
            "Korean callback failure analysis",
            topic_vector(&[(AXIS_CALLBACK, 1.0), (AXIS_AUTH, 0.7), (AXIS_KOREAN, 0.7)]),
        ),
    ]
    .into_iter()
    .collect()
}

fn add_model_axis(vectors: &mut BTreeMap<&'static str, Vec<f32>>, axis: usize) {
    for vector in vectors.values_mut() {
        vector[axis] = 0.05;
    }
}

fn topic_vector(weighted_axes: &[(usize, f32)]) -> Vec<f32> {
    let mut vector = vec![0.0; EVAL_VECTOR_DIMENSION];
    for (axis, weight) in weighted_axes {
        vector[*axis] = *weight;
    }
    vector
}

fn eval_embedding_fingerprint(model_id: &str) -> qgh::embedding::EmbeddingFingerprint {
    EmbeddingFingerprintSeed {
        provider: "local".to_string(),
        model_id: model_id.to_string(),
        model_revision: DEFAULT_HF_MODEL_REVISION.to_string(),
        pooling: PoolingKind::Cls,
        query_prefix: DEFAULT_QUERY_PREFIX.to_string(),
    }
    .with_dimension(EVAL_VECTOR_DIMENSION)
}

fn register_sqlite_vec_extension() {
    type SqliteVecEntryPoint = unsafe extern "C" fn(
        db: *mut rusqlite::ffi::sqlite3,
        pz_err_msg: *mut *const c_char,
        p_api: *const rusqlite::ffi::sqlite3_api_routines,
    ) -> c_int;
    let entry_point = unsafe {
        std::mem::transmute::<unsafe extern "C" fn(), SqliteVecEntryPoint>(
            sqlite_vec::sqlite3_vec_init,
        )
    };
    let rc = unsafe { rusqlite::ffi::sqlite3_auto_extension(Some(entry_point)) };
    assert_eq!(rc, rusqlite::ffi::SQLITE_OK);
}

struct EvalFixture {
    root: PathBuf,
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

    fn write_config_with_embedding_model(&self, api_base_url: &str, model_id: &str) {
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
provider = "local"
model = "hf:{model_id}"
file = "onnx/model_quantized.onnx"
pooling = "cls"
query_prefix = "query: "
"#
        );
        fs::write(self.config_home.join("qgh/config.toml"), config).unwrap();
    }

    fn qgh(&self, args: &[&str]) -> Output {
        self.qgh_with_embedding_vectors(args, None, None)
    }

    fn qgh_with_query_vectors(&self, args: &[&str], query_vectors_json: Option<&str>) -> Output {
        self.qgh_with_embedding_vectors(args, query_vectors_json, None)
    }

    fn qgh_with_embedding_vectors(
        &self,
        args: &[&str],
        query_vectors_json: Option<&str>,
        document_vectors_json: Option<&str>,
    ) -> Output {
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
            .current_dir(&self.root)
            .args(["--profile", "work"])
            .args(args);
        if let Some(query_vectors_json) = query_vectors_json {
            cmd.env(TEST_EMBEDDING_QUERY_VECTORS_ENV, query_vectors_json);
        }
        if let Some(document_vectors_json) = document_vectors_json {
            cmd.env(TEST_EMBEDDING_DOCUMENT_VECTORS_ENV, document_vectors_json);
        }
        cmd.output().unwrap()
    }

    fn seed_eval_chunks(&self, source_vectors: &BTreeMap<&'static str, Vec<f32>>) {
        register_sqlite_vec_extension();
        let conn = Connection::open(self.db_path()).unwrap();
        conn.execute("DELETE FROM chunk_embeddings", []).unwrap();
        conn.execute("DELETE FROM chunks", []).unwrap();
        conn.execute("UPDATE embedding_fingerprints SET active = 0", [])
            .unwrap();

        for source_id in source_vectors.keys() {
            let source_version_id: i64 = conn
                .query_row(
                    "SELECT coalesce(im.latest_version_id, cm.latest_version_id)
                     FROM source_entities se
                     LEFT JOIN issue_metadata im ON im.source_id = se.source_id
                     LEFT JOIN comment_metadata cm ON cm.source_id = se.source_id
                     WHERE se.source_id = ?1 AND se.lifecycle_state = 'active'",
                    [*source_id],
                    |row| row.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT INTO chunks (source_id, source_version_id, body)
                 VALUES (?1, ?2, ?3)",
                params![
                    *source_id,
                    source_version_id,
                    format!("eval embedding chunk for {source_id}")
                ],
            )
            .unwrap();
        }
    }

    fn embed_eval_vectors(
        &self,
        candidate: EvalModelCandidate,
        source_vectors: &BTreeMap<&'static str, Vec<f32>>,
    ) -> String {
        let fingerprint = eval_embedding_fingerprint(candidate.model_id);
        let fingerprint_hash = fingerprint.hash();
        let document_vectors_json = eval_document_vectors_json(source_vectors);
        let embed = self.qgh_with_embedding_vectors(
            &["embed", "--force", "--json"],
            None,
            Some(&document_vectors_json),
        );
        assert_success(&embed);
        let embed_json = stdout_json(&embed);
        assert_eq!(embed_json["data"]["embedding_state"], "refreshed");
        assert_eq!(
            embed_json["data"]["chunks"]["embedded"],
            source_vectors.len(),
            "qgh embed --force must embed every candidate document vector"
        );
        fingerprint_hash
    }

    fn assert_active_eval_fingerprint(
        &self,
        candidate: EvalModelCandidate,
        fingerprint_hash: &str,
        expected_embeddings: usize,
    ) {
        let conn = Connection::open(self.db_path()).unwrap();
        let active_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM embedding_fingerprints WHERE active = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_count, 1, "expected one active eval fingerprint");
        let (active_hash, active_model_id): (String, String) = conn
            .query_row(
                "SELECT fingerprint_hash, model_id
                 FROM embedding_fingerprints
                 WHERE active = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(active_hash, fingerprint_hash);
        assert_eq!(active_model_id, candidate.model_id);
        let active_embedding_count: i64 = conn
            .query_row(
                "SELECT count(*)
                 FROM chunk_embeddings ce
                 JOIN embedding_fingerprints ef ON ef.id = ce.fingerprint_id
                 WHERE ef.active = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_embedding_count as usize, expected_embeddings);
    }

    fn active_eval_vector_table_count(&self) -> usize {
        register_sqlite_vec_extension();
        let conn = Connection::open(self.db_path()).unwrap();
        conn.query_row(
            &format!("SELECT count(*) FROM {CHUNK_EMBEDDING_VECTORS_TABLE}"),
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap() as usize
    }

    fn db_path(&self) -> PathBuf {
        self.data_home.join("qgh/profiles/work/qgh.sqlite3")
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
            "Env token source reference must not persist literal tokens in config or logs.",
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
