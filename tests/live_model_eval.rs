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

#[cfg(feature = "fastembed-provider")]
#[test]
fn mcp_get_tombstone_envelope_counts_as_stale_round_trip_failure() {
    let private_message = "PRIVATE BODY MUST NOT ESCAPE";
    let evidence = live_model_eval_runtime::get_round_trip_evidence_for_test(
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": {
                "structuredContent": {
                    "ok": false,
                    "error": {
                        "code": "source.tombstoned",
                        "message": private_message,
                    },
                },
            },
        }),
        "qgh://github.com/issue/EXPECTED",
    )
    .expect("typed get error envelope is evidence, not a transport failure");

    assert_eq!(
        evidence,
        json!({
            "total": 1,
            "success": 0,
            "stale": 1,
            "quality_gate_failures": [
                "get_round_trip",
                "unexpected_tombstone_during_get",
            ],
        })
    );
    assert!(!serde_json::to_string(&evidence)
        .unwrap()
        .contains(private_message));
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

#[cfg(feature = "fastembed-provider")]
#[test]
fn fresh_blind_runtime_entrypoint_is_explicitly_opt_in() {
    assert!(!fresh_blind_eval_opt_in(None));
    assert!(!fresh_blind_eval_opt_in(Some("true")));
    assert!(fresh_blind_eval_opt_in(Some("1")));
}

#[test]
fn prepared_manifests_target_context_v1() {
    assert!(MODEL_PREP_SCRIPT.contains(r#""context_template_version": "qgh.context.v1""#));
    assert!(!MODEL_PREP_SCRIPT.contains("qgh.context.none.v1"));
}

#[test]
fn live_resource_protocol_uses_production_runtime_constants() {
    assert!(RUNTIME_SUPPORT.contains("FASTEMBED_BATCH_SIZE"));
    assert!(RUNTIME_SUPPORT.contains("FASTEMBED_INTRA_OP_THREADS"));
    assert!(!RUNTIME_SUPPORT.contains("batch_size_8_unavailable_existing_runtime_hardcodes_16"));
    assert!(!RUNTIME_SUPPORT.contains("intra_op_threads_4_not_exposed"));
}

#[test]
fn model_preparation_defines_the_lightweight_candidate_set() {
    for candidate in [
        "granite-embedding-97m-multilingual-r2",
        "dragonkue-koen-e5-tiny",
        "multilingual-e5-small",
        "multilingual-e5-small-ko-v2",
    ] {
        assert!(MODEL_PREP_SCRIPT.contains(candidate), "missing {candidate}");
        assert!(
            RUNTIME_SUPPORT.contains(candidate),
            "runtime missing {candidate}"
        );
    }
    assert!(MODEL_PREP_SCRIPT.contains("835ad14087e140460703cf0fae09f97d469d65c2"));
    assert!(MODEL_PREP_SCRIPT.contains("292c09c78c71a3f00ed56ee0d1ed9f0d39182fc9"));
    assert!(MODEL_PREP_SCRIPT.contains("614241f622f53c4eeff9890bdc4f31cfecc418b3"));
    assert!(MODEL_PREP_SCRIPT.contains("fcfc26bf355882620c48df58be112275bd756f50"));
    assert!(!MODEL_PREP_SCRIPT.contains("model_quint8_avx2.onnx"));
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn bm25_complement_metrics_count_rescue_preservation_and_harm() {
    let qrels = parse_jsonl::<QrelRecord>(DEV_QRELS_JSONL)
        .into_iter()
        .filter(|qrel| !qrel.relevant.is_empty())
        .take(3)
        .collect::<Vec<_>>();
    assert_eq!(qrels.len(), 3);
    let relevant = qrels
        .iter()
        .map(|qrel| qrel.relevant[0].source_id.clone())
        .collect::<Vec<_>>();
    let bm25 = BTreeMap::from([
        (qrels[0].query_id.clone(), vec!["irrelevant-a".to_string()]),
        (qrels[1].query_id.clone(), vec![relevant[1].clone()]),
        (qrels[2].query_id.clone(), vec![relevant[2].clone()]),
    ]);
    let hybrid = BTreeMap::from([
        (qrels[0].query_id.clone(), vec![relevant[0].clone()]),
        (qrels[1].query_id.clone(), vec!["irrelevant-b".to_string()]),
        (qrels[2].query_id.clone(), vec![relevant[2].clone()]),
    ]);

    let metrics = live_model_eval_runtime::bm25_complement_for_test(&qrels, &bm25, &hybrid);

    assert_eq!(metrics["positive_query_count"], 3);
    assert_eq!(metrics["bm25_miss_at_5"], 1);
    assert_eq!(metrics["rescued_at_5"], 1);
    assert_eq!(metrics["bm25_hit_preserved_at_5"], 1);
    assert_eq!(metrics["bm25_hit_harmed_at_5"], 1);
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn bm25_rescue_selection_prefers_net_rescue_then_smaller_snapshot() {
    let selected = live_model_eval_runtime::select_bm25_rescue_candidate_for_test(&[
        ("large-negative", 4, 3, 400),
        ("large-positive", 3, 1, 300),
        ("small-positive", 3, 1, 100),
    ]);

    assert_eq!(selected.as_deref(), Some("small-positive"));
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn resource_rss_watchdog_stops_an_over_limit_child() {
    let evidence = live_model_eval_runtime::rss_watchdog_for_test()
        .expect("resource watchdog returns structured evidence");

    assert_eq!(evidence["rss_cap_exceeded"], true);
    assert!(evidence["peak_rss_bytes"].as_u64().unwrap() > 1);
    assert!(evidence["elapsed_ms"].as_f64().unwrap() < 2_000.0);
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn resource_rss_watchdog_fails_closed_when_rss_cannot_be_observed() {
    let evidence = live_model_eval_runtime::rss_monitor_failure_for_test()
        .expect("resource watchdog returns structured monitor-failure evidence");

    assert_eq!(evidence["rss_monitor_failed"], true);
    assert_eq!(evidence["rss_cap_exceeded"], false);
    assert!(evidence["elapsed_ms"].as_f64().unwrap() < 2_000.0);
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn live_fixture_repository_allowlist_is_derived_from_the_public_corpus() {
    let corpus = [
        json!({"repo": "public-b/repo"}),
        json!({"repo": "public-a/repo"}),
        json!({"repo": "public-b/repo"}),
    ]
    .into_iter()
    .map(|record| serde_json::to_string(&record).unwrap())
    .collect::<Vec<_>>()
    .join("\n");

    assert_eq!(
        live_model_eval_runtime::repository_allowlist_for_test(&corpus).unwrap(),
        json!(["public-a/repo", "public-b/repo"])
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn real_manifest_tokenizer_contract_drives_frozen_and_resource_chunker_identity() {
    let artifact = |role: &str, relative_path: &str, marker: u8| {
        json!({
            "role": role,
            "relative_path": relative_path,
            "sha256": format!("{marker:02x}").repeat(32),
            "byte_size": u64::from(marker) + 1,
        })
    };
    let manifest = json!({
        "schema_version": "qgh.model_manifest.v1",
        "preset_id": null,
        "provider": "fastembed",
        "model_source": {"type": "local", "declared_id": "public-fixture"},
        "artifacts": [
            artifact("onnx_model", "onnx/model.onnx", 1),
            artifact("tokenizer", "tokenizer.json", 2),
            artifact("config", "config.json", 3),
            artifact("special_tokens_map", "special_tokens_map.json", 4),
            artifact("tokenizer_config", "tokenizer_config.json", 5),
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
        "context_template_version": "qgh.context.v1",
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let contract =
        live_model_eval_runtime::tokenizer_chunker_contract_for_test(&manifest_bytes, None, None)
            .expect("valid real manifest derives its exact tokenizer/chunker contract");
    let tokenizer_identity = contract["tokenizer_contract_identity"].as_str().unwrap();
    let chunker_fingerprint = contract["chunker_fingerprint"].as_str().unwrap();
    assert_eq!(tokenizer_identity.len(), 64);
    assert!(chunker_fingerprint.starts_with("markdown-token-v2:"));
    assert_eq!(chunker_fingerprint.len(), "markdown-token-v2:".len() + 64);
    assert_eq!(
        contract.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["chunker_fingerprint", "tokenizer_contract_identity"]
    );

    let mut tokenizer_tamper = manifest.clone();
    tokenizer_tamper["artifacts"][1]["sha256"] = json!("ff".repeat(32));
    assert!(
        live_model_eval_runtime::tokenizer_chunker_contract_for_test(
            &serde_json::to_vec(&tokenizer_tamper).unwrap(),
            Some(tokenizer_identity),
            Some(chunker_fingerprint),
        )
        .is_err()
    );

    let mut model_only_tamper = manifest;
    model_only_tamper["artifacts"][0]["sha256"] = json!("ee".repeat(32));
    live_model_eval_runtime::tokenizer_chunker_contract_for_test(
        &serde_json::to_vec(&model_only_tamper).unwrap(),
        Some(tokenizer_identity),
        Some(chunker_fingerprint),
    )
    .expect("non-tokenizer artifact does not change the tokenizer contract");

    for required in [
        "tokenizer_contract_identity_from_manifest",
        "chunker_fingerprint_for_tokenizer_identity",
        "tokenizer_contract_identity",
        "qgh.live_model_eval_config.v6",
        "qgh.live_model_eval_report.v5",
        "qgh.live_model_eval_candidate.v3",
        "qgh.live_model_eval_resource.v2",
    ] {
        assert!(RUNTIME_SUPPORT.contains(required), "missing {required}");
    }
    assert!(RUNTIME_SUPPORT.contains(
        "seed_50k_chunks(\n        &fixture.db_path(),\n        &chunk.body,\n        chunk.raw_token_count,\n        &tokenizer_chunker_contract.chunker_fingerprint,"
    ));
}

#[test]
fn model_preparation_records_download_and_cache_source_bytes() {
    let root = std::env::temp_dir().join(format!(
        "qgh-live-model-prepare-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let models_root = root.join("models");
    for (candidate, paths) in [
        (
            "gte-modernbert-base",
            vec![
                "onnx/model.onnx",
                "tokenizer.json",
                "config.json",
                "special_tokens_map.json",
                "tokenizer_config.json",
            ],
        ),
        (
            "arctic-embed-l-v2.0",
            vec![
                "onnx/model.onnx",
                "onnx/model.onnx_data",
                "tokenizer.json",
                "config.json",
                "special_tokens_map.json",
                "tokenizer_config.json",
            ],
        ),
        (
            "granite-embedding-97m-multilingual-r2",
            vec![
                "onnx/model.onnx",
                "tokenizer.json",
                "config.json",
                "special_tokens_map.json",
                "tokenizer_config.json",
            ],
        ),
        (
            "dragonkue-koen-e5-tiny",
            vec![
                "onnx/model.onnx",
                "tokenizer.json",
                "config.json",
                "special_tokens_map.json",
                "tokenizer_config.json",
            ],
        ),
        (
            "multilingual-e5-small",
            vec![
                "onnx/model.onnx",
                "tokenizer.json",
                "config.json",
                "special_tokens_map.json",
                "tokenizer_config.json",
            ],
        ),
    ] {
        for path in paths {
            let path = models_root.join(candidate).join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"public-test-artifact").unwrap();
        }
    }
    let output = std::process::Command::new("python3")
        .args([
            "tests/support/prepare_live_model_eval_models.py",
            "--output-root",
        ])
        .arg(&models_root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["schema_version"], "qgh.live_model_preparation.v1");
    assert_eq!(stdout["prepared"][0]["download_transfer_bytes"], 0);
    assert!(
        stdout["prepared"][0]["existing_snapshot_bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        stdout["prepared"][0]["prepared_snapshot_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    let provenance: serde_json::Value = serde_json::from_slice(
        &std::fs::read(models_root.join("preparation-provenance.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(provenance, stdout);
    assert_eq!(
        provenance["unavailable"],
        json!([{
            "candidate": "dragonkue-ko",
            "model_id": "dragonkue/snowflake-arctic-embed-l-v2.0-ko",
            "resolved_revision": "55ec6e9358a56d56af759bc8372e970caf8c305f",
            "required_artifact": "onnx/model.onnx",
            "availability": "missing_at_immutable_revision",
            "checked_at": "2026-07-10T17:45:46Z",
            "authentication": "none",
            "evidence": {
                "revision_http_status": 200,
                "tree_http_status": 200,
                "tree_entry_count": 12,
                "required_artifact_matches": 0,
                "tree_sha256": "3440d1cf94a3c8664310e4b0b03cb57da5a7e132fea5fa6087618a580aee6219",
                "path_sha256": "9e4c07c5352f95ac48d195ab5be417240ab20f1f773da95836b5c69ec7337dc0",
                "resolve_http_status": 404,
                "resolve_error": "EntryNotFound",
                "resolve_revision": "55ec6e9358a56d56af759bc8372e970caf8c305f"
            }
        }])
    );
    #[cfg(feature = "fastembed-provider")]
    assert_eq!(
        live_model_eval_runtime::prepared_model_download_bytes_for_test(
            &root,
            "gte-modernbert-base",
            "Alibaba-NLP/gte-modernbert-base",
            "e7f32e3c00f91d699e8c43b53106206bcc72bb22",
        )
        .expect("Rust verifies Python preparation provenance"),
        0
    );
    #[cfg(feature = "fastembed-provider")]
    {
        let blocker = live_model_eval_runtime::dragonkue_blocker_for_test(&root)
            .expect("immutable unavailability evidence produces an offline blocker");
        assert_eq!(blocker["candidate"], "dragonkue-ko");
        assert_eq!(
            blocker["schema_version"],
            "qgh.live_model_eval_candidate.v3"
        );
        assert_eq!(blocker["status"], "blocked");
        assert_eq!(
            blocker["blocker"],
            json!({
                "code": "eval.model_artifact_missing_at_immutable_revision",
                "phase": "preparation_provenance"
            })
        );
        assert_eq!(blocker["synthetic_substitution"], false);

        let provenance_path = models_root.join("preparation-provenance.json");
        let mut tampered_unavailable = provenance.clone();
        tampered_unavailable["unavailable"][0]["evidence"]["tree_sha256"] = json!("0".repeat(64));
        std::fs::write(
            &provenance_path,
            serde_json::to_vec_pretty(&tampered_unavailable).unwrap(),
        )
        .unwrap();
        assert!(live_model_eval_runtime::dragonkue_blocker_for_test(&root).is_err());
        std::fs::write(
            &provenance_path,
            serde_json::to_vec_pretty(&provenance).unwrap(),
        )
        .unwrap();
    }
    #[cfg(feature = "fastembed-provider")]
    {
        let provenance_path = models_root.join("preparation-provenance.json");
        let original = provenance.clone();

        let mut per_artifact_bytes = original.clone();
        per_artifact_bytes["prepared"][0]["artifact_acquisition"][0]["source_bytes"] = json!(
            per_artifact_bytes["prepared"][0]["artifact_acquisition"][0]["source_bytes"]
                .as_u64()
                .unwrap()
                + 1
        );
        per_artifact_bytes["prepared"][0]["existing_snapshot_bytes"] = json!(
            per_artifact_bytes["prepared"][0]["existing_snapshot_bytes"]
                .as_u64()
                .unwrap()
                + 1
        );
        std::fs::write(
            &provenance_path,
            serde_json::to_vec_pretty(&per_artifact_bytes).unwrap(),
        )
        .unwrap();
        assert!(
            live_model_eval_runtime::prepared_model_download_bytes_for_test(
                &root,
                "gte-modernbert-base",
                "Alibaba-NLP/gte-modernbert-base",
                "e7f32e3c00f91d699e8c43b53106206bcc72bb22",
            )
            .is_err()
        );

        let mut transfer_semantics = original.clone();
        let artifact_size = transfer_semantics["prepared"][0]["artifact_acquisition"][0]
            ["source_bytes"]
            .as_u64()
            .unwrap();
        transfer_semantics["prepared"][0]["artifact_acquisition"][0]["source"] = json!("curl");
        transfer_semantics["prepared"][0]["artifact_acquisition"][0]["download_transfer_bytes"] =
            json!(1);
        transfer_semantics["prepared"][0]["download_transfer_bytes"] = json!(1);
        transfer_semantics["prepared"][0]["existing_snapshot_bytes"] = json!(
            transfer_semantics["prepared"][0]["existing_snapshot_bytes"]
                .as_u64()
                .unwrap()
                - artifact_size
        );
        std::fs::write(
            &provenance_path,
            serde_json::to_vec_pretty(&transfer_semantics).unwrap(),
        )
        .unwrap();
        assert!(
            live_model_eval_runtime::prepared_model_download_bytes_for_test(
                &root,
                "gte-modernbert-base",
                "Alibaba-NLP/gte-modernbert-base",
                "e7f32e3c00f91d699e8c43b53106206bcc72bb22",
            )
            .is_err()
        );

        let mut aggregate = original;
        aggregate["prepared"][0]["download_transfer_bytes"] = json!(1);
        std::fs::write(
            &provenance_path,
            serde_json::to_vec_pretty(&aggregate).unwrap(),
        )
        .unwrap();
        assert!(
            live_model_eval_runtime::prepared_model_download_bytes_for_test(
                &root,
                "gte-modernbert-base",
                "Alibaba-NLP/gte-modernbert-base",
                "e7f32e3c00f91d699e8c43b53106206bcc72bb22",
            )
            .is_err()
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn model_preparation_offline_refresh_preserves_valid_acquisition_evidence() {
    let root = std::env::temp_dir().join(format!(
        "qgh-live-model-offline-prepare-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let models_root = root.join("models");
    for (candidate, paths) in [
        (
            "gte-modernbert-base",
            vec![
                "onnx/model.onnx",
                "tokenizer.json",
                "config.json",
                "special_tokens_map.json",
                "tokenizer_config.json",
            ],
        ),
        (
            "arctic-embed-l-v2.0",
            vec![
                "onnx/model.onnx",
                "onnx/model.onnx_data",
                "tokenizer.json",
                "config.json",
                "special_tokens_map.json",
                "tokenizer_config.json",
            ],
        ),
    ] {
        for path in paths {
            let path = models_root.join(candidate).join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"public-offline-artifact").unwrap();
        }
    }
    let run = |offline: bool| {
        let mut command = std::process::Command::new("python3");
        command
            .args([
                "tests/support/prepare_live_model_eval_models.py",
                "--output-root",
            ])
            .arg(&models_root)
            .args(["--candidates", "gte-modernbert-base,arctic-embed-l-v2.0"]);
        if offline {
            command.arg("--offline");
        }
        command.output().unwrap()
    };
    assert!(run(false).status.success());
    let provenance_path = models_root.join("preparation-provenance.json");
    let mut provenance: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&provenance_path).unwrap()).unwrap();
    let artifact_size = provenance["prepared"][0]["artifact_acquisition"][0]["source_bytes"]
        .as_u64()
        .unwrap();
    provenance["prepared"][0]["artifact_acquisition"][0]["source"] = json!("curl");
    provenance["prepared"][0]["artifact_acquisition"][0]["download_transfer_bytes"] =
        json!(artifact_size);
    provenance["prepared"][0]["download_transfer_bytes"] = json!(artifact_size);
    provenance["prepared"][0]["existing_snapshot_bytes"] = json!(
        provenance["prepared"][0]["existing_snapshot_bytes"]
            .as_u64()
            .unwrap()
            - artifact_size
    );
    std::fs::write(
        &provenance_path,
        serde_json::to_vec_pretty(&provenance).unwrap(),
    )
    .unwrap();

    let offline = run(true);
    assert!(
        offline.status.success(),
        "{}",
        String::from_utf8_lossy(&offline.stderr)
    );
    let refreshed: serde_json::Value = serde_json::from_slice(&offline.stdout).unwrap();
    assert_eq!(
        refreshed["prepared"][0]["artifact_acquisition"][0]["source"],
        "curl"
    );
    assert_eq!(
        refreshed["prepared"][0]["artifact_acquisition"][0]["download_transfer_bytes"],
        artifact_size
    );

    std::fs::write(
        models_root.join("gte-modernbert-base/onnx/model.onnx"),
        vec![b'x'; artifact_size as usize],
    )
    .unwrap();
    let changed_snapshot = run(true);
    assert!(changed_snapshot.status.success());
    let changed: serde_json::Value = serde_json::from_slice(&changed_snapshot.stdout).unwrap();
    assert_eq!(
        changed["prepared"][0]["artifact_acquisition"][0]["source"],
        "existing_snapshot"
    );
    assert_eq!(
        changed["prepared"][0]["artifact_acquisition"][0]["download_transfer_bytes"],
        0
    );

    std::fs::remove_file(models_root.join("gte-modernbert-base/tokenizer_config.json")).unwrap();
    let missing = run(true);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("offline preparation is missing"));
    assert!(!models_root
        .join("gte-modernbert-base/tokenizer_config.json.partial")
        .exists());
    std::fs::remove_dir_all(root).unwrap();
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
fn primary_dev_metrics_use_the_production_query_protocol() {
    let protocol = live_model_eval_runtime::dev_query_protocol_for_test();
    assert_eq!(protocol.primary_query_limit, 20);
    assert_eq!(protocol.primary_candidate_window, 80);
    assert_eq!(protocol.diagnostic_query_limit, 100);
    assert!(!protocol.diagnostic_can_select);
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn lexical_profile_freeze_contains_a_selected_profile_and_dev_report_binding() {
    assert!(!RUNTIME_SUPPORT.contains("pending_integrated_lane_d_ab"));
    assert_eq!(
        live_model_eval_runtime::lexical_profile_freeze_for_test(),
        json!({
            "production_profile": "production_v1",
            "comparison_candidate": "metadata_boost_v1",
            "selected_profile": "production_v1",
            "selection_reasons": ["weighted_ndcg_not_strictly_improved"],
            "dev_report_sha256": "report-sha256",
            "corpus_sha256": "corpus-sha256",
            "qrels_dev_sha256": "dev-sha256",
            "active_tantivy_generation": 7,
            "active_tantivy_path": "bm25-live/data/qgh/profiles/work/tantivy/generation-7",
            "tantivy_schema_fingerprint": "schema-sha256",
            "tantivy_generation_files_sha256": "files-sha256",
            "tantivy_generation_files": [{
                "relative_path": "meta.json",
                "byte_size": 42,
                "sha256": "meta-sha256",
            }],
            "heldout_confirmation_required": false,
            "heldout_fallback_profile": "production_v1",
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn promoted_metadata_profile_becomes_the_eval_baseline_and_heldout_fallback() {
    assert!(RUNTIME_SUPPORT
        .contains("let plan = LexicalProfileComparisonPlan::for_current_production()"));
    assert_eq!(
        live_model_eval_runtime::lexical_profile_comparison_plan_for_test("metadata_boost_v1"),
        json!({
            "production_profile": "metadata_boost_v1",
            "baseline_profile": "metadata_boost_v1",
            "comparison_candidate": "production_v1",
            "heldout_fallback_profile": "metadata_boost_v1",
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn lexical_profile_selects_metadata_only_on_strict_clean_dev_improvement() {
    let baseline = json!({
        "weighted_ndcg_at_10": 0.50,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.50, 0.50, 0.50, 0.50],
    });
    let candidate = json!({
        "weighted_ndcg_at_10": 0.51,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.50, 0.50, 0.50, 0.50],
    });

    assert_eq!(
        live_model_eval_runtime::lexical_profile_selection_for_test(baseline, candidate),
        json!({
            "selected_profile": "metadata_boost_v1",
            "reasons": [],
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn lexical_profile_keeps_v1_without_strict_weighted_dev_improvement() {
    let metrics = json!({
        "weighted_ndcg_at_10": 0.50,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.50, 0.50, 0.50, 0.50],
    });

    assert_eq!(
        live_model_eval_runtime::lexical_profile_selection_for_test(metrics.clone(), metrics),
        json!({
            "selected_profile": "production_v1",
            "reasons": ["weighted_ndcg_not_strictly_improved"],
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn post_promotion_selection_keeps_metadata_when_v1_is_not_strictly_better() {
    let metrics = json!({
        "weighted_ndcg_at_10": 0.50,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.50, 0.50, 0.50, 0.50],
    });

    assert_eq!(
        live_model_eval_runtime::lexical_profile_selection_for_production_for_test(
            "metadata_boost_v1",
            metrics.clone(),
            metrics,
        ),
        json!({
            "selected_profile": "metadata_boost_v1",
            "reasons": ["weighted_ndcg_not_strictly_improved"],
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn lexical_profile_rejects_exact_or_comment_only_regression() {
    let baseline = json!({
        "weighted_ndcg_at_10": 0.50,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.50, 0.50, 0.50, 0.50],
    });
    let exact_regression = json!({
        "weighted_ndcg_at_10": 0.60,
        "exact_top_1": 0.99,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.50, 0.50, 0.50, 0.50],
    });
    let comment_regression = json!({
        "weighted_ndcg_at_10": 0.60,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.49, 0.50, 0.50, 0.50],
    });

    assert_eq!(
        live_model_eval_runtime::lexical_profile_selection_for_test(
            baseline.clone(),
            exact_regression,
        ),
        json!({
            "selected_profile": "production_v1",
            "reasons": ["exact_identifier_regression"],
        })
    );
    assert_eq!(
        live_model_eval_runtime::lexical_profile_selection_for_test(baseline, comment_regression),
        json!({
            "selected_profile": "production_v1",
            "reasons": ["comment_only_regression"],
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn lexical_profile_rejects_filter_roundtrip_or_stale_regression() {
    let baseline = json!({
        "weighted_ndcg_at_10": 0.50,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.50, 0.50, 0.50, 0.50],
    });
    for (field, value, reason) in [
        ("hard_filter_violations", json!(1), "hard_filter_regression"),
        (
            "get_round_trip",
            json!(0.99),
            "query_get_round_trip_regression",
        ),
        ("stale_leakage", json!(1), "stale_leakage_regression"),
    ] {
        let mut candidate = baseline.clone();
        candidate["weighted_ndcg_at_10"] = json!(0.60);
        candidate[field] = value;
        let selection = live_model_eval_runtime::lexical_profile_selection_for_test(
            baseline.clone(),
            candidate,
        );
        assert_eq!(selection["selected_profile"], "production_v1");
        assert_eq!(selection["reasons"], json!([reason]));
    }
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn lexical_profile_rejects_nonempty_class_quality_gate_failures() {
    let baseline = json!({
        "weighted_ndcg_at_10": 0.50,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.50, 0.50, 0.50, 0.50],
        "quality_gate_failures": [],
    });
    let candidate = json!({
        "weighted_ndcg_at_10": 0.60,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.60, 0.60, 0.60, 0.60],
        "quality_gate_failures": ["korean_recall_at_5"],
    });

    assert_eq!(
        live_model_eval_runtime::lexical_profile_selection_for_test(baseline, candidate),
        json!({
            "selected_profile": "production_v1",
            "reasons": ["quality_gate_failure:korean_recall_at_5"],
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn heldout_confirmation_preserves_dev_selection_and_falls_back_to_v1_on_regression() {
    let production_v1 = json!({
        "weighted_ndcg_at_10": 0.55,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.55, 0.55, 0.55, 0.55],
        "quality_gate_failures": [],
    });
    let frozen_selected = json!({
        "weighted_ndcg_at_10": 0.54,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.55, 0.55, 0.55, 0.55],
        "quality_gate_failures": ["korean_recall_at_5"],
    });

    assert_eq!(
        live_model_eval_runtime::lexical_profile_heldout_confirmation_for_test(
            "frozen-config-sha256",
            "dev-report-sha256",
            "metadata_boost_v1",
            production_v1,
            frozen_selected,
        ),
        json!({
            "frozen_config_sha256": "frozen-config-sha256",
            "dev_report_sha256": "dev-report-sha256",
            "frozen_dev_selection": "metadata_boost_v1",
            "effective_profile": "production_v1",
            "promotion_eligible": false,
            "blockers": [
                "heldout_weighted_ndcg_regression",
                "quality_gate_failure:korean_recall_at_5",
            ],
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn heldout_confirmation_never_reselects_metadata_after_dev_kept_v1() {
    let production_v1 = json!({
        "weighted_ndcg_at_10": 0.50,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.50, 0.50, 0.50, 0.50],
        "quality_gate_failures": [],
    });
    let metadata_boost_v1 = json!({
        "weighted_ndcg_at_10": 0.60,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.60, 0.60, 0.60, 0.60],
        "quality_gate_failures": [],
    });

    assert_eq!(
        live_model_eval_runtime::lexical_profile_heldout_confirmation_for_test(
            "frozen-config-sha256",
            "dev-report-sha256",
            "production_v1",
            production_v1,
            metadata_boost_v1,
        ),
        json!({
            "frozen_config_sha256": "frozen-config-sha256",
            "dev_report_sha256": "dev-report-sha256",
            "frozen_dev_selection": "production_v1",
            "effective_profile": "production_v1",
            "promotion_eligible": false,
            "blockers": ["dev_selection_is_production_v1"],
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn post_promotion_heldout_recognizes_metadata_as_the_current_production_profile() {
    let metrics = json!({
        "weighted_ndcg_at_10": 0.60,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.60, 0.60, 0.60, 0.60],
        "quality_gate_failures": [],
    });

    assert_eq!(
        live_model_eval_runtime::lexical_profile_heldout_confirmation_for_production_for_test(
            "metadata_boost_v1",
            "frozen-config-sha256",
            "dev-report-sha256",
            "metadata_boost_v1",
            metrics.clone(),
            metrics,
        ),
        json!({
            "frozen_config_sha256": "frozen-config-sha256",
            "dev_report_sha256": "dev-report-sha256",
            "frozen_dev_selection": "metadata_boost_v1",
            "effective_profile": "metadata_boost_v1",
            "promotion_eligible": false,
            "blockers": ["dev_selection_is_metadata_boost_v1"],
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn post_promotion_heldout_artifact_names_the_production_baseline_truthfully() {
    assert_eq!(
        live_model_eval_runtime::lexical_profile_heldout_artifact_contract_for_test(
            "metadata_boost_v1",
        ),
        json!({
            "schema_version": "qgh.lexical_profile_heldout_confirmation.v2",
            "production_profile": "metadata_boost_v1",
            "has_production_baseline": true,
            "has_legacy_production_v1_key": false,
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn heldout_confirmation_allows_the_frozen_metadata_selection_only_when_clean() {
    let production_v1 = json!({
        "weighted_ndcg_at_10": 0.50,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.50, 0.50, 0.50, 0.50],
        "quality_gate_failures": [],
    });
    let metadata_boost_v1 = json!({
        "weighted_ndcg_at_10": 0.51,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.50, 0.50, 0.50, 0.50],
        "quality_gate_failures": [],
    });

    assert_eq!(
        live_model_eval_runtime::lexical_profile_heldout_confirmation_for_test(
            "frozen-config-sha256",
            "dev-report-sha256",
            "metadata_boost_v1",
            production_v1,
            metadata_boost_v1,
        ),
        json!({
            "frozen_config_sha256": "frozen-config-sha256",
            "dev_report_sha256": "dev-report-sha256",
            "frozen_dev_selection": "metadata_boost_v1",
            "effective_profile": "metadata_boost_v1",
            "promotion_eligible": true,
            "blockers": [],
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn heldout_confirmation_rejects_equal_weighted_ndcg_as_not_strictly_improved() {
    let metrics = json!({
        "weighted_ndcg_at_10": 0.50,
        "exact_top_1": 1.0,
        "hard_filter_violations": 0,
        "get_round_trip": 1.0,
        "stale_leakage": 0,
        "comment_only": [0.50, 0.50, 0.50, 0.50],
        "quality_gate_failures": [],
    });

    assert_eq!(
        live_model_eval_runtime::lexical_profile_heldout_confirmation_for_test(
            "frozen-config-sha256",
            "dev-report-sha256",
            "metadata_boost_v1",
            metrics.clone(),
            metrics,
        ),
        json!({
            "frozen_config_sha256": "frozen-config-sha256",
            "dev_report_sha256": "dev-report-sha256",
            "frozen_dev_selection": "metadata_boost_v1",
            "effective_profile": "production_v1",
            "promotion_eligible": false,
            "blockers": ["heldout_weighted_ndcg_not_strictly_improved"],
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn global_state_uses_heldout_v1_fallback_instead_of_the_rejected_dev_selection() {
    assert_eq!(
        live_model_eval_runtime::global_evaluation_for_test(
            false,
            false,
            "metadata_boost_v1",
            "production_v1",
            false,
            &["heldout_weighted_ndcg_not_strictly_improved"],
        ),
        json!({
            "promotion_eligible": false,
            "evaluation_state": "blocked_fresh_production_v1_model_evaluation_required",
            "promotion_blockers": [
                "lexical_profile_heldout_rejected",
                "heldout_weighted_ndcg_not_strictly_improved",
                "fresh_production_v1_model_evaluation_required",
            ],
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn post_promotion_rejected_v1_candidate_requests_fresh_metadata_production_evidence() {
    assert_eq!(
        live_model_eval_runtime::global_evaluation_for_production_for_test(
            false,
            "metadata_boost_v1",
            "production_v1",
            "metadata_boost_v1",
            false,
            &["heldout_weighted_ndcg_not_strictly_improved"],
        ),
        json!({
            "promotion_eligible": false,
            "evaluation_state": "blocked_fresh_metadata_boost_v1_model_evaluation_required",
            "promotion_blockers": [
                "lexical_profile_heldout_rejected",
                "heldout_weighted_ndcg_not_strictly_improved",
                "fresh_metadata_boost_v1_model_evaluation_required",
            ],
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn context_failure_is_candidate_local_when_another_candidate_is_eligible() {
    assert_eq!(
        live_model_eval_runtime::global_evaluation_for_test(
            true,
            true,
            "production_v1",
            "production_v1",
            false,
            &["dev_selection_is_production_v1"],
        ),
        json!({
            "promotion_eligible": true,
            "evaluation_state": "promotion_eligible",
            "promotion_blockers": [],
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn model_metrics_are_blocked_when_dev_selected_a_nonproduction_lexical_profile() {
    assert_eq!(
        qgh::search_eval::production_lexical_profile_for_eval(),
        qgh::search_eval::EvalLexicalProfile::ProductionV1,
    );
    assert_eq!(
        live_model_eval_runtime::model_candidate_lexical_gate_for_test("metadata_boost_v1"),
        json!({
            "can_run_model_metrics": false,
            "blocker_code": "eval.lexical_profile_promotion_required",
            "blocker_reason": "lexical_profile_promotion_required",
            "dev_metrics": null,
            "held_out_metrics": null,
        })
    );
    assert_eq!(
        live_model_eval_runtime::model_candidate_lexical_gate_for_test("production_v1"),
        json!({
            "can_run_model_metrics": true,
            "blocker_code": null,
            "blocker_reason": null,
            "dev_metrics": null,
            "held_out_metrics": null,
        })
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn resource_failure_preserves_heldout_quality_and_numeric_partial_evidence() {
    let report = live_model_eval_runtime::resource_failure_contract_for_test();
    assert!(report["held_out_metrics"].is_object());
    assert_eq!(report["resources"]["complete"], false);
    assert_eq!(
        report["resources"]["schema_version"],
        "qgh.live_model_eval_resource.v2"
    );
    assert_eq!(report["resources"]["phase"], "50k_embed");
    assert_eq!(
        report["resources"]["warm_path_includes_manifest_artifact_rehash"],
        false
    );
    assert_eq!(report["resources"]["measured_50k_chunk_count"], 12_500);
    assert_eq!(
        report["blocker"],
        json!({"code": "eval.resource_failed", "phase": "50k_embed"})
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn failing_resource_child_preserves_sanitized_numeric_evidence() {
    let evidence = live_model_eval_runtime::timed_failure_evidence_for_test()
        .expect("failing child returns typed numeric evidence");
    assert_eq!(evidence["embedded_chunks"], 12_500);
    assert!(evidence["elapsed_ms"].as_f64().unwrap() > 0.0);
    assert!(evidence["peak_rss_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        evidence.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["elapsed_ms", "embedded_chunks", "peak_rss_bytes"]
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn resource_seed_covers_every_active_latest_source_and_production_embed_is_exact() {
    let evidence = live_model_eval_runtime::resource_seed_embed_contract_for_test(
        std::path::Path::new(env!("CARGO_BIN_EXE_qgh")),
        CORPUS_JSONL,
    )
    .expect("50k resource corpus uses the production refresh/embed path");
    assert_eq!(evidence["seeded_chunks"], 50_000);
    assert!(evidence["active_latest_versions"].as_u64().unwrap() > 1);
    assert_eq!(evidence["active_latest_versions_without_chunks"], 0);
    assert!(
        evidence["minimum_chunks_per_active_version"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert_eq!(evidence["invalid_source_local_chunk_indices"], 0);
    assert_eq!(evidence["refreshed_chunks"], 0);
    assert_eq!(evidence["embedded_chunks"], 50_000);
    assert_eq!(evidence["post_embed_chunks"], 50_000);
    assert_eq!(
        evidence["tokenizer_contract_identity"],
        "qgh.debug-test-tokenizer-static.v1"
    );
    assert_eq!(
        evidence["chunker_fingerprint"],
        qgh::chunking::CHUNKER_FINGERPRINT
    );
    assert_eq!(evidence["distinct_chunker_fingerprints"], 1);
    assert_eq!(evidence["chunker_fingerprint_mismatch_rows"], 0);
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn backfill_success_requires_complete_generation_mapping_vec0_and_publication_counts() {
    let path = std::env::temp_dir().join(format!(
        "qgh-live-model-backfill-contract-{}-{}.sqlite3",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let connection = rusqlite::Connection::open(&path).unwrap();
    let chunker_fingerprint = format!("markdown-token-v2:{}", "a".repeat(64));
    connection
        .execute_batch(&format!(
            "CREATE TABLE chunks(id INTEGER PRIMARY KEY, chunker_fingerprint TEXT NOT NULL);
             CREATE TABLE embedding_generations(
               id INTEGER PRIMARY KEY, state TEXT, output_dimension INTEGER,
               total_chunks INTEGER, completed_chunks INTEGER, chunker_fingerprint TEXT NOT NULL
             );
             CREATE TABLE embedding_generation_chunks(generation_id INTEGER, chunk_id INTEGER);
             CREATE TABLE embedding_generation_vector_rows(
               generation_id INTEGER, vector_table TEXT, vector_rowid INTEGER
             );
             CREATE TABLE embedding_generation_vectors_d4(rowid INTEGER PRIMARY KEY);
             CREATE TABLE retrieval_publications(
               publication_id INTEGER PRIMARY KEY, embedding_generation_id INTEGER, active INTEGER
             );
             CREATE TABLE retrieval_publication_pointer(id INTEGER PRIMARY KEY, publication_id INTEGER);
             INSERT INTO chunks VALUES (1, '{chunker_fingerprint}'), (2, '{chunker_fingerprint}');
             INSERT INTO embedding_generations
               VALUES (7, 'active', 4, 2, 2, '{chunker_fingerprint}');
             INSERT INTO embedding_generation_chunks VALUES (7, 1), (7, 2);
             INSERT INTO embedding_generation_vector_rows
               VALUES (7, 'embedding_generation_vectors_d4', 1),
                      (7, 'embedding_generation_vectors_d4', 2);
             INSERT INTO embedding_generation_vectors_d4 VALUES (1), (2);
             INSERT INTO retrieval_publications VALUES (9, 7, 1);
             INSERT INTO retrieval_publication_pointer VALUES (1, 9);"
        ))
        .unwrap();
    drop(connection);
    let evidence =
        live_model_eval_runtime::backfill_integrity_for_test(&path, 2, &chunker_fingerprint)
            .expect("complete publication");
    assert_eq!(evidence["generation_total_chunks"], 2);
    assert_eq!(evidence["generation_output_dimension"], 4);
    assert_eq!(evidence["vector_table"], "embedding_generation_vectors_d4");
    assert_eq!(evidence["vector_mapping_rows"], 2);
    assert_eq!(evidence["vec0_rows"], 2);
    assert_eq!(evidence["chunker_fingerprint"], chunker_fingerprint);
    assert_eq!(
        evidence["generation_chunker_fingerprint"],
        chunker_fingerprint
    );
    assert_eq!(evidence["chunker_fingerprint_mismatch_rows"], 0);
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE chunks SET chunker_fingerprint = 'legacy-mismatch' WHERE id = 2",
            [],
        )
        .unwrap();
    drop(connection);
    assert!(
        live_model_eval_runtime::backfill_integrity_for_test(&path, 2, &chunker_fingerprint)
            .is_err()
    );
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE chunks SET chunker_fingerprint = ?1 WHERE id = 2",
            [&chunker_fingerprint],
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM embedding_generation_vectors_d4 WHERE rowid = 2",
            [],
        )
        .unwrap();
    drop(connection);
    assert!(
        live_model_eval_runtime::backfill_integrity_for_test(&path, 2, &chunker_fingerprint)
            .is_err()
    );
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "INSERT INTO embedding_generation_vectors_d4 VALUES (2);
             CREATE TABLE embedding_generation_vectors_d8(rowid INTEGER PRIMARY KEY);
             INSERT INTO embedding_generation_vectors_d8 VALUES (1), (2);
             UPDATE embedding_generation_vector_rows
             SET vector_table = 'embedding_generation_vectors_d8';",
        )
        .unwrap();
    drop(connection);
    assert!(
        live_model_eval_runtime::backfill_integrity_for_test(&path, 2, &chunker_fingerprint)
            .is_err()
    );
    std::fs::remove_file(path).unwrap();
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
fn parent_issue_title_change_invalidates_comment_context_hash_in_release_contract() {
    let before = qgh::context::prepare_embedding_input(
        qgh::context::EmbeddingSourceContext::Comment {
            repository: "github.com/owner/repo",
            parent_issue_number: 47,
            parent_issue_title: "Old title",
        },
        "Unchanged authoritative comment chunk.",
    );
    let after = qgh::context::prepare_embedding_input(
        qgh::context::EmbeddingSourceContext::Comment {
            repository: "github.com/owner/repo",
            parent_issue_number: 47,
            parent_issue_title: "New title",
        },
        "Unchanged authoritative comment chunk.",
    );

    assert_ne!(before.as_str(), after.as_str());
    assert_ne!(
        before.context_hash("manifest-hash", "chunker-fingerprint"),
        after.context_hash("manifest-hash", "chunker-fingerprint")
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn context_probe_uses_only_the_embedding_generation_in_the_active_publication() {
    let path = std::env::temp_dir().join(format!(
        "qgh-live-model-context-publication-{}-{}.sqlite3",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE embedding_generations(
               id INTEGER PRIMARY KEY, state TEXT, model_manifest_hash TEXT,
               context_template_version TEXT
             );
             CREATE TABLE embedding_generation_chunks(
               generation_id INTEGER, chunk_id INTEGER, context_hash TEXT
             );
             CREATE TABLE chunks(
               id INTEGER PRIMARY KEY, source_id TEXT, chunker_fingerprint TEXT, body TEXT
             );
             CREATE TABLE source_entities(
               source_id TEXT PRIMARY KEY, entity_type TEXT, host TEXT, repo TEXT
             );
             CREATE TABLE issue_metadata(source_id TEXT, issue_number INTEGER, title TEXT);
             CREATE TABLE comment_metadata(
               source_id TEXT, issue_number INTEGER, parent_issue_title TEXT
             );
             CREATE TABLE retrieval_publications(
               publication_id INTEGER PRIMARY KEY, embedding_generation_id INTEGER, active INTEGER
             );
             CREATE TABLE retrieval_publication_pointer(
               id INTEGER PRIMARY KEY, publication_id INTEGER
             );
             INSERT INTO embedding_generations VALUES
               (7, 'active', 'manifest-a', 'qgh.context.v1'),
               (8, 'ready', 'manifest-b', 'qgh.context.v1');
             INSERT INTO chunks VALUES
               (1, 'issue-source', 'chunker', 'issue chunk'),
               (2, 'comment-source', 'chunker', 'comment chunk');
             INSERT INTO source_entities VALUES
               ('issue-source', 'issue', 'github.com', 'juicyjusung/qgh'),
               ('comment-source', 'issue_comment', 'github.com', 'juicyjusung/qgh');
             INSERT INTO issue_metadata VALUES ('issue-source', 47, 'Active title');
             INSERT INTO comment_metadata VALUES ('comment-source', 47, 'Active title');
             INSERT INTO embedding_generation_chunks VALUES
               (7, 1, 'wrong-active-issue'),
               (7, 2, 'wrong-active-comment');
             INSERT INTO retrieval_publications VALUES (9, 7, 1);
             INSERT INTO retrieval_publication_pointer VALUES (1, 9);",
        )
        .unwrap();
    for (chunk_id, entity_type, body) in [
        (1_i64, "issue", "issue chunk"),
        (2_i64, "issue_comment", "comment chunk"),
    ] {
        let input = live_model_eval_runtime::context_input_for_test(
            entity_type,
            "github.com",
            "juicyjusung/qgh",
            47,
            "Active title",
            body,
        );
        let hash = qgh::context::embedding_context_hash(
            "manifest-b",
            "chunker",
            qgh::context::METADATA_CONTEXT_TEMPLATE_VERSION,
            &input,
        );
        connection
            .execute(
                "INSERT INTO embedding_generation_chunks VALUES (8, ?1, ?2)",
                rusqlite::params![chunk_id, hash],
            )
            .unwrap();
    }
    drop(connection);

    let evidence = live_model_eval_runtime::context_contract_for_test(
        &path,
        qgh::context::METADATA_CONTEXT_TEMPLATE_VERSION,
    )
    .expect("active publication is probeable");
    assert_eq!(evidence["passed"], false);
    assert_eq!(evidence["context_hash_mismatches"], 2);

    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute("DELETE FROM retrieval_publication_pointer", [])
        .unwrap();
    drop(connection);
    assert!(live_model_eval_runtime::context_contract_for_test(
        &path,
        qgh::context::METADATA_CONTEXT_TEMPLATE_VERSION,
    )
    .is_err());
    std::fs::remove_file(path).unwrap();
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn frozen_bm25_snapshot_rejects_an_active_publication_pointer_switch() {
    let root = std::env::temp_dir().join(format!(
        "qgh-live-model-frozen-tantivy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let generation_a = root.join("generation-7");
    let generation_b = root.join("generation-8");
    std::fs::create_dir_all(&generation_a).unwrap();
    std::fs::create_dir_all(&generation_b).unwrap();
    for path in [&generation_a, &generation_b] {
        std::fs::write(
            path.join("meta.json"),
            serde_json::to_vec(&json!({"schema": {"fields": []}})).unwrap(),
        )
        .unwrap();
    }
    let db_path = root.join("qgh.sqlite3");
    let connection = rusqlite::Connection::open(&db_path).unwrap();
    connection
        .execute_batch(&format!(
            "CREATE TABLE retrieval_publications(
               publication_id INTEGER PRIMARY KEY, tantivy_generation INTEGER, active INTEGER
             );
             CREATE TABLE retrieval_publication_pointer(
               id INTEGER PRIMARY KEY, publication_id INTEGER
             );
             CREATE TABLE index_generations(
               generation INTEGER PRIMARY KEY, path TEXT, active INTEGER
             );
             INSERT INTO retrieval_publications VALUES (70, 7, 1), (80, 8, 0);
             INSERT INTO retrieval_publication_pointer VALUES (1, 70);
             INSERT INTO index_generations VALUES
               (7, '{}', 1), (8, '{}', 0);",
            generation_a.display(),
            generation_b.display(),
        ))
        .unwrap();
    drop(connection);

    let frozen = live_model_eval_runtime::freeze_tantivy_snapshot_for_test(&db_path)
        .expect("active snapshot freezes");
    assert_eq!(frozen["path"], "generation-7");
    assert!(!serde_json::to_string(&frozen)
        .unwrap()
        .contains(root.to_string_lossy().as_ref()));
    let mut absolute_identity = frozen.clone();
    absolute_identity["path"] = json!(generation_a.to_string_lossy());
    assert!(
        live_model_eval_runtime::revalidate_tantivy_snapshot_for_test(&db_path, absolute_identity,)
            .is_err()
    );
    let connection = rusqlite::Connection::open(&db_path).unwrap();
    connection
        .execute_batch(
            "UPDATE retrieval_publications SET active = (publication_id = 80);
             UPDATE index_generations SET active = (generation = 8);
             UPDATE retrieval_publication_pointer SET publication_id = 80 WHERE id = 1;",
        )
        .unwrap();
    drop(connection);

    let error =
        live_model_eval_runtime::revalidate_tantivy_snapshot_for_test(&db_path, frozen.clone())
            .expect_err("pointer switch must invalidate the frozen run");
    assert_eq!(error.to_string(), "eval.frozen_identity_changed");

    let connection = rusqlite::Connection::open(&db_path).unwrap();
    connection
        .execute_batch(
            "UPDATE retrieval_publications SET active = (publication_id = 70);
             UPDATE index_generations SET active = (generation = 7);
             UPDATE retrieval_publication_pointer SET publication_id = 70 WHERE id = 1;",
        )
        .unwrap();
    drop(connection);
    std::fs::write(
        generation_a.join("meta.json"),
        serde_json::to_vec(&json!({"schema": {"fields": ["changed"]}})).unwrap(),
    )
    .unwrap();
    let error = live_model_eval_runtime::revalidate_tantivy_snapshot_for_test(&db_path, frozen)
        .expect_err("schema change within the same generation must invalidate the frozen run");
    assert_eq!(error.to_string(), "eval.frozen_identity_changed");
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn frozen_bm25_snapshot_rejects_same_generation_valid_index_replacement() {
    let root = std::env::temp_dir().join(format!(
        "qgh-live-model-frozen-index-files-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let generation = root.join("generation-7");
    let replacement = root.join("replacement");
    for (path, marker) in [(&generation, "original"), (&replacement, "replacement")] {
        std::fs::create_dir_all(path).unwrap();
        let mut schema = tantivy::schema::Schema::builder();
        let body = schema.add_text_field("body", tantivy::schema::TEXT | tantivy::schema::STORED);
        let index = tantivy::Index::create_in_dir(path, schema.build()).unwrap();
        let mut writer = index.writer(15_000_000).unwrap();
        let mut document = tantivy::TantivyDocument::default();
        document.add_text(body, marker);
        writer.add_document(document).unwrap();
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();
    }

    let db_path = root.join("qgh.sqlite3");
    let connection = rusqlite::Connection::open(&db_path).unwrap();
    connection
        .execute_batch(&format!(
            "CREATE TABLE retrieval_publications(
               publication_id INTEGER PRIMARY KEY, tantivy_generation INTEGER, active INTEGER
             );
             CREATE TABLE retrieval_publication_pointer(
               id INTEGER PRIMARY KEY, publication_id INTEGER
             );
             CREATE TABLE index_generations(
               generation INTEGER PRIMARY KEY, path TEXT, active INTEGER
             );
             INSERT INTO retrieval_publications VALUES (70, 7, 1);
             INSERT INTO retrieval_publication_pointer VALUES (1, 70);
             INSERT INTO index_generations VALUES (7, '{}', 1);",
            generation.display(),
        ))
        .unwrap();
    drop(connection);

    let frozen = live_model_eval_runtime::freeze_tantivy_snapshot_for_test(&db_path)
        .expect("active index files freeze");
    let files = frozen["generation_files"]
        .as_array()
        .expect("frozen snapshot carries a file manifest");
    assert!(files.len() > 1);
    assert!(files.windows(2).all(|pair| {
        pair[0]["relative_path"].as_str().unwrap() < pair[1]["relative_path"].as_str().unwrap()
    }));
    assert_eq!(
        frozen["generation_files_sha256"].as_str().unwrap().len(),
        64
    );
    std::fs::remove_dir_all(&generation).unwrap();
    std::fs::rename(&replacement, &generation).unwrap();

    let error = live_model_eval_runtime::revalidate_tantivy_snapshot_for_test(&db_path, frozen)
        .expect_err("same-schema index file replacement must invalidate the frozen run");
    assert_eq!(error.to_string(), "eval.frozen_identity_changed");
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(feature = "fastembed-provider", unix))]
#[test]
fn frozen_bm25_snapshot_rejects_symlink_nonregular_and_escaped_generation_entries() {
    let root = std::path::PathBuf::from("/tmp").join(format!(
        "qgh-fc-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let profile = root.join("profile");
    let generation = profile.join("generation-7");
    std::fs::create_dir_all(&generation).unwrap();
    std::fs::write(
        generation.join("meta.json"),
        serde_json::to_vec(&json!({"schema": {"fields": []}})).unwrap(),
    )
    .unwrap();
    let outside_file = root.join("outside-segment");
    std::fs::write(&outside_file, b"outside").unwrap();
    std::os::unix::fs::symlink(&outside_file, generation.join("segment-link")).unwrap();

    let db_path = profile.join("qgh.sqlite3");
    let connection = rusqlite::Connection::open(&db_path).unwrap();
    connection
        .execute_batch(&format!(
            "CREATE TABLE retrieval_publications(
               publication_id INTEGER PRIMARY KEY, tantivy_generation INTEGER, active INTEGER
             );
             CREATE TABLE retrieval_publication_pointer(
               id INTEGER PRIMARY KEY, publication_id INTEGER
             );
             CREATE TABLE index_generations(
               generation INTEGER PRIMARY KEY, path TEXT, active INTEGER
             );
             INSERT INTO retrieval_publications VALUES (70, 7, 1);
             INSERT INTO retrieval_publication_pointer VALUES (1, 70);
             INSERT INTO index_generations VALUES (7, '{}', 1);",
            generation.display(),
        ))
        .unwrap();
    drop(connection);

    assert!(live_model_eval_runtime::freeze_tantivy_snapshot_for_test(&db_path).is_err());
    std::fs::remove_file(generation.join("segment-link")).unwrap();
    let socket_path = generation.join("nonregular.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    assert!(live_model_eval_runtime::freeze_tantivy_snapshot_for_test(&db_path).is_err());
    drop(listener);
    std::fs::remove_file(socket_path).unwrap();

    let escaped = root.join("escaped-generation");
    std::fs::create_dir_all(&escaped).unwrap();
    std::fs::write(
        escaped.join("meta.json"),
        serde_json::to_vec(&json!({"schema": {"fields": []}})).unwrap(),
    )
    .unwrap();
    let connection = rusqlite::Connection::open(&db_path).unwrap();
    connection
        .execute(
            "UPDATE index_generations SET path = ?1 WHERE generation = 7",
            [escaped.to_string_lossy().as_ref()],
        )
        .unwrap();
    drop(connection);
    assert!(live_model_eval_runtime::freeze_tantivy_snapshot_for_test(&db_path).is_err());
    std::fs::remove_dir_all(root).unwrap();
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
fn live_eval_binary_accepts_only_the_canonical_cargo_built_executable() {
    let root = std::env::temp_dir().join(format!(
        "qgh-live-binary-provenance-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let cargo_binary = root.join("cargo-qgh");
    std::fs::write(&cargo_binary, b"cargo-built").unwrap();
    let copied_override = root.join("copied-qgh");
    std::fs::copy(&cargo_binary, &copied_override).unwrap();

    assert_eq!(
        live_model_eval_runtime::resolve_eval_binary_for_test(&cargo_binary, None)
            .expect("Cargo binary is accepted"),
        cargo_binary.canonicalize().unwrap()
    );
    assert!(live_model_eval_runtime::resolve_eval_binary_for_test(
        &cargo_binary,
        Some(&copied_override)
    )
    .is_err());
    assert!(
        live_model_eval_runtime::resolve_eval_binary_for_test(&root.join("missing"), None).is_err()
    );
    assert!(live_model_eval_runtime::resolve_eval_binary_for_test(&root, None).is_err());

    #[cfg(unix)]
    {
        let symlink = root.join("symlink-qgh");
        std::os::unix::fs::symlink(&cargo_binary, &symlink).unwrap();
        assert!(live_model_eval_runtime::resolve_eval_binary_for_test(&symlink, None).is_err());
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(all(feature = "fastembed-provider", unix))]
#[test]
fn canonical_eval_root_rejects_an_existing_symlink() {
    let base = std::env::temp_dir().join(format!(
        "qgh-live-model-root-symlink-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cwd = base.join("repo");
    let outside = base.join("outside");
    std::fs::create_dir_all(cwd.join("target")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, cwd.join("target/qgh-eval")).unwrap();
    assert!(
        live_model_eval_runtime::ensure_target_root_from_cwd_for_test(
            &cwd,
            std::path::Path::new("target/qgh-eval/run"),
        )
        .is_err()
    );
    std::fs::remove_file(cwd.join("target/qgh-eval")).unwrap();
    std::fs::remove_dir_all(base).unwrap();
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
        (live_model_eval_runtime::weighted_score_for_test([0.0, 0.0, 1.0, 0.0, 0.0, 0.0]) - 0.15)
            .abs()
            < 1e-12
    );
    assert!(
        (live_model_eval_runtime::weighted_score_for_test([0.0, 0.0, 0.0, 0.0, 1.0, 1.0]) - 0.05)
            .abs()
            < 1e-12
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn production_hard_filter_contract_excludes_competing_sources() {
    let evidence = live_model_eval_runtime::run_hard_filter_contract_probe(
        std::path::Path::new(env!("CARGO_BIN_EXE_qgh")),
        CORPUS_JSONL,
    )
    .expect("production hard-filter contract probe passes");
    assert_eq!(evidence.active_competing_sources, 7);
    assert_eq!(evidence.bm25_filtered_queries, 4);
    assert_eq!(evidence.hybrid_filtered_queries, 4);
    assert_eq!(evidence.hybrid_ranked_results, 17);
    assert_eq!(evidence.hybrid_results_with_both_branches, 17);
    assert_eq!(evidence.exact_issue_queries, 2);
}

#[cfg(all(feature = "fastembed-provider", not(debug_assertions)))]
#[test]
fn production_release_bm25_filter_and_round_trip_contract() {
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_qgh"));
    let evidence =
        live_model_eval_runtime::run_release_bm25_filter_contract_probe(binary, CORPUS_JSONL)
            .expect("release BM25 filter contract probe passes");
    assert_eq!(evidence.active_competing_sources, 7);
    assert_eq!(evidence.bm25_filtered_queries, 4);
    assert_eq!(evidence.exact_issue_queries, 2);
    println!(
        "QGH_CONTRACT_BINARY_WITNESS={}",
        live_model_eval_runtime::release_binary_witness_for_test(binary)
            .expect("release binary witness")
    );
}

#[test]
fn candidate_reports_capture_post_embed_schema_fingerprints() {
    assert!(RUNTIME_SUPPORT.contains("candidate_database_schema_fingerprint"));
    assert!(RUNTIME_SUPPORT.contains("candidate_tantivy_schema_fingerprint"));
}

#[test]
fn heldout_parse_occurs_after_frozen_config_write() {
    let freeze = RUNTIME_SUPPORT
        .find("fs::write(root.join(\"frozen-config.json\")")
        .unwrap();
    let heldout_parse = RUNTIME_SUPPORT
        .find("parse_jsonl_checked::<QrelRecord>(test_raw)")
        .unwrap();
    assert!(freeze < heldout_parse);
}

#[test]
fn actual_candidate_hybrid_filter_gate_precedes_freeze_and_heldout_open() {
    let candidate_preparation = RUNTIME_SUPPORT
        .find("candidate_states.push(prepare_candidate_dev(")
        .expect("candidate dev preparation exists");
    let freeze = RUNTIME_SUPPORT
        .find("fs::write(root.join(\"frozen-config.json\")")
        .expect("frozen config exists");
    let heldout_parse = RUNTIME_SUPPORT
        .find("parse_jsonl_checked::<QrelRecord>(test_raw)")
        .expect("held-out parse exists");
    assert!(candidate_preparation < freeze && freeze < heldout_parse);
    let candidate_dev = &RUNTIME_SUPPORT[RUNTIME_SUPPORT
        .find("fn try_prepare_candidate_dev(")
        .expect("candidate dev function exists")..];
    let dev_metrics = candidate_dev
        .find("let offline_dev_diagnostics =")
        .expect("candidate dev metrics are frozen");
    let hybrid_filter = candidate_dev
        .find("let hybrid_filter_contract =")
        .expect("candidate hybrid filter contract runs");
    assert!(dev_metrics < hybrid_filter);
    assert!(RUNTIME_SUPPORT.contains("manifest_relative_path"));
    assert!(RUNTIME_SUPPORT.contains("prepared_snapshot_sha256"));
    assert!(RUNTIME_SUPPORT.contains("release_binary_sha256"));
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn candidate_hybrid_filter_evidence_is_complete_and_root_relative() {
    let evidence = live_model_eval_runtime::candidate_hybrid_filter_contract_for_test(
        "models/candidate/manifest.json",
    )
    .expect("complete root-relative evidence");
    assert_eq!(
        evidence["schema_version"],
        "qgh.candidate_hybrid_filter_contract.v2"
    );
    assert_eq!(
        evidence["tokenizer_contract_identity"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert!(evidence["chunker_fingerprint"]
        .as_str()
        .unwrap()
        .starts_with("markdown-token-v2:"));
    assert_eq!(evidence["active_competing_sources"], 7);
    assert_eq!(evidence["embedded_chunks"], 7);
    assert_eq!(evidence["hybrid_filtered_queries"], 4);
    assert_eq!(evidence["hybrid_ranked_results"], 17);
    assert_eq!(evidence["hybrid_results_with_both_branches"], 17);
    assert_eq!(evidence["exact_issue_queries"], 2);
    assert_eq!(
        evidence["manifest_relative_path"],
        "models/candidate/manifest.json"
    );
    assert!(evidence["manifest_hash"].as_str().unwrap().len() == 64);
    assert!(evidence["prepared_snapshot_sha256"].as_str().unwrap().len() == 64);
    assert!(evidence["release_binary_sha256"].as_str().unwrap().len() == 64);
    assert!(
        live_model_eval_runtime::candidate_hybrid_filter_contract_for_test(
            "/absolute/models/candidate/manifest.json"
        )
        .is_err()
    );
    assert!(
        live_model_eval_runtime::candidate_hybrid_filter_contract_for_test(
            "models/../candidate/manifest.json"
        )
        .is_err()
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn runtime_fixture_preflight_validates_raw_provenance_without_opening_heldout_json() {
    let evidence = live_model_eval_runtime::fixture_preflight_for_test(
        CORPUS_JSONL,
        DEV_QRELS_JSONL,
        TEST_QRELS_JSONL,
        PROVENANCE_JSON,
    )
    .expect("committed raw fixture provenance passes");
    assert_eq!(evidence["corpus_source_count"], 154);
    assert_eq!(evidence["dev_query_count"], 40);
    assert_eq!(evidence["heldout_raw_record_count"], 80);

    let invalid_heldout = std::iter::repeat_n("{not-json}\n", 80).collect::<String>();
    let mut raw_only_provenance: serde_json::Value = serde_json::from_str(PROVENANCE_JSON).unwrap();
    raw_only_provenance["qrels_test_sha256"] = json!(digest_hex(&invalid_heldout));
    assert!(live_model_eval_runtime::fixture_preflight_for_test(
        CORPUS_JSONL,
        DEV_QRELS_JSONL,
        &invalid_heldout,
        &serde_json::to_string(&raw_only_provenance).unwrap(),
    )
    .is_ok());

    assert!(live_model_eval_runtime::fixture_preflight_for_test(
        &format!("{CORPUS_JSONL}\n"),
        DEV_QRELS_JSONL,
        TEST_QRELS_JSONL,
        PROVENANCE_JSON,
    )
    .is_err());
    let mut private_provenance: serde_json::Value = serde_json::from_str(PROVENANCE_JSON).unwrap();
    private_provenance["repositories"][0]["visibility"] = json!("private");
    assert!(live_model_eval_runtime::fixture_preflight_for_test(
        CORPUS_JSONL,
        DEV_QRELS_JSONL,
        TEST_QRELS_JSONL,
        &serde_json::to_string(&private_provenance).unwrap(),
    )
    .is_err());
    let mut authenticated_provenance: serde_json::Value =
        serde_json::from_str(PROVENANCE_JSON).unwrap();
    authenticated_provenance["acquisition"]["authentication"] = json!("token");
    assert!(live_model_eval_runtime::fixture_preflight_for_test(
        CORPUS_JSONL,
        DEV_QRELS_JSONL,
        TEST_QRELS_JSONL,
        &serde_json::to_string(&authenticated_provenance).unwrap(),
    )
    .is_err());
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn frozen_heldout_open_validates_classes_gold_adjudication_and_thread_split() {
    live_model_eval_runtime::heldout_fixture_contract_for_test(
        CORPUS_JSONL,
        DEV_QRELS_JSONL,
        TEST_QRELS_JSONL,
        PROVENANCE_JSON,
    )
    .expect("committed held-out contract passes");

    let tamper_first = |mutate: &dyn Fn(&mut serde_json::Value)| {
        let mut records = TEST_QRELS_JSONL
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        mutate(&mut records[0]);
        records
            .into_iter()
            .map(|record| serde_json::to_string(&record).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let bad_class = tamper_first(&|record| record["class"] = json!("negative"));
    assert!(live_model_eval_runtime::heldout_fixture_contract_for_test(
        CORPUS_JSONL,
        DEV_QRELS_JSONL,
        &bad_class,
        PROVENANCE_JSON,
    )
    .is_err());
    let bad_gold = tamper_first(&|record| {
        record["relevant"][0]["source_id"] = json!("qgh://github.com/issue/I_missing")
    });
    assert!(live_model_eval_runtime::heldout_fixture_contract_for_test(
        CORPUS_JSONL,
        DEV_QRELS_JSONL,
        &bad_gold,
        PROVENANCE_JSON,
    )
    .is_err());
    let one_adjudicator = tamper_first(&|record| record["adjudicators"] = json!(["only-one"]));
    assert!(live_model_eval_runtime::heldout_fixture_contract_for_test(
        CORPUS_JSONL,
        DEV_QRELS_JSONL,
        &one_adjudicator,
        PROVENANCE_JSON,
    )
    .is_err());

    let dev_source =
        serde_json::from_str::<serde_json::Value>(DEV_QRELS_JSONL.lines().next().unwrap()).unwrap()
            ["relevant"][0]["source_id"]
            .clone();
    let thread_leak = tamper_first(&|record| {
        record["relevant"] = json!([{
            "source_id": dev_source,
            "grade": 3,
            "rationale": "valid public source forced across the frozen split"
        }]);
        record["filters"] = json!({"repo": "juicyjusung/qgh"});
    });
    assert!(live_model_eval_runtime::heldout_fixture_contract_for_test(
        CORPUS_JSONL,
        DEV_QRELS_JSONL,
        &thread_leak,
        PROVENANCE_JSON,
    )
    .is_err());

    let mut bad_adjudication: serde_json::Value = serde_json::from_str(PROVENANCE_JSON).unwrap();
    bad_adjudication["adjudication"]["method"] = json!("unreviewed");
    assert!(live_model_eval_runtime::heldout_fixture_contract_for_test(
        CORPUS_JSONL,
        DEV_QRELS_JSONL,
        TEST_QRELS_JSONL,
        &serde_json::to_string(&bad_adjudication).unwrap(),
    )
    .is_err());
}

#[test]
fn lexical_profile_is_selected_and_report_bound_before_heldout_then_never_reselected() {
    let dev_ab = RUNTIME_SUPPORT
        .find("run_lexical_profile_dev_ab(")
        .expect("dev A/B exists");
    let freeze = RUNTIME_SUPPORT
        .find("fs::write(root.join(\"frozen-config.json\")")
        .expect("frozen config exists");
    let heldout_parse = RUNTIME_SUPPORT
        .find("parse_jsonl_checked::<QrelRecord>(test_raw)")
        .expect("held-out parse exists");
    assert!(dev_ab < freeze && freeze < heldout_parse);
    assert_eq!(
        RUNTIME_SUPPORT
            .matches("let selection = select_lexical_profile(\n        plan,")
            .count(),
        1
    );
    assert!(RUNTIME_SUPPORT[heldout_parse..].contains("frozen_lexical_profile.selected_profile"));
    for binding in [
        "corpus_sha256",
        "qrels_dev_sha256",
        "active_tantivy_generation",
        "dev_report_sha256",
    ] {
        assert!(RUNTIME_SUPPORT.contains(binding));
    }
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn frozen_run_identity_covers_complete_snapshots_and_is_revalidated_at_phase_boundaries() {
    let root = std::env::temp_dir().join(format!(
        "qgh-live-model-snapshot-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("onnx")).unwrap();
    std::fs::write(root.join("manifest.json"), b"manifest-v1").unwrap();
    std::fs::write(root.join("onnx/model.onnx"), b"model-v1").unwrap();
    let first =
        live_model_eval_runtime::prepared_snapshot_digest_for_test(&root).expect("snapshot hashes");
    assert_eq!(first["file_count"], 2);
    assert_eq!(first["sha256"].as_str().unwrap().len(), 64);
    std::fs::write(root.join("onnx/model.onnx"), b"model-v2").unwrap();
    let second = live_model_eval_runtime::prepared_snapshot_digest_for_test(&root)
        .expect("changed snapshot hashes");
    assert_ne!(first["sha256"], second["sha256"]);
    std::fs::remove_dir_all(root).unwrap();

    for field in [
        "integrated_git_head",
        "worktree_clean",
        "release_binary_sha256",
        "contract_gate_bundle_sha256",
        "candidate_states",
        "manifest_hash",
        "prepared_snapshot_sha256",
        "artifact_set_sha256",
    ] {
        assert!(
            RUNTIME_SUPPORT.contains(field),
            "missing frozen field {field}"
        );
    }
    let freeze = RUNTIME_SUPPORT
        .find("fs::write(root.join(\"frozen-config.json\")")
        .unwrap();
    let heldout_revalidation = RUNTIME_SUPPORT
        .find("frozen_guard.revalidate_before_heldout")
        .unwrap();
    let heldout_parse = RUNTIME_SUPPORT
        .find("parse_jsonl_checked::<QrelRecord>(test_raw)")
        .unwrap();
    assert!(freeze < heldout_revalidation && heldout_revalidation < heldout_parse);
    let resource_revalidation = RUNTIME_SUPPORT
        .find("frozen_guard.revalidate_before_50k")
        .unwrap();
    let backfill = RUNTIME_SUPPORT.find("measure_50k_backfill(").unwrap();
    assert!(resource_revalidation < backfill);
    let post_resource_revalidation = RUNTIME_SUPPORT
        .find("frozen_guard.revalidate_after_50k")
        .unwrap();
    let resource_eligibility = RUNTIME_SUPPORT.find("live_resource_failures(").unwrap();
    assert!(backfill < post_resource_revalidation);
    assert!(post_resource_revalidation < resource_eligibility);
    let final_revalidation = RUNTIME_SUPPORT
        .find("frozen_guard.revalidate_before_final_report")
        .unwrap();
    let final_report = RUNTIME_SUPPORT.find("let mut report = FullReport").unwrap();
    assert!(final_revalidation < final_report);
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn live_eval_file_hashing_uses_bounded_streaming_reads() {
    struct BoundedReader {
        remaining: usize,
        maximum_request: usize,
    }

    impl std::io::Read for BoundedReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if buffer.len() > self.maximum_request {
                return Err(std::io::Error::other("unbounded read request"));
            }
            let read = self.remaining.min(buffer.len());
            buffer[..read].fill(0x5a);
            self.remaining -= read;
            Ok(read)
        }
    }

    let hash = live_model_eval_runtime::stream_sha256_for_test(BoundedReader {
        remaining: 3 * 1024 * 1024 + 17,
        maximum_request: 1024 * 1024,
    })
    .expect("hashing stays within the bounded reader contract");
    assert_eq!(hash.len(), 64);
}

#[test]
fn runtime_records_schema_fingerprints_and_derives_redaction_state() {
    assert!(RUNTIME_SUPPORT.contains("database_schema_fingerprint"));
    assert!(RUNTIME_SUPPORT.contains("tantivy_schema_fingerprint"));
    assert!(!RUNTIME_SUPPORT.contains("raw_query_or_body_logged: false"));
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn contract_gate_test_count_is_derived_from_cargo_output() {
    let stdout = br#"
running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 31 filtered out

running 1 test
test store::tests::purge_retry_finishes_idempotently_and_clears_pending ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 206 filtered out
"#;
    let stderr = b"test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
    assert_eq!(
        live_model_eval_runtime::observed_contract_test_count_for_test(stdout, stderr),
        1
    );
    assert_eq!(
        live_model_eval_runtime::observed_contract_test_count_for_test(
            b"test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out\n",
            b"",
        ),
        0
    );
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn canonical_gate_bundle_is_strict_and_bound_to_git_binary_and_file_hash() {
    let root = std::env::temp_dir().join(format!(
        "qgh-live-model-gate-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let git_sha = "a".repeat(40);
    let binary_sha = "b".repeat(64);
    let bundle =
        live_model_eval_runtime::contract_gate_bundle_json_for_test(&root, &git_sha, &binary_sha)
            .expect("gate result artifacts");
    assert_eq!(
        bundle["schema_version"],
        "qgh.live_model_eval_gate_bundle.v3"
    );
    let names = bundle["gates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|gate| gate["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "edit_reconciliation",
            "delete_and_stale_exclusion",
            "purge_pending_retry",
            "parent_context_invalidation",
            "concurrent_publication_snapshot",
            "bm25_search_quality",
        ]
    );
    let commands = bundle["gates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|gate| gate["command"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        commands,
        [
            "cargo test --release --all-features --test issue_body_tracer sync_issue_refreshes_target_issue_and_reconciles_comment_diff -- --exact",
            "cargo test --release --all-features --test issue_body_tracer full_reconciliation_tombstones_deleted_comments_and_updates_status -- --exact",
            "cargo test --release --all-features --test issue_body_tracer pending_purge_is_retried_by_next_sync_without_touching_user_backup -- --exact",
            "cargo test --release --all-features --test live_model_eval parent_issue_title_change_invalidates_comment_context_hash_in_release_contract -- --exact",
            "cargo test --release --all-features --test issue_body_tracer concurrent_cli_sync_and_mcp_reads_keep_index_queryable -- --exact",
            "cargo test --release --all-features --test live_model_eval production_release_bm25_filter_and_round_trip_contract -- --exact --nocapture",
        ]
    );
    let path = root.join("contract-gate-bundle.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();
    let frozen_hash = live_model_eval_runtime::verify_contract_gate_bundle_path_for_test(
        &root,
        &git_sha,
        &binary_sha,
        None,
    )
    .expect("valid canonical gate bundle");
    assert_eq!(frozen_hash.len(), 64);

    let first_result_path = root.join("contract-gates/edit_reconciliation.json");
    let mut empty_result: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&first_result_path).unwrap()).unwrap();
    assert_eq!(
        empty_result["schema_version"],
        "qgh.live_model_eval_gate_result.v3"
    );
    assert_eq!(empty_result["observed_test_count"], 1);
    assert_eq!(
        empty_result["command_output_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(empty_result["cargo_profile"], "release");
    assert_eq!(empty_result["candidate_binary_root"], "repository");
    assert_eq!(empty_result["candidate_binary_path"], "target/release/qgh");
    assert_eq!(empty_result["candidate_binary_sha256"], binary_sha);
    assert_eq!(empty_result["candidate_binary_exercised"], false);
    assert!(
        live_model_eval_runtime::release_binary_identity_is_confined_for_test(
            empty_result["candidate_binary_path"].as_str().unwrap()
        )
    );
    assert!(
        !live_model_eval_runtime::release_binary_identity_is_confined_for_test(
            "forged/release/qgh"
        )
    );
    empty_result["observed_test_count"] = json!(0);
    let empty_result_bytes = serde_json::to_vec_pretty(&empty_result).unwrap();
    std::fs::write(&first_result_path, &empty_result_bytes).unwrap();
    let mut empty_bundle = bundle.clone();
    empty_bundle["gates"][0]["result_sha256"] =
        json!(format!("{:x}", Sha256::digest(&empty_result_bytes)));
    std::fs::write(&path, serde_json::to_vec_pretty(&empty_bundle).unwrap()).unwrap();
    assert!(
        live_model_eval_runtime::verify_contract_gate_bundle_path_for_test(
            &root,
            &git_sha,
            &binary_sha,
            None,
        )
        .is_err()
    );
    let bundle =
        live_model_eval_runtime::contract_gate_bundle_json_for_test(&root, &git_sha, &binary_sha)
            .expect("restore non-empty gate result artifacts");
    std::fs::write(&path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();

    let hard_filter_result_path = root.join("contract-gates/bm25_search_quality.json");
    let mut debug_mismatch: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&hard_filter_result_path).unwrap()).unwrap();
    assert_eq!(debug_mismatch["candidate_binary_exercised"], true);
    debug_mismatch["cargo_profile"] = json!("debug");
    let debug_mismatch_bytes = serde_json::to_vec_pretty(&debug_mismatch).unwrap();
    std::fs::write(&hard_filter_result_path, &debug_mismatch_bytes).unwrap();
    let mut debug_bundle = bundle.clone();
    debug_bundle["gates"][5]["result_sha256"] =
        json!(format!("{:x}", Sha256::digest(&debug_mismatch_bytes)));
    std::fs::write(&path, serde_json::to_vec_pretty(&debug_bundle).unwrap()).unwrap();
    assert!(
        live_model_eval_runtime::verify_contract_gate_bundle_path_for_test(
            &root,
            &git_sha,
            &binary_sha,
            None,
        )
        .is_err()
    );
    let bundle =
        live_model_eval_runtime::contract_gate_bundle_json_for_test(&root, &git_sha, &binary_sha)
            .expect("restore release-profile gate result artifacts");
    std::fs::write(&path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();

    let nested_result = root.join("contract-gates/nested/edit_reconciliation.json");
    std::fs::create_dir_all(nested_result.parent().unwrap()).unwrap();
    std::fs::copy(
        root.join("contract-gates/edit_reconciliation.json"),
        &nested_result,
    )
    .unwrap();
    let mut nested_bundle = bundle.clone();
    nested_bundle["gates"][0]["result_artifact"] =
        json!("contract-gates/nested/edit_reconciliation.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&nested_bundle).unwrap()).unwrap();
    assert!(
        live_model_eval_runtime::verify_contract_gate_bundle_path_for_test(
            &root,
            &git_sha,
            &binary_sha,
            None,
        )
        .is_err()
    );
    std::fs::write(&path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();

    std::fs::write(root.join("contract-gates/edit_reconciliation.json"), b"{}").unwrap();
    assert!(
        live_model_eval_runtime::verify_contract_gate_bundle_path_for_test(
            &root,
            &git_sha,
            &binary_sha,
            Some(&frozen_hash),
        )
        .is_err()
    );
    live_model_eval_runtime::contract_gate_bundle_json_for_test(&root, &git_sha, &binary_sha)
        .expect("restore gate result artifacts");

    let mut changed = bundle.clone();
    changed["gates"][0]["result_sha256"] = json!("d".repeat(64));
    std::fs::write(&path, serde_json::to_vec_pretty(&changed).unwrap()).unwrap();
    assert!(
        live_model_eval_runtime::verify_contract_gate_bundle_path_for_test(
            &root,
            &git_sha,
            &binary_sha,
            Some(&frozen_hash),
        )
        .is_err()
    );

    let mut unknown = bundle;
    unknown["unexpected"] = json!(true);
    std::fs::write(&path, serde_json::to_vec_pretty(&unknown).unwrap()).unwrap();
    assert!(
        live_model_eval_runtime::verify_contract_gate_bundle_path_for_test(
            &root,
            &git_sha,
            &binary_sha,
            None,
        )
        .is_err()
    );
    assert!(!RUNTIME_SUPPORT.contains("QGH_LIVE_MODEL_EVAL_STALE_GATE_STATUS"));
    assert!(!RUNTIME_SUPPORT.contains("QGH_LIVE_MODEL_EVAL_FILTER_GATE_STATUS"));
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn redaction_audit_scans_canonical_gate_artifacts_and_partial_canary_fragments() {
    let root = std::env::temp_dir().join(format!(
        "qgh-live-model-redaction-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("nested")).unwrap();
    let canary = "private-canary-fragment";
    std::fs::write(root.join("contract-gate-bundle.json"), canary).unwrap();
    std::fs::write(root.join("nested/eval-write.partial"), canary).unwrap();
    std::fs::write(root.join("ignored.bin"), canary).unwrap();
    let evidence = live_model_eval_runtime::redaction_file_scan_for_test(&root, canary)
        .expect("redaction scan completes");
    assert_eq!(evidence["artifact_files_checked"], 2);
    assert_eq!(
        evidence["violation_artifacts"],
        json!(["contract-gate-bundle.json", "nested/eval-write.partial"])
    );
    assert_eq!(evidence["passed"], false);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn redaction_audit_excludes_model_payloads_but_scans_model_evidence() {
    let root = std::env::temp_dir().join(format!(
        "qgh-live-model-redaction-inputs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let model_root = root.join("models/candidate");
    std::fs::create_dir_all(&model_root).unwrap();
    let canary = "private-canary-fragment";
    std::fs::write(model_root.join("tokenizer.json"), canary).unwrap();
    std::fs::write(model_root.join("config.json"), canary).unwrap();
    std::fs::write(model_root.join("manifest.json"), canary).unwrap();
    std::fs::write(root.join("models/preparation-provenance.json"), "{}\n").unwrap();
    let cached_model_root =
        root.join("hard-filter-contract-debug/cache/qgh/hf/models--candidate/snapshots/revision");
    std::fs::create_dir_all(&cached_model_root).unwrap();
    std::fs::write(cached_model_root.join("tokenizer.json"), canary).unwrap();
    let prepared_model_root =
        root.join("hard-filter-contract-debug/cache/qgh/prepared-models/snapshots/digest");
    std::fs::create_dir_all(&prepared_model_root).unwrap();
    std::fs::write(prepared_model_root.join("tokenizer.json"), canary).unwrap();
    std::fs::write(
        root.join("hard-filter-contract-debug/candidate-events.jsonl"),
        canary,
    )
    .unwrap();

    let evidence = live_model_eval_runtime::redaction_file_scan_for_test(&root, canary)
        .expect("redaction scan completes");
    assert_eq!(evidence["artifact_files_checked"], 3);
    assert_eq!(
        evidence["violation_artifacts"],
        json!([
            "hard-filter-contract-debug/candidate-events.jsonl",
            "models/candidate/manifest.json"
        ])
    );
    assert_eq!(evidence["passed"], false);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn redaction_audit_detects_json_escaped_sensitive_values() {
    let root = std::env::temp_dir().join(format!(
        "qgh-live-model-redaction-escaped-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let sensitive = "private line\n\"quoted\"\\path";
    std::fs::write(
        root.join("escaped-report.json"),
        serde_json::to_vec(&json!({"message": sensitive})).unwrap(),
    )
    .unwrap();

    let evidence = live_model_eval_runtime::redaction_file_scan_for_test(&root, sensitive)
        .expect("redaction scan completes");
    assert_eq!(
        evidence["violation_artifacts"],
        json!(["escaped-report.json"])
    );
    assert_eq!(evidence["passed"], false);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn path_redaction_scans_artifacts_stdout_and_stderr() {
    let root = std::env::temp_dir().join(format!(
        "qgh-live-model-path-redaction-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let marker = root.join("unique-worktree-marker");
    let marker = marker.to_string_lossy().to_string();
    std::fs::write(
        root.join("path-report.json"),
        serde_json::to_vec(&json!({"nested": {"tantivy": &marker}})).unwrap(),
    )
    .unwrap();
    let stdout = serde_json::to_vec(&json!({"model": &marker})).unwrap();
    let stderr = format!("runtime path={marker}").into_bytes();

    let evidence = live_model_eval_runtime::path_redaction_scan_for_test(
        &root,
        &[marker.as_str()],
        &[stdout],
        &[stderr],
    )
    .expect("path redaction scan completes");
    assert_eq!(evidence["stdout_streams_checked"], 1);
    assert_eq!(evidence["stderr_streams_checked"], 1);
    assert_eq!(evidence["path_markers_checked"], 1);
    assert_eq!(
        evidence["violation_artifacts"],
        json!(["stdout-stream-0", "stderr-stream-0", "path-report.json"])
    );
    assert_eq!(evidence["path_privacy_passed"], false);
    assert_eq!(evidence["passed"], false);
    assert_eq!(
        live_model_eval_runtime::final_report_artifact_for_test(),
        "live-model-eval-report.json"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(feature = "fastembed-provider")]
#[test]
fn path_redaction_allows_only_contracted_repo_policy_json_fields() {
    let root = std::env::temp_dir().join(format!(
        "qgh-live-model-path-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let marker = root.join("contracted-repo-policy-path");
    let marker = marker.to_string_lossy().to_string();
    let cli_meta = serde_json::to_vec(&json!({
        "meta": {"repo_policy_path": &marker}
    }))
    .unwrap();
    let cli_status = serde_json::to_vec(&json!({
        "data": {"resolution": {"repo_policy_path": &marker}}
    }))
    .unwrap();
    let mcp_jsonl = format!(
        "{}\n{}\n",
        json!({
            "result": {"structuredContent": {
                "meta": {"repo_policy_path": &marker}
            }}
        }),
        json!({
            "result": {"structuredContent": {
                "data": {"resolution": {"repo_policy_path": &marker}}
            }}
        })
    )
    .into_bytes();
    let unexpected_stdout = serde_json::to_vec(&json!({
        "meta": {
            "repo_policy_path": &marker,
            "unexpected_path": &marker
        }
    }))
    .unwrap();
    let stderr = serde_json::to_vec(&json!({
        "meta": {"repo_policy_path": &marker}
    }))
    .unwrap();
    std::fs::write(
        root.join("path-report.json"),
        serde_json::to_vec(&json!({
            "meta": {"repo_policy_path": &marker}
        }))
        .unwrap(),
    )
    .unwrap();

    let evidence = live_model_eval_runtime::path_redaction_scan_for_test(
        &root,
        &[marker.as_str()],
        &[cli_meta, cli_status, mcp_jsonl, unexpected_stdout],
        &[stderr],
    )
    .expect("path redaction scan completes");
    assert_eq!(evidence["stdout_streams_checked"], 4);
    assert_eq!(evidence["stderr_streams_checked"], 1);
    assert_eq!(
        evidence["violation_artifacts"],
        json!(["stdout-stream-3", "stderr-stream-0", "path-report.json"])
    );
    assert_eq!(evidence["path_privacy_passed"], false);
    std::fs::remove_dir_all(root).unwrap();
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
#[ignore = "loads machine-only fresh blind qrels and local ONNX models"]
fn fresh_blind_model_runtime_evaluation() {
    assert!(
        fresh_blind_eval_opt_in(std::env::var("QGH_FRESH_BLIND_MODEL_EVAL").ok().as_deref()),
        "set QGH_FRESH_BLIND_MODEL_EVAL=1 to run the fresh blind evaluation"
    );
    let fixture_root = std::env::var_os("QGH_FRESH_BLIND_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .expect("set QGH_FRESH_BLIND_FIXTURE_ROOT");
    let eval_root = std::env::var_os("QGH_LIVE_MODEL_EVAL_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target/qgh-eval/fresh-blind-run"));
    let read = |name: &str| {
        std::fs::read_to_string(fixture_root.join(name))
            .unwrap_or_else(|_| panic!("fresh blind fixture {name} is readable"))
    };
    live_model_eval_runtime::run(
        &eval_root,
        &read("corpus.jsonl"),
        &read("qrels-dev.jsonl"),
        &read("qrels-test.jsonl"),
        &read("provenance.json"),
    )
    .expect("fresh blind model evaluation completes");
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

fn fresh_blind_eval_opt_in(value: Option<&str>) -> bool {
    value == Some("1")
}
