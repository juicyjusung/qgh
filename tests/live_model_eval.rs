use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value, STORED, STRING, TEXT};
use tantivy::{Index, TantivyDocument, Term};

#[cfg(feature = "fastembed-provider")]
#[path = "support/live_model_eval_runtime.rs"]
mod live_model_eval_runtime;

const CORPUS_JSONL: &str = include_str!("fixtures/live-model-eval/corpus.jsonl");
const DEV_QRELS_JSONL: &str = include_str!("fixtures/live-model-eval/qrels-dev.jsonl");
const TEST_QRELS_JSONL: &str = include_str!("fixtures/live-model-eval/qrels-test.jsonl");
const PROVENANCE_JSON: &str = include_str!("fixtures/live-model-eval/provenance.json");
const MODEL_PREP_SCRIPT: &str = include_str!("support/prepare_live_model_eval_models.py");
const RUNTIME_SUPPORT: &str = include_str!("support/live_model_eval_runtime.rs");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusRecord {
    schema_version: String,
    source_id: String,
    entity_type: String,
    repo: String,
    issue_number: u64,
    canonical_url: String,
    title: String,
    body: String,
    github_updated_at: String,
    body_sha256: String,
    snapshot_at: String,
    license: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum QueryClass {
    EnglishSemantic,
    KoreanSemantic,
    KoQueryEnSource,
    EnQueryKoSource,
    ExactIdentifier,
    CommentOnly,
    LongContext,
    Negative,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RelevantSource {
    source_id: String,
    grade: u8,
    rationale: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryFilters {
    repo: String,
    #[serde(default)]
    issue_number: Option<u64>,
    #[serde(default)]
    source_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QrelRecord {
    schema_version: String,
    query_id: String,
    split: String,
    query: String,
    #[serde(rename = "class")]
    query_class: QueryClass,
    relevant: Vec<RelevantSource>,
    filters: QueryFilters,
    rationale: String,
    labeler: String,
    adjudicators: Vec<String>,
    ambiguous: bool,
    second_adjudication: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcquisitionProvenance {
    method: String,
    authentication: String,
    raw_response_committed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryProvenance {
    repo: String,
    visibility: String,
    license: String,
    repo_url: String,
    issues_api: String,
    source_count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureProvenance {
    schema_version: String,
    snapshot_at: String,
    acquisition: AcquisitionProvenance,
    repositories: Vec<RepositoryProvenance>,
    exclusions: Vec<String>,
    exclusion_counts: ExclusionCounts,
    adjudication: AdjudicationProvenance,
    judgment_pool: JudgmentPoolProvenance,
    corpus_sha256: String,
    qrels_dev_sha256: String,
    qrels_test_sha256: String,
    dev_query_count: usize,
    test_query_count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExclusionCounts {
    absolute_local_path: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JudgmentPoolProvenance {
    method: String,
    complete: bool,
    multi_source_query_count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdjudicationProvenance {
    method: String,
    ambiguous_candidate_policy: String,
    title_only_paraphrases_allowed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
struct QueryMetrics {
    ndcg_at_10: f64,
    mrr_at_10: f64,
    recall_at_5: f64,
    recall_at_10: f64,
    recall_at_20: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
struct ResourceMetrics {
    snapshot_bytes: u64,
    peak_rss_bytes: u64,
    cold_start_ms: f64,
    warm_query_p50_ms: f64,
    warm_query_p95_ms: f64,
    indexing_chunks_per_second: f64,
    db_bytes_per_chunk: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceTier {
    Light,
    Quality,
}

#[derive(Clone, Copy)]
struct LexicalFields {
    source_id: Field,
    entity_type: Field,
    repo: Field,
    issue_number: Field,
    title: Field,
    body: Field,
    cjk_ngrams: Field,
}

struct LexicalEvalIndex {
    index: Index,
    fields: LexicalFields,
    exact_issues: BTreeMap<(String, u64), String>,
}

#[test]
fn public_corpus_records_are_strict_and_hash_addressed() {
    let records = CORPUS_JSONL
        .lines()
        .map(|line| serde_json::from_str::<CorpusRecord>(line).expect("strict corpus record"))
        .collect::<Vec<_>>();

    assert!(!records.is_empty());
    for record in records {
        assert_eq!(record.schema_version, "qgh.live_model_corpus.v1");
        assert!(record.source_id.starts_with("qgh://github.com/"));
        assert!(matches!(
            record.entity_type.as_str(),
            "issue" | "issue_comment"
        ));
        assert!(!record.repo.is_empty());
        assert!(record.issue_number > 0);
        assert!(record.canonical_url.starts_with("https://github.com/"));
        assert!(!record.title.is_empty());
        assert!(!record.body.is_empty());
        assert!(record.github_updated_at.ends_with('Z'));
        assert_eq!(record.body_sha256.len(), 64);
        assert_eq!(
            record.body_sha256,
            format!("{:x}", Sha256::digest(record.body))
        );
        assert!(record.snapshot_at.ends_with('Z'));
        assert!(!record.license.is_empty());
    }
}

#[test]
fn public_corpus_excludes_absolute_local_paths() {
    let records = parse_jsonl::<CorpusRecord>(CORPUS_JSONL);
    assert!(records
        .iter()
        .all(|record| !record.title.contains("/Users/") && !record.body.contains("/Users/")));
}

#[test]
fn qrels_are_strict_balanced_and_split_by_issue_thread() {
    assert_qrels_contract(CORPUS_JSONL, DEV_QRELS_JSONL, TEST_QRELS_JSONL);
}

#[test]
fn qrels_include_pooled_alternates_and_two_adjudicators() {
    let corpus = parse_jsonl::<CorpusRecord>(CORPUS_JSONL);
    let source_issues = corpus
        .iter()
        .map(|source| (source.source_id.as_str(), source.issue_number))
        .collect::<BTreeMap<_, _>>();
    let test = parse_jsonl::<QrelRecord>(TEST_QRELS_JSONL);
    let multi_source = test
        .iter()
        .filter(|qrel| qrel.relevant.len() > 1)
        .collect::<Vec<_>>();
    assert!(multi_source.len() >= 10);
    assert!(test.iter().all(|qrel| qrel.adjudicators.len() >= 2));
    assert!(multi_source
        .iter()
        .flat_map(|qrel| &qrel.relevant)
        .any(|judgment| judgment.grade < 3));
    let test_001 = test
        .iter()
        .find(|qrel| qrel.query_id == "test-001")
        .unwrap();
    assert_eq!(
        test_001
            .relevant
            .iter()
            .filter_map(|judgment| source_issues.get(judgment.source_id.as_str()))
            .copied()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([1, 2])
    );
}

#[test]
fn provenance_matches_the_committed_public_snapshot() {
    assert_provenance_contract(
        PROVENANCE_JSON,
        CORPUS_JSONL,
        DEV_QRELS_JSONL,
        TEST_QRELS_JSONL,
    );
}

#[test]
fn metric_math_uses_graded_ndcg_and_source_level_recall() {
    let metrics = metrics_for(
        &[("source-a", 3), ("source-b", 1)],
        &["distractor", "source-b", "source-a"],
    );
    let expected_dcg = 1.0 / 3.0_f64.log2() + 7.0 / 4.0_f64.log2();
    let expected_idcg = 7.0 + 1.0 / 3.0_f64.log2();
    assert!((metrics.ndcg_at_10 - expected_dcg / expected_idcg).abs() < 1e-12);
    assert!((metrics.mrr_at_10 - 0.5).abs() < 1e-12);
    assert!((metrics.recall_at_5 - 1.0).abs() < 1e-12);
    assert!((metrics.recall_at_10 - 1.0).abs() < 1e-12);
    assert!((metrics.recall_at_20 - 1.0).abs() < 1e-12);
}

#[test]
fn normal_query_events_redact_raw_query_and_body() {
    let raw_query = "RAW_QUERY_SENTINEL";
    let raw_body = "RAW_BODY_SENTINEL";
    let event = redacted_query_event(
        "test-001",
        QueryClass::EnglishSemantic,
        raw_query,
        &["qgh://github.com/issue/source-a"],
        metrics_for(
            &[("qgh://github.com/issue/source-a", 3)],
            &["qgh://github.com/issue/source-a"],
        ),
    );
    let rendered = serde_json::to_string(&event).unwrap();
    assert!(!rendered.contains(raw_query));
    assert!(!rendered.contains(raw_body));
    assert_eq!(event["query_sha256"], digest_hex(raw_query));
    assert_eq!(
        event
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "class".to_string(),
            "metrics".to_string(),
            "query_id".to_string(),
            "query_sha256".to_string(),
            "ranked_source_ids".to_string(),
        ])
    );
}

#[test]
fn resource_gate_reports_each_blocking_dimension() {
    let light = ResourceMetrics {
        snapshot_bytes: 500 * 1024 * 1024,
        peak_rss_bytes: 1024 * 1024 * 1024,
        cold_start_ms: 5_000.0,
        warm_query_p50_ms: 800.0,
        warm_query_p95_ms: 1_500.0,
        indexing_chunks_per_second: 10.0,
        db_bytes_per_chunk: 3.0 * 1024.0,
    };
    assert!(resource_gate(ResourceTier::Light, light).is_empty());

    let too_slow = ResourceMetrics {
        warm_query_p95_ms: 1_500.1,
        cold_start_ms: 10_000.1,
        indexing_chunks_per_second: 2.9,
        ..light
    };
    assert_eq!(
        resource_gate(ResourceTier::Quality, too_slow),
        vec![
            "warm_query_p95_ms",
            "cold_start_ms",
            "indexing_chunks_per_second"
        ]
    );
}

#[test]
fn bm25_baseline_applies_hard_filters_and_round_trips() {
    let corpus = parse_jsonl::<CorpusRecord>(CORPUS_JSONL);
    let qrels = parse_jsonl::<QrelRecord>(TEST_QRELS_JSONL);
    let index = LexicalEvalIndex::build(&corpus).expect("build live BM25 fixture");
    let source_map = corpus
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let mut exact_hits = 0usize;
    let mut exact_queries = 0usize;
    for qrel in &qrels {
        let ranked = index.search(qrel, 20).expect("BM25 query");
        for source_id in &ranked {
            let source = source_map
                .get(source_id.as_str())
                .expect("every BM25 hit must round-trip through the corpus");
            assert_eq!(source.repo, qrel.filters.repo);
            if let Some(issue_number) = qrel.filters.issue_number {
                assert_eq!(source.issue_number, issue_number);
            }
            if let Some(source_type) = qrel.filters.source_type.as_deref() {
                assert_eq!(source.entity_type, source_type);
            }
        }
        if qrel.query_class == QueryClass::ExactIdentifier {
            exact_queries += 1;
            exact_hits += usize::from(ranked.first().is_some_and(|source_id| {
                qrel.relevant
                    .iter()
                    .any(|relevant| relevant.source_id == *source_id)
            }));
        }
    }
    assert!(exact_queries > 0);
    assert!(exact_hits as f64 / exact_queries as f64 >= 0.95);
}

#[test]
fn live_runtime_entrypoint_is_explicitly_opt_in() {
    assert!(!live_eval_opt_in(None));
    assert!(!live_eval_opt_in(Some("0")));
    assert!(!live_eval_opt_in(Some("true")));
    assert!(live_eval_opt_in(Some("1")));
}

#[test]
fn prepared_manifests_target_context_v1() {
    assert!(MODEL_PREP_SCRIPT.contains(r#""context_template_version": "qgh.context.v1""#));
    assert!(!MODEL_PREP_SCRIPT.contains("qgh.context.none.v1"));
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn dev_grid_fuses_real_branch_observations_with_deterministic_ties() {
    let ranked = live_model_eval_runtime::fuse_for_test(
        &[
            ("source-a", Some(10.0), Some(0.40)),
            ("source-b", Some(9.0), Some(0.10)),
            ("source-c", Some(8.0), Some(0.20)),
        ],
        60,
        2,
    );
    assert_eq!(ranked, ["source-b", "source-a", "source-c"]);
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn metadata_context_templates_are_exact_and_versioned() {
    assert_eq!(
        live_model_eval_runtime::context_input_for_test(
            "issue",
            "github.com",
            "juicyjusung/qgh",
            47,
            "Hybrid search",
            "chunk body",
        ),
        "Repository: github.com/juicyjusung/qgh\nIssue #47: Hybrid search\n\nchunk body"
    );
    assert_eq!(
        live_model_eval_runtime::context_input_for_test(
            "issue_comment",
            "github.com",
            "juicyjusung/qgh",
            47,
            "Hybrid search",
            "comment chunk",
        ),
        "Repository: github.com/juicyjusung/qgh\nComment on issue #47: Hybrid search\n\ncomment chunk"
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn tier_selection_uses_size_for_light_and_quality_for_quality() {
    let candidates = [
        ("large-best", 900_u64, 0.95_f64, 0.80_f64, true),
        ("small-near", 400_u64, 0.948_f64, 0.82_f64, true),
        ("tiny-too-far", 100_u64, 0.80_f64, 0.99_f64, true),
    ];
    assert_eq!(
        live_model_eval_runtime::select_tier_for_test(&candidates, true),
        Some("small-near".to_string())
    );
    assert_eq!(
        live_model_eval_runtime::select_tier_for_test(&candidates, false),
        Some("small-near".to_string())
    );

    let quality_tie = [
        ("larger", 800_u64, 0.950_f64, 0.88_f64, true),
        ("smaller", 400_u64, 0.948_f64, 0.88_f64, true),
    ];
    assert_eq!(
        live_model_eval_runtime::select_tier_for_test(&quality_tie, false),
        Some("smaller".to_string())
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn target_root_rejects_lookalikes_and_parent_traversal() {
    let cwd = std::env::current_dir().unwrap();
    assert!(live_model_eval_runtime::target_root_allowed_for_test(
        &cwd,
        std::path::Path::new("target/qgh-eval/run")
    ));
    assert!(!live_model_eval_runtime::target_root_allowed_for_test(
        &cwd,
        std::path::Path::new("target/qgh-eval-evil/run")
    ));
    assert!(!live_model_eval_runtime::target_root_allowed_for_test(
        &cwd,
        std::path::Path::new("target/qgh-eval/../escape")
    ));
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn hybrid_gate_requires_every_model_scored_query_to_use_hybrid() {
    assert!(live_model_eval_runtime::hybrid_gate_for_test(20, 20));
    assert!(!live_model_eval_runtime::hybrid_gate_for_test(20, 19));
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn weighted_quality_uses_the_frozen_class_weights() {
    assert!((live_model_eval_runtime::weighted_score_for_test([1.0; 6]) - 1.0).abs() < 1e-12);
    assert!(
        (live_model_eval_runtime::weighted_score_for_test([0.0, 0.0, 1.0, 0.0, 0.0, 0.0]) - 0.10)
            .abs()
            < 1e-12
    );
    assert!(
        (live_model_eval_runtime::weighted_score_for_test([0.0, 0.0, 0.0, 0.0, 1.0, 1.0]) - 0.10)
            .abs()
            < 1e-12
    );
}

#[test]
fn heldout_parse_occurs_after_frozen_config_write() {
    let freeze = RUNTIME_SUPPORT
        .find("fs::write(root.join(\"frozen-config.json\")")
        .unwrap();
    let heldout_parse = RUNTIME_SUPPORT
        .find("parse_jsonl::<QrelRecord>(test_raw)")
        .unwrap();
    assert!(freeze < heldout_parse);
}

#[test]
fn runtime_records_schema_fingerprints_and_derives_redaction_state() {
    assert!(RUNTIME_SUPPORT.contains("database_schema_fingerprint"));
    assert!(RUNTIME_SUPPORT.contains("tantivy_schema_fingerprint"));
    assert!(!RUNTIME_SUPPORT.contains("raw_query_or_body_logged: false"));
}

#[cfg(feature = "fastembed-provider")]
#[test]
#[ignore = "downloads/loads local ONNX models; set QGH_LIVE_MODEL_EVAL=1"]
fn live_model_runtime_evaluation() {
    assert!(
        live_eval_opt_in(std::env::var("QGH_LIVE_MODEL_EVAL").ok().as_deref()),
        "set QGH_LIVE_MODEL_EVAL=1 to run the opt-in live evaluation"
    );
    let root = std::env::var_os("QGH_LIVE_MODEL_EVAL_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target/qgh-eval"));
    live_model_eval_runtime::run(
        &root,
        CORPUS_JSONL,
        DEV_QRELS_JSONL,
        TEST_QRELS_JSONL,
        PROVENANCE_JSON,
    )
    .expect("live model evaluation completes");
}

#[cfg(feature = "fastembed-provider")]
#[test]
#[ignore = "runs the release qgh binary against the qgh-only public snapshot"]
fn live_model_runtime_smoke() {
    assert_eq!(
        std::env::var("QGH_LIVE_MODEL_EVAL_SMOKE").as_deref(),
        Ok("1"),
        "set QGH_LIVE_MODEL_EVAL_SMOKE=1 to run the runtime smoke"
    );
    let root = std::env::var_os("QGH_LIVE_MODEL_EVAL_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target/qgh-eval"));
    live_model_eval_runtime::run_smoke(&root, CORPUS_JSONL, DEV_QRELS_JSONL)
        .expect("qgh-only runtime smoke completes");
}

#[cfg(feature = "fastembed-provider")]
#[test]
#[ignore = "loads one local model and probes stored issue/comment context hashes"]
fn live_model_context_contract_smoke() {
    assert_eq!(
        std::env::var("QGH_LIVE_MODEL_EVAL_CONTEXT_SMOKE").as_deref(),
        Ok("1"),
        "set QGH_LIVE_MODEL_EVAL_CONTEXT_SMOKE=1 to run the context probe"
    );
    let root = std::env::var_os("QGH_LIVE_MODEL_EVAL_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target/qgh-eval"));
    let manifest = std::env::var_os("QGH_LIVE_MODEL_EVAL_CONTEXT_MANIFEST")
        .map(std::path::PathBuf::from)
        .expect("set QGH_LIVE_MODEL_EVAL_CONTEXT_MANIFEST");
    live_model_eval_runtime::run_context_probe_smoke(&root, CORPUS_JSONL, &manifest)
        .expect("context contract runtime smoke completes");
}

fn parse_jsonl<T: for<'de> Deserialize<'de>>(raw: &str) -> Vec<T> {
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("strict JSONL record"))
        .collect()
}

fn assert_qrels_contract(corpus_raw: &str, dev_raw: &str, test_raw: &str) {
    let corpus = parse_jsonl::<CorpusRecord>(corpus_raw);
    let source_threads = corpus
        .iter()
        .map(|record| {
            (
                record.source_id.as_str(),
                (
                    record.repo.as_str(),
                    record.issue_number,
                    record.entity_type.as_str(),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let dev = parse_jsonl::<QrelRecord>(dev_raw);
    let test = parse_jsonl::<QrelRecord>(test_raw);
    assert_eq!(dev.len(), 40, "dev must contain exactly 40 queries");
    assert_eq!(
        test.len(),
        80,
        "held-out test must contain exactly 80 queries"
    );

    let expected_test_counts = BTreeMap::from([
        (QueryClass::EnglishSemantic, 20),
        (QueryClass::KoreanSemantic, 15),
        (QueryClass::KoQueryEnSource, 10),
        (QueryClass::EnQueryKoSource, 10),
        (QueryClass::ExactIdentifier, 10),
        (QueryClass::CommentOnly, 5),
        (QueryClass::LongContext, 5),
        (QueryClass::Negative, 5),
    ]);
    let actual_test_counts = test.iter().fold(BTreeMap::new(), |mut counts, qrel| {
        *counts.entry(qrel.query_class).or_insert(0) += 1;
        counts
    });
    assert_eq!(actual_test_counts, expected_test_counts);

    let mut query_ids = BTreeSet::new();
    let mut dev_threads = BTreeSet::new();
    let mut test_threads = BTreeSet::new();
    for (expected_split, records, threads) in [
        ("dev", &dev, &mut dev_threads),
        ("test", &test, &mut test_threads),
    ] {
        for qrel in records {
            assert_eq!(qrel.schema_version, "qgh.live_model_qrel.v1");
            assert_eq!(qrel.split, expected_split);
            assert!(
                query_ids.insert(qrel.query_id.as_str()),
                "duplicate query id"
            );
            assert!(!qrel.query.trim().is_empty());
            assert!(!qrel.rationale.trim().is_empty());
            assert!(!qrel.labeler.trim().is_empty());
            assert!(qrel.adjudicators.len() >= 2);
            assert!(
                !qrel.ambiguous,
                "ambiguous records must be adjudicated or excluded"
            );
            assert!(qrel.second_adjudication.is_none());
            if qrel.query_class == QueryClass::Negative {
                assert!(qrel.relevant.is_empty());
                continue;
            }
            assert!(!qrel.relevant.is_empty());
            for relevant in &qrel.relevant {
                assert!((1..=3).contains(&relevant.grade));
                assert!(!relevant.rationale.trim().is_empty());
                let (repo, issue_number, entity_type) = source_threads
                    .get(relevant.source_id.as_str())
                    .expect("gold source must exist in corpus");
                assert_eq!(*repo, qrel.filters.repo);
                if let Some(expected_issue) = qrel.filters.issue_number {
                    assert_eq!(*issue_number, expected_issue);
                }
                if let Some(expected_type) = qrel.filters.source_type.as_deref() {
                    assert_eq!(*entity_type, expected_type);
                }
                threads.insert((*repo, *issue_number));
            }
        }
    }
    let leakage = dev_threads.intersection(&test_threads).collect::<Vec<_>>();
    assert!(
        leakage.is_empty(),
        "issue/comment split leakage: {leakage:?}"
    );
}

fn digest_hex(raw: &str) -> String {
    format!("{:x}", Sha256::digest(raw.as_bytes()))
}

fn assert_provenance_contract(
    provenance_raw: &str,
    corpus_raw: &str,
    dev_raw: &str,
    test_raw: &str,
) {
    let provenance: FixtureProvenance =
        serde_json::from_str(provenance_raw).expect("strict provenance record");
    assert_eq!(provenance.schema_version, "qgh.live_model_provenance.v1");
    assert!(provenance.snapshot_at.ends_with('Z'));
    assert_eq!(
        provenance.acquisition.method,
        "unauthenticated GitHub REST API"
    );
    assert_eq!(provenance.acquisition.authentication, "none");
    assert!(!provenance.acquisition.raw_response_committed);
    assert_eq!(provenance.corpus_sha256, digest_hex(corpus_raw));
    assert_eq!(provenance.qrels_dev_sha256, digest_hex(dev_raw));
    assert_eq!(provenance.qrels_test_sha256, digest_hex(test_raw));
    assert_eq!(provenance.dev_query_count, 40);
    assert_eq!(provenance.test_query_count, 80);
    assert!(provenance
        .exclusions
        .iter()
        .any(|value| value.contains("secret-like")));

    let corpus = parse_jsonl::<CorpusRecord>(corpus_raw);
    assert!(corpus
        .iter()
        .all(|record| record.snapshot_at == provenance.snapshot_at));
    let source_counts = corpus.iter().fold(BTreeMap::new(), |mut counts, record| {
        *counts.entry(record.repo.as_str()).or_insert(0usize) += 1;
        counts
    });
    assert_eq!(provenance.repositories.len(), 1);
    assert_eq!(provenance.repositories[0].repo, "juicyjusung/qgh");
    assert_eq!(provenance.adjudication.method, "manual source-body review");
    assert_eq!(
        provenance.adjudication.ambiguous_candidate_policy,
        "second adjudication or exclusion"
    );
    assert!(!provenance.adjudication.title_only_paraphrases_allowed);
    assert_eq!(provenance.exclusion_counts.absolute_local_path, 2);
    assert!(provenance.judgment_pool.complete);
    assert!(provenance.judgment_pool.multi_source_query_count >= 10);
    assert!(provenance
        .judgment_pool
        .method
        .contains("source-body overlap review"));
    for repository in provenance.repositories {
        assert_eq!(repository.visibility, "public");
        assert!(!repository.license.is_empty());
        assert_eq!(
            repository.repo_url,
            format!("https://github.com/{}", repository.repo)
        );
        assert!(repository
            .issues_api
            .starts_with("https://api.github.com/repos/"));
        assert_eq!(
            source_counts.get(repository.repo.as_str()),
            Some(&repository.source_count)
        );
    }
}

fn metrics_for<R: AsRef<str>, S: AsRef<str>>(relevant: &[(R, u8)], ranked: &[S]) -> QueryMetrics {
    let grades = relevant
        .iter()
        .map(|(source_id, grade)| (source_id.as_ref(), *grade))
        .collect::<BTreeMap<_, _>>();
    let recall_at = |cutoff: usize| {
        if grades.is_empty() {
            return 0.0;
        }
        let found = ranked
            .iter()
            .take(cutoff)
            .filter(|source_id| grades.contains_key(source_id.as_ref()))
            .map(AsRef::as_ref)
            .collect::<BTreeSet<_>>()
            .len();
        found as f64 / grades.len() as f64
    };
    let mrr_at_10 = ranked
        .iter()
        .take(10)
        .position(|source_id| grades.contains_key(source_id.as_ref()))
        .map_or(0.0, |index| 1.0 / (index + 1) as f64);
    let dcg_at_10 = ranked
        .iter()
        .take(10)
        .enumerate()
        .map(|(index, source_id)| {
            let grade = grades.get(source_id.as_ref()).copied().unwrap_or(0);
            (2_f64.powi(i32::from(grade)) - 1.0) / (index as f64 + 2.0).log2()
        })
        .sum::<f64>();
    let mut ideal_grades = grades.values().copied().collect::<Vec<_>>();
    ideal_grades.sort_unstable_by(|left, right| right.cmp(left));
    let ideal_dcg_at_10 = ideal_grades
        .into_iter()
        .take(10)
        .enumerate()
        .map(|(index, grade)| (2_f64.powi(i32::from(grade)) - 1.0) / (index as f64 + 2.0).log2())
        .sum::<f64>();
    QueryMetrics {
        ndcg_at_10: if ideal_dcg_at_10 > 0.0 {
            dcg_at_10 / ideal_dcg_at_10
        } else {
            0.0
        },
        mrr_at_10,
        recall_at_5: recall_at(5),
        recall_at_10: recall_at(10),
        recall_at_20: recall_at(20),
    }
}

fn redacted_query_event(
    query_id: &str,
    query_class: QueryClass,
    raw_query: &str,
    ranked_source_ids: &[&str],
    metrics: QueryMetrics,
) -> serde_json::Value {
    json!({
        "query_id": query_id,
        "query_sha256": digest_hex(raw_query),
        "class": query_class,
        "ranked_source_ids": ranked_source_ids,
        "metrics": metrics,
    })
}

fn resource_gate(tier: ResourceTier, metrics: ResourceMetrics) -> Vec<&'static str> {
    const GIB: u64 = 1024 * 1024 * 1024;
    let mut failures = Vec::new();
    match tier {
        ResourceTier::Light => {
            if metrics.snapshot_bytes > 500 * 1024 * 1024 {
                failures.push("snapshot_bytes");
            }
            if metrics.peak_rss_bytes > GIB {
                failures.push("peak_rss_bytes");
            }
            if metrics.cold_start_ms > 5_000.0 {
                failures.push("cold_start_ms");
            }
            if metrics.warm_query_p95_ms > 1_500.0 {
                failures.push("warm_query_p95_ms");
            }
            if metrics.indexing_chunks_per_second < 10.0 {
                failures.push("indexing_chunks_per_second");
            }
            if metrics.db_bytes_per_chunk > 3.0 * 1024.0 {
                failures.push("db_bytes_per_chunk");
            }
        }
        ResourceTier::Quality => {
            if metrics.warm_query_p95_ms > 1_500.0 {
                failures.push("warm_query_p95_ms");
            }
            if metrics.cold_start_ms > 10_000.0 {
                failures.push("cold_start_ms");
            }
            if metrics.peak_rss_bytes > 5 * GIB / 2 {
                failures.push("peak_rss_bytes");
            }
            if metrics.indexing_chunks_per_second < 3.0 {
                failures.push("indexing_chunks_per_second");
            }
        }
    }
    failures
}

impl LexicalEvalIndex {
    fn build(corpus: &[CorpusRecord]) -> Result<Self, Box<dyn Error>> {
        let mut schema = Schema::builder();
        let fields = LexicalFields {
            source_id: schema.add_text_field("source_id", STRING | STORED),
            entity_type: schema.add_text_field("entity_type", STRING | STORED),
            repo: schema.add_text_field("repo", STRING | STORED),
            issue_number: schema.add_text_field("issue_number", STRING | STORED),
            title: schema.add_text_field("title", TEXT | STORED),
            body: schema.add_text_field("body", TEXT | STORED),
            cjk_ngrams: schema.add_text_field("cjk_ngrams", TEXT),
        };
        let index = Index::create_in_ram(schema.build());
        let mut writer = index.writer(20_000_000)?;
        let mut exact_issues = BTreeMap::new();
        for source in corpus {
            let mut document = TantivyDocument::default();
            document.add_text(fields.source_id, &source.source_id);
            document.add_text(fields.entity_type, &source.entity_type);
            document.add_text(fields.repo, &source.repo);
            document.add_text(fields.issue_number, source.issue_number.to_string());
            document.add_text(fields.title, &source.title);
            document.add_text(fields.body, &source.body);
            document.add_text(
                fields.cjk_ngrams,
                cjk_ngrams(&format!("{} {}", source.title, source.body)),
            );
            writer.add_document(document)?;
            if source.entity_type == "issue" {
                exact_issues.insert(
                    (source.repo.clone(), source.issue_number),
                    source.source_id.clone(),
                );
            }
        }
        writer.commit()?;
        Ok(Self {
            index,
            fields,
            exact_issues,
        })
    }

    fn search(&self, qrel: &QrelRecord, limit: usize) -> Result<Vec<String>, Box<dyn Error>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        if qrel.query_class == QueryClass::ExactIdentifier {
            return Ok(qrel
                .filters
                .issue_number
                .and_then(|number| {
                    self.exact_issues
                        .get(&(qrel.filters.repo.clone(), number))
                        .cloned()
                })
                .into_iter()
                .collect());
        }
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let parser = QueryParser::for_index(
            &self.index,
            vec![self.fields.title, self.fields.body, self.fields.cjk_ngrams],
        );
        let query_text = expand_cjk_query(&qrel.query);
        let text_query = parser.parse_query(&query_text)?;
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, text_query)];
        clauses.push((
            Occur::Must,
            exact_term_query(self.fields.repo, &qrel.filters.repo),
        ));
        if let Some(source_type) = qrel.filters.source_type.as_deref() {
            clauses.push((
                Occur::Must,
                exact_term_query(self.fields.entity_type, source_type),
            ));
        }
        if let Some(issue_number) = qrel.filters.issue_number {
            clauses.push((
                Occur::Must,
                exact_term_query(self.fields.issue_number, &issue_number.to_string()),
            ));
        }
        let query = BooleanQuery::new(clauses);
        let documents = searcher.search(&query, &TopDocs::with_limit(limit))?;
        documents
            .into_iter()
            .map(|(_, address)| {
                let document = searcher.doc::<TantivyDocument>(address)?;
                document
                    .get_first(self.fields.source_id)
                    .and_then(|value| value.as_str())
                    .map(ToString::to_string)
                    .ok_or_else(|| "BM25 result is missing source_id".into())
            })
            .collect()
    }
}

fn exact_term_query(field: Field, value: &str) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_text(field, value),
        IndexRecordOption::Basic,
    ))
}

fn expand_cjk_query(query: &str) -> String {
    let expanded = cjk_ngrams(query);
    if expanded.is_empty() {
        query.to_string()
    } else {
        format!("{query} {expanded}")
    }
}

fn cjk_ngrams(text: &str) -> String {
    let mut terms = Vec::new();
    let mut run = Vec::new();
    for character in text.chars() {
        if is_cjk(character) {
            run.push(character);
        } else {
            push_cjk_ngrams(&run, &mut terms);
            run.clear();
        }
    }
    push_cjk_ngrams(&run, &mut terms);
    terms.join(" ")
}

fn push_cjk_ngrams(run: &[char], terms: &mut Vec<String>) {
    for size in 2..=3 {
        for window in run.windows(size) {
            terms.push(window.iter().collect());
        }
    }
}

fn is_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x3040..=0x30ff | 0x3400..=0x9fff | 0xac00..=0xd7af
    )
}

fn live_eval_opt_in(value: Option<&str>) -> bool {
    value == Some("1")
}
