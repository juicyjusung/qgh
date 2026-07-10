use super::{
    digest_hex, metrics_for, parse_jsonl, redacted_query_event, CorpusRecord, FixtureProvenance,
    QrelRecord, QueryClass,
};
use percent_encoding::percent_decode_str;
use qgh::chunking::{
    chunk_markdown_with_config, ChunkerConfig, CHUNKER_FINGERPRINT, CHUNKER_VERSION,
};
use qgh::embedding::{
    FastembedProviderOptions, FastembedTokenizer, ModelManifestV1, PoolingKind, PreparedModelStore,
    QuantizationKind,
};
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering as AtomicOrdering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::Instant;

const TOP_K: usize = 20;
const WARMUP_RUNS: usize = 1;
const MEASURED_RUNS: usize = 3;
const COLD_PROCESS_RUNS: usize = 5;
const RRF_K: usize = 60;
const CANDIDATE_WINDOW: usize = TOP_K * 4;
const DEV_DIAGNOSTIC_RRF_K: [usize; 3] = [20, 60, 100];
const DEV_DIAGNOSTIC_WINDOWS: [usize; 3] = [40, 80, 100];
const REQUIRED_BATCH_SIZE: usize = 8;
const EFFECTIVE_BATCH_SIZE: usize = 16;
const REQUIRED_INTRA_OP_THREADS: usize = 4;
const DRAGONKUE_MODEL_ID: &str = "dragonkue/snowflake-arctic-embed-l-v2.0-ko";
const DRAGONKUE_REVISION: &str = "55ec6e9358a56d56af759bc8372e970caf8c305f";

type DynError = Box<dyn Error>;

#[derive(Debug, Serialize)]
struct HostRecord {
    os: String,
    os_version: String,
    architecture: String,
    cpu: String,
    hardware_model_identifier: String,
    total_cores: String,
    system_profiler_memory: String,
    ram_bytes: u64,
    rustc: String,
    cargo: String,
    fastembed: &'static str,
    ort: &'static str,
    power_source: String,
    ac_power_mode: Option<u8>,
    reference_protocol_match: bool,
    binary: String,
    binary_sha256: String,
    git_sha: String,
}

#[derive(Debug, Clone, Serialize)]
struct ClassMetrics {
    query_count: usize,
    ndcg_at_10: f64,
    mrr_at_10: f64,
    recall_at_5: f64,
    recall_at_10: f64,
    recall_at_20: f64,
    negative_top_result_rate: Option<f64>,
}

#[derive(Debug, Serialize)]
struct RetrievalMetrics {
    query_count: usize,
    per_class: BTreeMap<QueryClass, ClassMetrics>,
    weighted_ndcg_at_10: f64,
    exact_top_1: f64,
    hard_filter_violations: usize,
    get_round_trip: f64,
    stale_leakage_live_fixture: Option<usize>,
    duplicate_crowding_queries: usize,
    hybrid_path_queries: usize,
    quality_gate_failures: Vec<String>,
}

#[derive(Debug, Serialize)]
struct OfflineFusionDiagnostic {
    scope: &'static str,
    rrf_k: usize,
    candidate_window: usize,
    complete: bool,
    incomplete_reason: Option<&'static str>,
    minimum_observed_lexical_candidates: usize,
    minimum_observed_vector_candidates: usize,
    weighted_ndcg_at_10: Option<f64>,
    exact_top_1: Option<f64>,
}

#[derive(Debug, Serialize)]
struct ResourceEvidence {
    cold_process_samples_ms: Vec<f64>,
    cold_start_p95_ms: f64,
    warm_query_sample_count: usize,
    warm_query_p50_ms: f64,
    warm_query_p95_ms: f64,
    warm_path_includes_manifest_artifact_rehash: bool,
    isolated_peak_rss_bytes: u64,
    complete_model_snapshot_bytes: u64,
    quality_corpus_chunk_count: usize,
    quality_corpus_embed_and_write_seconds: f64,
    quality_corpus_db_growth_bytes_per_chunk: f64,
    measured_50k_chunk_count: usize,
    measured_chunk_tokens: usize,
    measured_50k_embed_and_write_seconds: f64,
    measured_50k_chunks_per_second: f64,
    measured_50k_db_growth_bytes_per_chunk: f64,
    required_batch_size: usize,
    effective_batch_size: usize,
    required_intra_op_threads: usize,
    effective_intra_op_threads: Option<usize>,
    effective_ort_inter_op: String,
    effective_ort_execution_mode: String,
    fastembed_version: String,
    protocol_unverified: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Blocker {
    code: String,
    message: String,
    details: Value,
    evidence: Value,
}

#[derive(Debug, Serialize)]
struct ExternalContractGate {
    name: &'static str,
    command: &'static str,
    status: String,
    result_sha256: Option<String>,
}

#[derive(Debug, Serialize)]
struct CandidateReport {
    candidate: String,
    model_id: String,
    resolved_revision: String,
    runtime: String,
    status: String,
    manifest_hash: Option<String>,
    dev_metrics: Option<RetrievalMetrics>,
    held_out_metrics: Option<RetrievalMetrics>,
    offline_dev_diagnostics: Vec<OfflineFusionDiagnostic>,
    resources: Option<ResourceEvidence>,
    light_gate_failures: Vec<String>,
    quality_resource_gate_failures: Vec<String>,
    blocker: Option<Blocker>,
    synthetic_substitution: bool,
}

#[derive(Debug, Serialize)]
struct FrozenConfig {
    schema_version: &'static str,
    corpus_sha256: String,
    qrels_dev_sha256: String,
    qrels_test_sha256: String,
    chunker_version: &'static str,
    chunker_fingerprint: &'static str,
    context_profile: &'static str,
    fusion: &'static str,
    rrf_k: usize,
    candidate_window: usize,
    lexical_profile: &'static str,
    warmup_runs: usize,
    measured_runs: usize,
    cold_process_runs: usize,
    required_50k_chunks: usize,
    required_chunk_tokens: usize,
    required_batch_size: usize,
    required_intra_op_threads: usize,
    evaluation_state: &'static str,
    context_contract_status: &'static str,
}

#[derive(Debug, Serialize)]
struct FullReport {
    schema_version: &'static str,
    run_finished_at: String,
    corpus_snapshot_at: String,
    host: HostRecord,
    frozen_config_hash: String,
    bm25_dev: RetrievalMetrics,
    bm25: RetrievalMetrics,
    candidates: Vec<CandidateReport>,
    selected_light_candidate: Option<String>,
    selected_quality_candidate: Option<String>,
    raw_query_or_body_logged: bool,
    promotion_eligible: bool,
    required_integrated_rerun: &'static str,
    host_protocol_failures: Vec<String>,
    stale_contract_gate: ExternalContractGate,
}

#[derive(Debug, Serialize)]
struct SmokeReport {
    schema_version: &'static str,
    corpus_sha256: String,
    corpus_source_count: usize,
    query_id: String,
    query_sha256: String,
    ranked_source_count: usize,
    get_round_trip: f64,
    raw_query_or_body_logged: bool,
}

#[derive(Default)]
struct ClassAccumulator {
    count: usize,
    ndcg_at_10: f64,
    mrr_at_10: f64,
    recall_at_5: f64,
    recall_at_10: f64,
    recall_at_20: f64,
    nonempty: usize,
}

struct QueryEvidence {
    rankings: BTreeMap<String, Vec<String>>,
    branch_observations: BTreeMap<String, Vec<BranchObservation>>,
    get_total: usize,
    get_success: usize,
    stale_failures: usize,
    hybrid_path_queries: usize,
}

#[derive(Debug, Clone)]
struct BranchObservation {
    source_id: String,
    lexical_score: Option<f64>,
    vector_distance: Option<f64>,
}

#[derive(Debug)]
struct FusionAccumulator {
    source_id: String,
    lexical_rank: Option<usize>,
    vector_rank: Option<usize>,
}

fn fuse_branch_observations(
    observations: &[BranchObservation],
    rrf_k: usize,
    candidate_window: usize,
) -> Vec<String> {
    let mut lexical = observations
        .iter()
        .filter_map(|hit| {
            hit.lexical_score
                .map(|score| (hit.source_id.clone(), score))
        })
        .collect::<Vec<_>>();
    lexical.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    let mut vector = observations
        .iter()
        .filter_map(|hit| {
            hit.vector_distance
                .map(|distance| (hit.source_id.clone(), distance))
        })
        .collect::<Vec<_>>();
    vector.sort_by(|left, right| {
        left.1
            .partial_cmp(&right.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    let mut fused = BTreeMap::<String, FusionAccumulator>::new();
    for (index, (source_id, _)) in lexical.into_iter().take(candidate_window).enumerate() {
        fused
            .entry(source_id.clone())
            .or_insert(FusionAccumulator {
                source_id,
                lexical_rank: None,
                vector_rank: None,
            })
            .lexical_rank = Some(index + 1);
    }
    for (index, (source_id, _)) in vector.into_iter().take(candidate_window).enumerate() {
        fused
            .entry(source_id.clone())
            .or_insert(FusionAccumulator {
                source_id,
                lexical_rank: None,
                vector_rank: None,
            })
            .vector_rank = Some(index + 1);
    }
    let component = |rank: Option<usize>| rank.map_or(0.0, |rank| 1.0 / (rrf_k + rank) as f64);
    let mut fused = fused.into_values().collect::<Vec<_>>();
    fused.sort_by(|left, right| {
        let left_score = component(left.lexical_rank) + component(left.vector_rank);
        let right_score = component(right.lexical_rank) + component(right.vector_rank);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                left.lexical_rank
                    .into_iter()
                    .chain(left.vector_rank)
                    .min()
                    .cmp(
                        &right
                            .lexical_rank
                            .into_iter()
                            .chain(right.vector_rank)
                            .min(),
                    )
            })
            .then_with(|| left.source_id.cmp(&right.source_id))
    });
    fused.into_iter().map(|hit| hit.source_id).collect()
}

pub(super) fn fuse_for_test(
    observations: &[(&str, Option<f64>, Option<f64>)],
    rrf_k: usize,
    candidate_window: usize,
) -> Vec<String> {
    fuse_branch_observations(
        &observations
            .iter()
            .map(
                |(source_id, lexical_score, vector_distance)| BranchObservation {
                    source_id: (*source_id).to_string(),
                    lexical_score: *lexical_score,
                    vector_distance: *vector_distance,
                },
            )
            .collect::<Vec<_>>(),
        rrf_k,
        candidate_window,
    )
}

struct WarmEvidence {
    held_out: QueryEvidence,
    latencies_ms: Vec<f64>,
    peak_rss_bytes: u64,
}

struct PreparedCandidate {
    candidate: String,
    model_id: String,
    revision: String,
    manifest_hash: String,
    fixture: CliFixture,
    manifest_path: PathBuf,
    snapshot_bytes: u64,
    chunk_count: usize,
    embed_seconds: f64,
    db_growth_bytes_per_chunk: f64,
    cold_samples_ms: Vec<f64>,
    isolated_peak_rss: u64,
    dev_metrics: RetrievalMetrics,
    offline_dev_diagnostics: Vec<OfflineFusionDiagnostic>,
}

pub(super) fn run(
    root: &Path,
    corpus_raw: &str,
    dev_raw: &str,
    test_raw: &str,
    provenance_raw: &str,
) -> Result<(), DynError> {
    ensure_target_root(root)?;
    fs::create_dir_all(root)?;
    for stale in [
        "bm25-live",
        "arctic-embed-l-v2.0-live",
        "gte-modernbert-base-live",
    ] {
        remove_dir_if_exists(&root.join(stale))?;
    }
    let corpus = parse_jsonl::<CorpusRecord>(corpus_raw);
    let dev = parse_jsonl::<QrelRecord>(dev_raw);
    let held_out = parse_jsonl::<QrelRecord>(test_raw);
    let provenance: FixtureProvenance = serde_json::from_str(provenance_raw)?;
    let binary = eval_binary()?;
    let host = host_record(&binary);
    let host_protocol_failures = host_protocol_failures(&host);
    let stale_contract_gate = stale_contract_gate();
    let stale_contract_verified = stale_contract_gate.status == "passed"
        && stale_contract_gate
            .result_sha256
            .as_deref()
            .is_some_and(is_sha256);
    let server = PublicSnapshotServer::start(&corpus)?;
    eprintln!("live-eval phase=bm25-dev-real-store status=running");
    let bm25_fixture = CliFixture::new(
        root.join("bm25-live"),
        binary.clone(),
        server.base_url.clone(),
    )?;
    bm25_fixture.write_config(None)?;
    bm25_fixture.sync()?;
    let bm25_dev_evidence = run_single_pass(&bm25_fixture, &dev)?;
    let bm25_dev = evaluate_rankings(
        &corpus,
        &dev,
        &bm25_dev_evidence,
        &root.join("bm25-live/dev-events.jsonl"),
    )?;

    let mut candidate_states = Vec::new();
    for (candidate, model_id, revision, manifest) in [
        (
            "arctic-embed-l-v2.0",
            "Snowflake/snowflake-arctic-embed-l-v2.0",
            "ac6544c8a46e00af67e330e85a9028c66b8cfd9a",
            root.join("models/arctic-embed-l-v2.0/manifest.json"),
        ),
        (
            "gte-modernbert-base",
            "Alibaba-NLP/gte-modernbert-base",
            "e7f32e3c00f91d699e8c43b53106206bcc72bb22",
            root.join("models/gte-modernbert-base/manifest.json"),
        ),
    ] {
        candidate_states.push(prepare_candidate_dev(
            root, &server, &binary, candidate, model_id, revision, &manifest, &corpus, &dev,
        ));
    }

    // Production exposes neither k nor candidate-window knobs.  The only
    // deployable frozen values are the actual source constants: k=60 and
    // TOP_K(20) * overfetch(4) = 80.  The k/window grid above is diagnostic.
    let frozen = FrozenConfig {
        schema_version: "qgh.live_model_eval_config.v1",
        corpus_sha256: digest_hex(corpus_raw),
        qrels_dev_sha256: digest_hex(dev_raw),
        qrels_test_sha256: digest_hex(test_raw),
        chunker_version: CHUNKER_VERSION,
        chunker_fingerprint: CHUNKER_FINGERPRINT,
        context_profile: "required_generation=qgh.context.v1; current_manifest=qgh.context.none.v1",
        fusion: "production_equal_rrf",
        rrf_k: RRF_K,
        candidate_window: CANDIDATE_WINDOW,
        lexical_profile: "production_v1",
        warmup_runs: WARMUP_RUNS,
        measured_runs: MEASURED_RUNS,
        cold_process_runs: COLD_PROCESS_RUNS,
        required_50k_chunks: 50_000,
        required_chunk_tokens: 900,
        required_batch_size: REQUIRED_BATCH_SIZE,
        required_intra_op_threads: REQUIRED_INTRA_OP_THREADS,
        evaluation_state: "pre_integration_harness_validation_only",
        context_contract_status: "blocked_manifest_context_none_vs_generation_context_v1",
    };
    let frozen_bytes = serde_json::to_vec_pretty(&frozen)?;
    let frozen_config_hash = digest_hex(std::str::from_utf8(&frozen_bytes)?);
    fs::write(root.join("frozen-config.json"), with_newline(frozen_bytes))?;

    eprintln!("live-eval phase=heldout-open-once status=running");
    let bm25_evidence = run_single_pass(&bm25_fixture, &held_out)?;
    let bm25 = evaluate_rankings(
        &corpus,
        &held_out,
        &bm25_evidence,
        &root.join("bm25-live/heldout-events.jsonl"),
    )?;
    write_pretty(root.join("bm25-live/dev-report.json"), &bm25_dev)?;
    write_pretty(root.join("bm25-live/heldout-report.json"), &bm25)?;

    let mut candidates = vec![dragonkue_blocker(root)];
    for state in candidate_states {
        candidates.push(match state {
            Ok(prepared) => finish_candidate(root, &server, &binary, prepared, &corpus, &held_out),
            Err(report) => *report,
        });
    }
    for candidate in &mut candidates {
        candidate
            .light_gate_failures
            .extend(host_protocol_failures.iter().cloned());
        candidate
            .quality_resource_gate_failures
            .extend(host_protocol_failures.iter().cloned());
        if !stale_contract_verified {
            candidate
                .light_gate_failures
                .push("stale_contract_external_evidence_required".to_string());
            candidate
                .quality_resource_gate_failures
                .push("stale_contract_external_evidence_required".to_string());
        }
    }

    let selected_light_candidate = select_candidate(&candidates, true);
    let selected_quality_candidate = select_candidate(&candidates, false);
    let report = FullReport {
        schema_version: "qgh.live_model_eval_report.v1",
        run_finished_at: command_output("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]),
        corpus_snapshot_at: provenance.snapshot_at,
        host,
        frozen_config_hash,
        bm25_dev,
        bm25,
        candidates,
        selected_light_candidate,
        selected_quality_candidate,
        raw_query_or_body_logged: false,
        promotion_eligible: false,
        required_integrated_rerun:
            "rerun all model/profile and 50k gates after Lane D+A+B context-v1 integration",
        host_protocol_failures,
        stale_contract_gate,
    };
    write_pretty(root.join("live-model-eval-report.json"), &report)?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "artifact": root.join("live-model-eval-report.json"),
            "candidate_statuses": report.candidates.iter().map(|candidate| json!({
                "candidate": candidate.candidate,
                "status": candidate.status,
            })).collect::<Vec<_>>(),
            "selected_light_candidate": report.selected_light_candidate,
            "selected_quality_candidate": report.selected_quality_candidate,
        }))?
    );
    Ok(())
}

pub(super) fn run_smoke(root: &Path, corpus_raw: &str, dev_raw: &str) -> Result<(), DynError> {
    ensure_target_root(root)?;
    fs::create_dir_all(root)?;
    let corpus = parse_jsonl::<CorpusRecord>(corpus_raw);
    let dev = parse_jsonl::<QrelRecord>(dev_raw);
    let qrel = dev
        .iter()
        .find(|qrel| qrel.query_class == QueryClass::ExactIdentifier)
        .ok_or("exact smoke qrel missing")?;
    let binary = eval_binary()?;
    let server = PublicSnapshotServer::start(&corpus)?;
    let fixture = CliFixture::new(
        root.join("qgh-only-runtime-smoke"),
        binary,
        server.base_url.clone(),
    )?;
    fixture.write_config(None)?;
    fixture.sync()?;
    let evidence = run_single_pass(&fixture, std::slice::from_ref(qrel))?;
    let ranked_source_count = evidence.rankings.get(&qrel.query_id).map_or(0, Vec::len);
    if ranked_source_count == 0
        || evidence.get_total == 0
        || evidence.get_success != evidence.get_total
    {
        return Err("qgh-only runtime smoke did not query -> get round-trip".into());
    }
    let report = SmokeReport {
        schema_version: "qgh.live_model_eval_smoke.v1",
        corpus_sha256: digest_hex(corpus_raw),
        corpus_source_count: corpus.len(),
        query_id: qrel.query_id.clone(),
        query_sha256: digest_hex(&qrel.query),
        ranked_source_count,
        get_round_trip: evidence.get_success as f64 / evidence.get_total as f64,
        raw_query_or_body_logged: false,
    };
    write_pretty(root.join("qgh-only-runtime-smoke.json"), &report)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare_candidate_dev(
    root: &Path,
    server: &PublicSnapshotServer,
    binary: &Path,
    candidate: &str,
    model_id: &str,
    revision: &str,
    manifest_path: &Path,
    corpus: &[CorpusRecord],
    dev: &[QrelRecord],
) -> Result<PreparedCandidate, Box<CandidateReport>> {
    match try_prepare_candidate_dev(
        root,
        server,
        binary,
        candidate,
        model_id,
        revision,
        manifest_path,
        corpus,
        dev,
    ) {
        Ok(prepared) => Ok(prepared),
        Err(error) => {
            eprintln!("live-eval candidate={candidate} status=blocked code=eval.runtime_failed");
            Err(Box::new(blocked_candidate(
                candidate,
                model_id,
                revision,
                "eval.runtime_failed",
                safe_error_message(&error.to_string()),
                json!({
                    "manifest_name": manifest_path.file_name().and_then(|name| name.to_str()),
                    "manifest_exists": manifest_path.is_file(),
                }),
            )))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn try_prepare_candidate_dev(
    root: &Path,
    server: &PublicSnapshotServer,
    binary: &Path,
    candidate: &str,
    model_id: &str,
    revision: &str,
    manifest_path: &Path,
    corpus: &[CorpusRecord],
    dev: &[QrelRecord],
) -> Result<PreparedCandidate, DynError> {
    eprintln!("live-eval candidate={candidate} phase=sync status=running");
    let fixture = CliFixture::new(
        root.join(format!("{candidate}-live")),
        binary.to_path_buf(),
        server.base_url.clone(),
    )?;
    fixture.write_config(None)?;
    fixture.sync()?;
    let db_bytes_before = fixture.db_bytes()?;
    fixture.write_config(Some(manifest_path))?;
    let manifest_bytes = fs::read(manifest_path)?;
    let manifest = ModelManifestV1::from_json_slice(&manifest_bytes)?;
    let manifest_hash = manifest.hash();
    let snapshot_bytes = directory_bytes(manifest_path.parent().ok_or("manifest parent missing")?)?;

    eprintln!("live-eval candidate={candidate} phase=embed status=running");
    let quality_embed = fixture.timed_qgh(&["embed", "--force", "--json"])?;
    let embed_seconds = quality_embed.elapsed_ms / 1_000.0;
    let embed_json: Value = serde_json::from_slice(&quality_embed.output.stdout)?;
    let chunk_count = embed_json["data"]["chunks"]["embedded"]
        .as_u64()
        .unwrap_or_default() as usize;
    if chunk_count == 0 {
        return Err("embedding completed without any chunks".into());
    }
    let db_growth = fixture.db_bytes()?.saturating_sub(db_bytes_before);

    eprintln!("live-eval candidate={candidate} phase=cold-processes status=running");
    let mut cold_samples_ms = Vec::with_capacity(COLD_PROCESS_RUNS);
    let mut isolated_peak_rss = quality_embed.peak_rss_bytes;
    for _ in 0..COLD_PROCESS_RUNS {
        let sample = fixture.timed_query(&dev[0])?;
        cold_samples_ms.push(sample.elapsed_ms);
        isolated_peak_rss = isolated_peak_rss.max(sample.peak_rss_bytes);
    }

    eprintln!("live-eval candidate={candidate} phase=dev-mcp status=running");
    let (dev_evidence, dev_peak_rss) = run_dev_mcp(&fixture, dev)?;
    isolated_peak_rss = isolated_peak_rss.max(dev_peak_rss);
    let dev_metrics = evaluate_rankings(
        corpus,
        dev,
        &dev_evidence,
        &fixture.root.join("dev-events.jsonl"),
    )?;
    let offline_dev_diagnostics = offline_fusion_diagnostics(dev, &dev_evidence)?;
    Ok(PreparedCandidate {
        candidate: candidate.to_string(),
        model_id: model_id.to_string(),
        revision: revision.to_string(),
        manifest_hash,
        fixture,
        manifest_path: manifest_path.to_path_buf(),
        snapshot_bytes,
        chunk_count,
        embed_seconds,
        db_growth_bytes_per_chunk: db_growth as f64 / chunk_count as f64,
        cold_samples_ms,
        isolated_peak_rss,
        dev_metrics,
        offline_dev_diagnostics,
    })
}

fn finish_candidate(
    root: &Path,
    server: &PublicSnapshotServer,
    binary: &Path,
    mut prepared: PreparedCandidate,
    corpus: &[CorpusRecord],
    held_out: &[QrelRecord],
) -> CandidateReport {
    match try_finish_candidate(root, server, binary, &mut prepared, corpus, held_out) {
        Ok((held_out_metrics, resources)) => {
            let light_gate_failures = live_resource_failures(&resources, true);
            let quality_resource_gate_failures = live_resource_failures(&resources, false);
            let report = CandidateReport {
                candidate: prepared.candidate.clone(),
                model_id: prepared.model_id.clone(),
                resolved_revision: prepared.revision.clone(),
                runtime: "qgh release binary / fastembed UserDefinedEmbeddingModel".to_string(),
                status: "pre_integration_validation_resource_blocked".to_string(),
                manifest_hash: Some(prepared.manifest_hash.clone()),
                dev_metrics: Some(prepared.dev_metrics),
                held_out_metrics: Some(held_out_metrics),
                offline_dev_diagnostics: prepared.offline_dev_diagnostics,
                resources: Some(resources),
                light_gate_failures,
                quality_resource_gate_failures,
                blocker: Some(Blocker {
                    code: "eval.context_contract_not_integrated".to_string(),
                    message: "Manifest context-none and generation context-v1 are not an integrated final-eval contract.".to_string(),
                    details: json!({}),
                    evidence: json!({"required_rerun": "Lane D+A+B integrated SHA"}),
                }),
                synthetic_substitution: false,
            };
            let _ = write_pretty(prepared.fixture.root.join("report.json"), &report);
            report
        }
        Err(error) => CandidateReport {
            candidate: prepared.candidate,
            model_id: prepared.model_id,
            resolved_revision: prepared.revision,
            runtime: "qgh release binary / fastembed UserDefinedEmbeddingModel".to_string(),
            status: "blocked_after_dev".to_string(),
            manifest_hash: Some(prepared.manifest_hash),
            dev_metrics: Some(prepared.dev_metrics),
            held_out_metrics: None,
            offline_dev_diagnostics: prepared.offline_dev_diagnostics,
            resources: None,
            light_gate_failures: vec!["runtime_unavailable".to_string()],
            quality_resource_gate_failures: vec!["runtime_unavailable".to_string()],
            blocker: Some(Blocker {
                code: "eval.runtime_failed".to_string(),
                message: safe_error_message(&error.to_string()),
                details: json!({}),
                evidence: json!({"phase": "heldout_or_resource"}),
            }),
            synthetic_substitution: false,
        },
    }
}

fn try_finish_candidate(
    root: &Path,
    server: &PublicSnapshotServer,
    binary: &Path,
    prepared: &mut PreparedCandidate,
    corpus: &[CorpusRecord],
    held_out: &[QrelRecord],
) -> Result<(RetrievalMetrics, ResourceEvidence), DynError> {
    eprintln!(
        "live-eval candidate={} phase=heldout-warm-mcp status=running",
        prepared.candidate
    );
    let warm = run_heldout_mcp(&prepared.fixture, held_out)?;
    prepared.isolated_peak_rss = prepared.isolated_peak_rss.max(warm.peak_rss_bytes);
    let held_out_metrics = evaluate_rankings(
        corpus,
        held_out,
        &warm.held_out,
        &prepared.fixture.root.join("heldout-events.jsonl"),
    )?;
    eprintln!(
        "live-eval candidate={} phase=50k-effective-runtime status=running",
        prepared.candidate
    );
    let backfill = measure_50k_backfill(
        root,
        server,
        binary,
        &prepared.candidate,
        &prepared.manifest_path,
        corpus,
        &prepared.fixture.cache_home,
    )?;
    prepared.isolated_peak_rss = prepared.isolated_peak_rss.max(backfill.peak_rss_bytes);
    let resources = ResourceEvidence {
        cold_start_p95_ms: percentile(&prepared.cold_samples_ms, 0.95),
        cold_process_samples_ms: prepared.cold_samples_ms.clone(),
        warm_query_sample_count: warm.latencies_ms.len(),
        warm_query_p50_ms: percentile(&warm.latencies_ms, 0.50),
        warm_query_p95_ms: percentile(&warm.latencies_ms, 0.95),
        warm_path_includes_manifest_artifact_rehash: true,
        isolated_peak_rss_bytes: prepared.isolated_peak_rss,
        complete_model_snapshot_bytes: prepared.snapshot_bytes,
        quality_corpus_chunk_count: prepared.chunk_count,
        quality_corpus_embed_and_write_seconds: prepared.embed_seconds,
        quality_corpus_db_growth_bytes_per_chunk: prepared.db_growth_bytes_per_chunk,
        measured_50k_chunk_count: backfill.chunk_count,
        measured_chunk_tokens: backfill.chunk_tokens,
        measured_50k_embed_and_write_seconds: backfill.seconds,
        measured_50k_chunks_per_second: backfill.chunks_per_second,
        measured_50k_db_growth_bytes_per_chunk: backfill.db_growth_bytes_per_chunk,
        required_batch_size: REQUIRED_BATCH_SIZE,
        effective_batch_size: EFFECTIVE_BATCH_SIZE,
        required_intra_op_threads: REQUIRED_INTRA_OP_THREADS,
        effective_intra_op_threads: None,
        effective_ort_inter_op: "fastembed/ORT effective default; not exposed by qgh v1"
            .to_string(),
        effective_ort_execution_mode: "fastembed/ORT effective default; not exposed by qgh v1"
            .to_string(),
        fastembed_version: "5.17.2".to_string(),
        protocol_unverified: vec![
            "batch_size_8_unavailable_existing_runtime_hardcodes_16".to_string(),
            "intra_op_threads_4_not_exposed".to_string(),
        ],
    };
    Ok((held_out_metrics, resources))
}

fn blocked_candidate(
    candidate: &str,
    model_id: &str,
    revision: &str,
    code: &str,
    message: String,
    evidence: Value,
) -> CandidateReport {
    CandidateReport {
        candidate: candidate.to_string(),
        model_id: model_id.to_string(),
        resolved_revision: revision.to_string(),
        runtime: "qgh release binary / fastembed UserDefinedEmbeddingModel".to_string(),
        status: "blocked".to_string(),
        manifest_hash: None,
        dev_metrics: None,
        held_out_metrics: None,
        offline_dev_diagnostics: Vec::new(),
        resources: None,
        light_gate_failures: vec!["runtime_unavailable".to_string()],
        quality_resource_gate_failures: vec!["runtime_unavailable".to_string()],
        blocker: Some(Blocker {
            code: code.to_string(),
            message,
            details: json!({}),
            evidence,
        }),
        synthetic_substitution: false,
    }
}

fn dragonkue_blocker(root: &Path) -> CandidateReport {
    eprintln!("live-eval candidate=dragonkue-ko phase=manifest-probe status=running");
    let store = PreparedModelStore::new(root.join("dragonkue-probe/prepared-models"));
    let options = FastembedProviderOptions {
        manifest_path: None,
        model: Some(format!("hf:{DRAGONKUE_MODEL_ID}@{DRAGONKUE_REVISION}")),
        model_path: None,
        file: Some("onnx/model.onnx".to_string()),
        pooling: Some(PoolingKind::Cls),
        query_prefix: Some("query: ".to_string()),
        quantization: Some(QuantizationKind::None),
        token_source_env: None,
        cache_dir: Some(root.join("dragonkue-probe/hf-cache")),
    };
    let blocker = match store.acquire(&options) {
        Ok(_) => Blocker {
            code: "eval.unexpected_dragonkue_onnx".to_string(),
            message: "Upstream unexpectedly exposed the required ONNX graph; manual review required before scoring.".to_string(),
            details: json!({}),
            evidence: dragonkue_evidence(),
        },
        Err(error) => Blocker {
            code: error.code().to_string(),
            message: error.message().to_string(),
            details: error.details().clone(),
            evidence: dragonkue_evidence(),
        },
    };
    eprintln!(
        "live-eval candidate=dragonkue-ko status=blocked code={}",
        blocker.code
    );
    CandidateReport {
        candidate: "dragonkue-ko".to_string(),
        model_id: DRAGONKUE_MODEL_ID.to_string(),
        resolved_revision: DRAGONKUE_REVISION.to_string(),
        runtime: "qgh fastembed UserDefinedEmbeddingModel".to_string(),
        status: "blocked".to_string(),
        manifest_hash: None,
        dev_metrics: None,
        held_out_metrics: None,
        offline_dev_diagnostics: Vec::new(),
        resources: None,
        light_gate_failures: vec!["runtime_unavailable".to_string()],
        quality_resource_gate_failures: vec!["runtime_unavailable".to_string()],
        blocker: Some(blocker),
        synthetic_substitution: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn measure_50k_backfill(
    root: &Path,
    server: &PublicSnapshotServer,
    binary: &Path,
    candidate: &str,
    manifest_path: &Path,
    corpus: &[CorpusRecord],
    shared_cache: &Path,
) -> Result<BackfillEvidence, DynError> {
    let fixture = CliFixture::new_with_cache(
        root.join(format!("{candidate}-resource-live")),
        binary.to_path_buf(),
        server.base_url.clone(),
        shared_cache.to_path_buf(),
    )?;
    fixture.write_config(None)?;
    fixture.sync()?;
    fixture.write_config(Some(manifest_path))?;
    let _ = fixture.qgh(&[
        "query",
        "resource schema initialization",
        "--repo",
        "juicyjusung/qgh",
        "--json",
    ])?;
    let (chunk_body, chunk_tokens) = public_900_token_chunk(manifest_path, corpus)?;
    seed_50k_chunks(&fixture.db_path(), &chunk_body, chunk_tokens)?;
    let seeded_chunk_count = chunk_count(&fixture.db_path())?;
    if seeded_chunk_count != 50_000 {
        return Err(format!("resource seed produced {seeded_chunk_count} chunks").into());
    }
    let bytes_before = checkpoint_and_storage_bytes(&fixture.db_path())?;
    let started_at = command_output("date", &["+%Y-%m-%dT%H:%M:%S%z"]);
    eprintln!(
        "live-eval candidate={candidate} phase=50k-production-embed status=running chunks={seeded_chunk_count} started_at={started_at}"
    );
    let embed = fixture.timed_qgh_with_start(&["embed", "--force", "--json"], candidate)?;
    let seconds = embed.elapsed_ms / 1_000.0;
    let envelope: Value = serde_json::from_slice(&embed.output.stdout)?;
    let embedded = envelope["data"]["chunks"]["embedded"]
        .as_u64()
        .unwrap_or_default() as usize;
    if embedded != 50_000 {
        return Err(format!("effective-runtime backfill embedded {embedded} chunks").into());
    }
    let bytes_after = checkpoint_and_storage_bytes(&fixture.db_path())?;
    let db_growth = bytes_after.saturating_sub(bytes_before);
    Ok(BackfillEvidence {
        chunk_count: embedded,
        chunk_tokens,
        seconds,
        chunks_per_second: embedded as f64 / seconds,
        db_growth_bytes_per_chunk: db_growth as f64 / embedded as f64,
        peak_rss_bytes: embed.peak_rss_bytes,
    })
}

fn public_900_token_chunk(
    manifest_path: &Path,
    corpus: &[CorpusRecord],
) -> Result<(String, usize), DynError> {
    let store = PreparedModelStore::new(PathBuf::new());
    let snapshot = store.load_manifest(manifest_path)?;
    let tokenizer = FastembedTokenizer::from_prepared_snapshot(&snapshot)?;
    let public_text = corpus
        .iter()
        .find(|source| source.repo == "juicyjusung/qgh" && source.entity_type == "issue")
        .map(|source| source.body.as_str())
        .ok_or("public English source missing")?;
    let repeated = std::iter::repeat_n(public_text, 32)
        .collect::<Vec<_>>()
        .join("\n\n");
    let chunks = chunk_markdown_with_config(
        &repeated,
        &tokenizer,
        ChunkerConfig {
            target_tokens: 900,
            overlap_tokens: 0,
            boundary_search_tokens: 0,
        },
    )?;
    let chunk = chunks
        .into_iter()
        .find(|chunk| chunk.token_count == 900)
        .ok_or("tokenizer did not produce an exact 900-token public chunk")?;
    Ok((chunk.body, chunk.token_count))
}

fn seed_50k_chunks(db_path: &Path, body: &str, token_count: usize) -> Result<(), DynError> {
    let mut connection = Connection::open(db_path)?;
    let (source_id, source_version_id): (String, i64) = connection.query_row(
        "SELECT se.source_id, im.latest_version_id
         FROM source_entities se
         JOIN issue_metadata im ON im.source_id = se.source_id
         WHERE se.lifecycle_state = 'active'
         ORDER BY se.source_id
         LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM chunks", [])?;
    {
        let mut insert = transaction.prepare(
            "INSERT INTO chunks (
                source_id, source_version_id, body, chunk_index,
                token_start, token_end, byte_start, byte_end,
                chunker_version, chunker_fingerprint, heading_path_json
             ) VALUES (?1, ?2, ?3, ?4, 0, ?5, 0, ?6, ?7, ?8, '[]')",
        )?;
        for chunk_index in 0..50_000_i64 {
            insert.execute(params![
                source_id,
                source_version_id,
                body,
                chunk_index,
                token_count as i64,
                body.len() as i64,
                CHUNKER_VERSION,
                CHUNKER_FINGERPRINT,
            ])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn chunk_count(db_path: &Path) -> Result<usize, DynError> {
    let connection = Connection::open(db_path)?;
    Ok(connection.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?)
}

fn checkpoint_and_storage_bytes(db_path: &Path) -> Result<u64, DynError> {
    {
        let connection = Connection::open(db_path)?;
        let _: (i64, i64, i64) =
            connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
    }
    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    let main_bytes = fs::metadata(db_path)?.len();
    let wal_bytes = fs::metadata(wal_path).map_or(0, |metadata| metadata.len());
    Ok(main_bytes + wal_bytes)
}

fn dragonkue_evidence() -> Value {
    json!({
        "model_api": format!("https://huggingface.co/api/models/{DRAGONKUE_MODEL_ID}"),
        "resolved_revision": DRAGONKUE_REVISION,
        "license": "apache-2.0",
        "upstream_file_list_contains_onnx": false,
        "required_runtime_artifact": "onnx/model.onnx",
        "existing_runtime_adapter": "fastembed UserDefinedEmbeddingModel requires ONNX",
        "synthetic_substitution": false,
    })
}

fn run_single_pass(fixture: &CliFixture, qrels: &[QrelRecord]) -> Result<QueryEvidence, DynError> {
    let mut client = McpClient::start(fixture)?;
    let evidence = query_pass(&mut client, qrels, true, TOP_K)?.0;
    let _ = client.finish()?;
    Ok(evidence)
}

fn run_dev_mcp(fixture: &CliFixture, dev: &[QrelRecord]) -> Result<(QueryEvidence, u64), DynError> {
    let mut client = McpClient::start(fixture)?;
    let (dev_evidence, _) = query_pass(&mut client, dev, true, 100)?;
    let peak_rss_bytes = client.finish()?;
    Ok((dev_evidence, peak_rss_bytes))
}

fn run_heldout_mcp(
    fixture: &CliFixture,
    held_out: &[QrelRecord],
) -> Result<WarmEvidence, DynError> {
    let mut client = McpClient::start(fixture)?;
    for _ in 0..WARMUP_RUNS {
        let _ = query_pass(&mut client, held_out, false, TOP_K)?;
    }
    let mut latencies = Vec::with_capacity(held_out.len() * MEASURED_RUNS);
    let mut held_out_evidence = None;
    for measured in 0..MEASURED_RUNS {
        let capture_get = measured + 1 == MEASURED_RUNS;
        let (evidence, mut pass_latencies) = query_pass(&mut client, held_out, capture_get, TOP_K)?;
        latencies.append(&mut pass_latencies);
        if capture_get {
            held_out_evidence = Some(evidence);
        }
    }
    let peak_rss_bytes = client.finish()?;
    Ok(WarmEvidence {
        held_out: held_out_evidence.ok_or("held-out evidence missing")?,
        latencies_ms: latencies,
        peak_rss_bytes,
    })
}

fn query_pass(
    client: &mut McpClient,
    qrels: &[QrelRecord],
    verify_get: bool,
    query_limit: usize,
) -> Result<(QueryEvidence, Vec<f64>), DynError> {
    let mut rankings = BTreeMap::new();
    let mut branch_observations = BTreeMap::new();
    let mut latencies = Vec::with_capacity(qrels.len());
    let mut get_total = 0usize;
    let mut get_success = 0usize;
    let mut stale_failures = 0usize;
    let mut hybrid_path_queries = 0usize;
    for qrel in qrels {
        let mut arguments = json!({
            "query": qrel.query,
            "limit": query_limit,
            "repo": qrel.filters.repo,
        });
        if let Some(issue_number) = qrel.filters.issue_number {
            arguments["issue"] = json!(issue_number);
        }
        let started = Instant::now();
        let response = client.call_tool("query", arguments)?;
        latencies.push(started.elapsed().as_secs_f64() * 1_000.0);
        let structured = structured_content(&response)?;
        let results = structured["data"]["results"]
            .as_array()
            .ok_or("query result array missing")?;
        hybrid_path_queries += usize::from(
            results
                .iter()
                .any(|result| result["ranking"]["kind"].as_str() == Some("hybrid")),
        );
        let ranked = results
            .iter()
            .take(TOP_K)
            .filter_map(|result| result["source_id"].as_str().map(ToString::to_string))
            .collect::<Vec<_>>();
        let observed = results
            .iter()
            .filter_map(|result| {
                let source_id = result["source_id"].as_str()?.to_string();
                Some(BranchObservation {
                    source_id,
                    lexical_score: result["ranking"]["lexical_score"].as_f64(),
                    vector_distance: result["ranking"]["vector_distance"].as_f64(),
                })
            })
            .collect::<Vec<_>>();
        if verify_get {
            for source_id in &ranked {
                get_total += 1;
                let get_response = client.call_tool("get", json!({ "source_id": source_id }))?;
                match structured_content(&get_response) {
                    Ok(get) if get["data"]["source"]["source_id"].as_str() == Some(source_id) => {
                        get_success += 1;
                    }
                    Ok(get) => {
                        stale_failures +=
                            usize::from(get["error"]["code"].as_str() == Some("source.tombstoned"));
                    }
                    Err(_) => {}
                }
            }
        }
        rankings.insert(qrel.query_id.clone(), ranked);
        branch_observations.insert(qrel.query_id.clone(), observed);
    }
    Ok((
        QueryEvidence {
            rankings,
            branch_observations,
            get_total,
            get_success,
            stale_failures,
            hybrid_path_queries,
        },
        latencies,
    ))
}

fn offline_fusion_diagnostics(
    qrels: &[QrelRecord],
    evidence: &QueryEvidence,
) -> Result<Vec<OfflineFusionDiagnostic>, DynError> {
    let diagnostic_qrels = qrels
        .iter()
        .filter(|qrel| {
            !matches!(
                qrel.query_class,
                QueryClass::ExactIdentifier | QueryClass::Negative
            )
        })
        .collect::<Vec<_>>();
    let mut diagnostics = Vec::new();
    for rrf_k in DEV_DIAGNOSTIC_RRF_K {
        for candidate_window in DEV_DIAGNOSTIC_WINDOWS {
            let minimum_lexical = diagnostic_qrels
                .iter()
                .map(|qrel| {
                    evidence
                        .branch_observations
                        .get(&qrel.query_id)
                        .map_or(0, |hits| {
                            hits.iter()
                                .filter(|hit| hit.lexical_score.is_some())
                                .count()
                        })
                })
                .min()
                .unwrap_or(0);
            let minimum_vector = diagnostic_qrels
                .iter()
                .map(|qrel| {
                    evidence
                        .branch_observations
                        .get(&qrel.query_id)
                        .map_or(0, |hits| {
                            hits.iter()
                                .filter(|hit| hit.vector_distance.is_some())
                                .count()
                        })
                })
                .min()
                .unwrap_or(0);
            let complete =
                minimum_lexical >= candidate_window && minimum_vector >= candidate_window;
            let (weighted_ndcg_at_10, exact_top_1) = if complete {
                let mut per_class = BTreeMap::<QueryClass, (usize, f64)>::new();
                let mut exact_total = 0usize;
                let mut exact_hits = 0usize;
                for qrel in qrels {
                    let ranked = if qrel.query_class == QueryClass::ExactIdentifier {
                        evidence
                            .rankings
                            .get(&qrel.query_id)
                            .cloned()
                            .ok_or("dev diagnostic exact ranking missing")?
                    } else {
                        let observations = evidence
                            .branch_observations
                            .get(&qrel.query_id)
                            .ok_or("dev diagnostic branch observations missing")?;
                        fuse_branch_observations(observations, rrf_k, candidate_window)
                    };
                    let relevant = qrel
                        .relevant
                        .iter()
                        .map(|source| (source.source_id.as_str(), source.grade))
                        .collect::<Vec<_>>();
                    let metrics = metrics_for(&relevant, &ranked);
                    let entry = per_class.entry(qrel.query_class).or_default();
                    entry.0 += 1;
                    entry.1 += metrics.ndcg_at_10;
                    if qrel.query_class == QueryClass::ExactIdentifier {
                        exact_total += 1;
                        exact_hits += usize::from(ranked.first().is_some_and(|source_id| {
                            qrel.relevant
                                .iter()
                                .any(|gold| gold.source_id == *source_id)
                        }));
                    }
                }
                let mean = |class| {
                    per_class
                        .get(&class)
                        .map_or(0.0, |(count, sum)| sum / (*count).max(1) as f64)
                };
                (
                    Some(
                        0.50 * mean(QueryClass::EnglishSemantic)
                            + 0.20 * mean(QueryClass::KoreanSemantic)
                            + 0.10 * mean(QueryClass::KoQueryEnSource)
                            + 0.10 * mean(QueryClass::EnQueryKoSource)
                            + 0.05 * mean(QueryClass::CommentOnly)
                            + 0.05 * mean(QueryClass::LongContext),
                    ),
                    Some(if exact_total == 0 {
                        1.0
                    } else {
                        exact_hits as f64 / exact_total as f64
                    }),
                )
            } else {
                (None, None)
            };
            diagnostics.push(OfflineFusionDiagnostic {
                scope: "offline_dev_diagnostic_not_deployable",
                rrf_k,
                candidate_window,
                complete,
                incomplete_reason: (!complete).then_some("insufficient_branch_observability"),
                minimum_observed_lexical_candidates: minimum_lexical,
                minimum_observed_vector_candidates: minimum_vector,
                weighted_ndcg_at_10,
                exact_top_1,
            });
        }
    }
    Ok(diagnostics)
}

fn evaluate_rankings(
    corpus: &[CorpusRecord],
    qrels: &[QrelRecord],
    evidence: &QueryEvidence,
    events_path: &Path,
) -> Result<RetrievalMetrics, DynError> {
    if let Some(parent) = events_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut events = fs::File::create(events_path)?;
    let sources = corpus
        .iter()
        .map(|source| (source.source_id.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let mut per_class = BTreeMap::<QueryClass, ClassAccumulator>::new();
    let mut exact_total = 0usize;
    let mut exact_hits = 0usize;
    let mut hard_filter_violations = 0usize;
    let mut duplicate_crowding_queries = 0usize;
    for qrel in qrels {
        let ranked = evidence
            .rankings
            .get(&qrel.query_id)
            .ok_or("query ranking missing")?;
        let relevant = qrel
            .relevant
            .iter()
            .map(|source| (source.source_id.as_str(), source.grade))
            .collect::<Vec<_>>();
        let metrics = metrics_for(&relevant, ranked);
        let accumulator = per_class.entry(qrel.query_class).or_default();
        accumulator.count += 1;
        accumulator.ndcg_at_10 += metrics.ndcg_at_10;
        accumulator.mrr_at_10 += metrics.mrr_at_10;
        accumulator.recall_at_5 += metrics.recall_at_5;
        accumulator.recall_at_10 += metrics.recall_at_10;
        accumulator.recall_at_20 += metrics.recall_at_20;
        accumulator.nonempty += usize::from(!ranked.is_empty());
        if qrel.query_class == QueryClass::ExactIdentifier {
            exact_total += 1;
            exact_hits += usize::from(ranked.first().is_some_and(|source_id| {
                qrel.relevant
                    .iter()
                    .any(|gold| gold.source_id == *source_id)
            }));
        }
        let mut unique = BTreeSet::new();
        duplicate_crowding_queries += usize::from(
            ranked
                .iter()
                .take(10)
                .any(|source_id| !unique.insert(source_id.as_str())),
        );
        for source_id in ranked.iter().take(TOP_K) {
            let Some(source) = sources.get(source_id.as_str()) else {
                hard_filter_violations += 1;
                continue;
            };
            hard_filter_violations += usize::from(source.repo != qrel.filters.repo);
            hard_filter_violations += usize::from(
                qrel.filters
                    .issue_number
                    .is_some_and(|issue_number| source.issue_number != issue_number),
            );
        }
        let ranked_refs = ranked.iter().map(String::as_str).collect::<Vec<_>>();
        let event = redacted_query_event(
            &qrel.query_id,
            qrel.query_class,
            &qrel.query,
            &ranked_refs,
            metrics,
        );
        writeln!(events, "{}", serde_json::to_string(&event)?)?;
    }
    let per_class = per_class
        .into_iter()
        .map(|(class, accumulator)| {
            let count = accumulator.count.max(1) as f64;
            (
                class,
                ClassMetrics {
                    query_count: accumulator.count,
                    ndcg_at_10: accumulator.ndcg_at_10 / count,
                    mrr_at_10: accumulator.mrr_at_10 / count,
                    recall_at_5: accumulator.recall_at_5 / count,
                    recall_at_10: accumulator.recall_at_10 / count,
                    recall_at_20: accumulator.recall_at_20 / count,
                    negative_top_result_rate: (class == QueryClass::Negative)
                        .then_some(accumulator.nonempty as f64 / count),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let exact_top_1 = if exact_total == 0 {
        1.0
    } else {
        exact_hits as f64 / exact_total as f64
    };
    let get_round_trip = if evidence.get_total == 0 {
        1.0
    } else {
        evidence.get_success as f64 / evidence.get_total as f64
    };
    let weighted_ndcg_at_10 = weighted_ndcg(&per_class);
    let mut quality_gate_failures = Vec::new();
    if exact_top_1 < 0.95 {
        quality_gate_failures.push("exact_top_1".to_string());
    }
    for (class, minimum, name) in [
        (QueryClass::EnglishSemantic, 0.75, "english_recall_at_5"),
        (QueryClass::KoreanSemantic, 0.65, "korean_recall_at_5"),
        (QueryClass::KoQueryEnSource, 0.60, "ko_to_en_recall_at_5"),
        (QueryClass::EnQueryKoSource, 0.60, "en_to_ko_recall_at_5"),
    ] {
        if per_class
            .get(&class)
            .map_or(0.0, |metrics| metrics.recall_at_5)
            < minimum
        {
            quality_gate_failures.push(name.to_string());
        }
    }
    if hard_filter_violations != 0 {
        quality_gate_failures.push("hard_filter_violations".to_string());
    }
    if get_round_trip < 1.0 {
        quality_gate_failures.push("get_round_trip".to_string());
    }
    if evidence.stale_failures != 0 {
        quality_gate_failures.push("unexpected_tombstone_during_get".to_string());
    }
    Ok(RetrievalMetrics {
        query_count: qrels.len(),
        per_class,
        weighted_ndcg_at_10,
        exact_top_1,
        hard_filter_violations,
        get_round_trip,
        stale_leakage_live_fixture: None,
        duplicate_crowding_queries,
        hybrid_path_queries: evidence.hybrid_path_queries,
        quality_gate_failures,
    })
}

fn weighted_ndcg(per_class: &BTreeMap<QueryClass, ClassMetrics>) -> f64 {
    let metric = |class| per_class.get(&class).map_or(0.0, |value| value.ndcg_at_10);
    0.50 * metric(QueryClass::EnglishSemantic)
        + 0.20 * metric(QueryClass::KoreanSemantic)
        + 0.15 * metric(QueryClass::KoQueryEnSource)
        + 0.10 * metric(QueryClass::EnQueryKoSource)
        + 0.025 * metric(QueryClass::CommentOnly)
        + 0.025 * metric(QueryClass::LongContext)
}

fn live_resource_failures(resources: &ResourceEvidence, light: bool) -> Vec<String> {
    let mut failures = resources.protocol_unverified.clone();
    let gib = 1024_u64 * 1024 * 1024;
    if resources.warm_query_p95_ms > 1_500.0 {
        failures.push("warm_query_p95_ms".to_string());
    }
    if light {
        if resources.cold_start_p95_ms > 5_000.0 {
            failures.push("cold_start_p95_ms".to_string());
        }
        if resources.isolated_peak_rss_bytes > gib {
            failures.push("isolated_peak_rss_bytes".to_string());
        }
        if resources.complete_model_snapshot_bytes > 500 * 1024 * 1024 {
            failures.push("complete_model_snapshot_bytes".to_string());
        }
        if resources.measured_50k_chunks_per_second < 10.0 {
            failures.push("measured_50k_chunks_per_second".to_string());
        }
        if resources.measured_50k_db_growth_bytes_per_chunk > 3.0 * 1024.0 {
            failures.push("measured_50k_db_growth_bytes_per_chunk".to_string());
        }
    } else {
        if resources.cold_start_p95_ms > 10_000.0 {
            failures.push("cold_start_p95_ms".to_string());
        }
        if resources.isolated_peak_rss_bytes > 5 * gib / 2 {
            failures.push("isolated_peak_rss_bytes".to_string());
        }
        if resources.measured_50k_chunks_per_second < 3.0 {
            failures.push("measured_50k_chunks_per_second".to_string());
        }
    }
    failures
}

fn select_candidate(candidates: &[CandidateReport], light: bool) -> Option<String> {
    let mut eligible = candidates
        .iter()
        .filter(|candidate| candidate.status.starts_with("completed"))
        .filter(|candidate| {
            candidate
                .held_out_metrics
                .as_ref()
                .is_some_and(|metrics| metrics.quality_gate_failures.is_empty())
        })
        .filter(|candidate| {
            if light {
                candidate.light_gate_failures.is_empty()
            } else {
                candidate.quality_resource_gate_failures.is_empty()
            }
        })
        .collect::<Vec<_>>();
    eligible.sort_by(|left, right| {
        let left_ndcg = left
            .held_out_metrics
            .as_ref()
            .map_or(0.0, |metrics| metrics.weighted_ndcg_at_10);
        let right_ndcg = right
            .held_out_metrics
            .as_ref()
            .map_or(0.0, |metrics| metrics.weighted_ndcg_at_10);
        right_ndcg
            .partial_cmp(&left_ndcg)
            .unwrap_or(Ordering::Equal)
    });
    eligible
        .first()
        .map(|candidate| candidate.candidate.clone())
}

struct TimedQuery {
    elapsed_ms: f64,
    peak_rss_bytes: u64,
}

struct TimedOutput {
    output: Output,
    elapsed_ms: f64,
    peak_rss_bytes: u64,
}

struct BackfillEvidence {
    chunk_count: usize,
    chunk_tokens: usize,
    seconds: f64,
    chunks_per_second: f64,
    db_growth_bytes_per_chunk: f64,
    peak_rss_bytes: u64,
}

struct CliFixture {
    root: PathBuf,
    config_home: PathBuf,
    data_home: PathBuf,
    cache_home: PathBuf,
    binary: PathBuf,
    api_base_url: String,
}

impl CliFixture {
    fn new(root: PathBuf, binary: PathBuf, api_base_url: String) -> Result<Self, DynError> {
        let cache_home = root.join("cache");
        Self::new_with_cache(root, binary, api_base_url, cache_home)
    }

    fn new_with_cache(
        root: PathBuf,
        binary: PathBuf,
        api_base_url: String,
        cache_home: PathBuf,
    ) -> Result<Self, DynError> {
        let root = absolute_path(root)?;
        let cache_home = absolute_path(cache_home)?;
        remove_dir_if_exists(&root)?;
        let config_home = root.join("config");
        let data_home = root.join("data");
        fs::create_dir_all(config_home.join("qgh"))?;
        fs::create_dir_all(&data_home)?;
        fs::create_dir_all(&cache_home)?;
        Ok(Self {
            root,
            config_home,
            data_home,
            cache_home,
            binary,
            api_base_url,
        })
    }

    fn write_config(&self, manifest_path: Option<&Path>) -> Result<(), DynError> {
        let embedding = manifest_path.map_or_else(String::new, |path| {
            format!(
                "\n[embedding]\nprovider = \"local\"\nmanifest_path = \"{}\"\n",
                path.canonicalize()
                    .unwrap_or_else(|_| path.to_path_buf())
                    .to_string_lossy()
            )
        });
        let config = format!(
            r#"schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{}"
web_base_url = "https://github.com"
repos = ["juicyjusung/qgh"]

[profiles.work.token_source]
type = "env"
env = "QGH_PUBLIC_FIXTURE_AUTH"
{}"#,
            self.api_base_url, embedding
        );
        fs::write(self.config_home.join("qgh/config.toml"), config)?;
        Ok(())
    }

    fn sync(&self) -> Result<(), DynError> {
        let _ = self.qgh(&["sync", "--all", "--json"])?;
        Ok(())
    }

    fn qgh(&self, arguments: &[&str]) -> Result<Output, DynError> {
        let output = self.base_command().args(arguments).output()?;
        if !output.status.success() {
            return Err(command_failure(&output).into());
        }
        Ok(output)
    }

    fn timed_qgh(&self, arguments: &[&str]) -> Result<TimedOutput, DynError> {
        let mut command = Command::new("/usr/bin/time");
        command
            .arg("-l")
            .arg(&self.binary)
            .args(["--profile", "work"]);
        command.args(arguments);
        self.apply_env(&mut command);
        let started = Instant::now();
        let output = command.output()?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        if !output.status.success() {
            return Err(command_failure(&output).into());
        }
        let peak_rss_bytes = parse_peak_rss(&String::from_utf8_lossy(&output.stderr));
        Ok(TimedOutput {
            output,
            elapsed_ms,
            peak_rss_bytes,
        })
    }

    fn timed_qgh_with_start(
        &self,
        arguments: &[&str],
        candidate: &str,
    ) -> Result<TimedOutput, DynError> {
        let mut command = Command::new("/usr/bin/time");
        command
            .arg("-l")
            .arg(&self.binary)
            .args(["--profile", "work"]);
        command
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.apply_env(&mut command);
        let started = Instant::now();
        let child = command.spawn()?;
        eprintln!(
            "live-eval candidate={candidate} phase=50k-production-embed time_wrapper_pid={}",
            child.id()
        );
        let output = child.wait_with_output()?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        if !output.status.success() {
            return Err(command_failure(&output).into());
        }
        let peak_rss_bytes = parse_peak_rss(&String::from_utf8_lossy(&output.stderr));
        Ok(TimedOutput {
            output,
            elapsed_ms,
            peak_rss_bytes,
        })
    }

    fn timed_query(&self, qrel: &QrelRecord) -> Result<TimedQuery, DynError> {
        let mut command = Command::new("/usr/bin/time");
        command
            .arg("-l")
            .arg(&self.binary)
            .args([
                "--profile",
                "work",
                "query",
                &qrel.query,
                "--limit",
                "20",
                "--repo",
            ])
            .arg(&qrel.filters.repo);
        if let Some(issue_number) = qrel.filters.issue_number {
            command.args(["--issue", &issue_number.to_string()]);
        }
        command.arg("--json");
        self.apply_env(&mut command);
        let started = Instant::now();
        let output = command.output()?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        if !output.status.success() {
            return Err(command_failure(&output).into());
        }
        let envelope: Value = serde_json::from_slice(&output.stdout)?;
        if envelope["ok"].as_bool() != Some(true) {
            return Err("cold query returned a non-success envelope".into());
        }
        Ok(TimedQuery {
            elapsed_ms,
            peak_rss_bytes: parse_peak_rss(&String::from_utf8_lossy(&output.stderr)),
        })
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command.args(["--profile", "work"]);
        self.apply_env(&mut command);
        command
    }

    fn apply_env(&self, command: &mut Command) {
        command
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_DATA_HOME", &self.data_home)
            .env("XDG_CACHE_HOME", &self.cache_home)
            .env("QGH_PUBLIC_FIXTURE_AUTH", "unused-public-fixture")
            .env_remove("QGH_PROFILE")
            .env_remove("RUST_LOG")
            .current_dir(&self.root);
    }

    fn db_path(&self) -> PathBuf {
        self.data_home.join("qgh/profiles/work/qgh.sqlite3")
    }

    fn db_bytes(&self) -> Result<u64, DynError> {
        Ok(fs::metadata(self.db_path())?.len())
    }
}

struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    stderr: Option<ChildStderr>,
    next_id: u64,
}

impl McpClient {
    fn start(fixture: &CliFixture) -> Result<Self, DynError> {
        let mut command = Command::new("/usr/bin/time");
        command
            .arg("-l")
            .arg(&fixture.binary)
            .args(["--profile", "work", "mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        fixture.apply_env(&mut command);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or("MCP stdin missing")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("MCP stdout missing")?);
        let stderr = child.stderr.take().ok_or("MCP stderr missing")?;
        let mut client = Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: Some(stderr),
            next_id: 1,
        };
        let initialize = client.request(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "qgh-live-eval", "version": "1"}
            }
        }))?;
        if initialize["result"]["protocolVersion"].as_str() != Some("2025-11-25") {
            return Err("MCP initialization failed".into());
        }
        client.notify(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))?;
        client.next_id = 2;
        Ok(client)
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, DynError> {
        let id = self.next_id;
        self.next_id += 1;
        self.request(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }))
    }

    fn request(&mut self, message: Value) -> Result<Value, DynError> {
        self.notify(message)?;
        let mut line = String::new();
        let read = self
            .stdout
            .as_mut()
            .ok_or("MCP stdout closed")?
            .read_line(&mut line)?;
        if read == 0 {
            return Err("MCP server closed stdout".into());
        }
        Ok(serde_json::from_str(&line)?)
    }

    fn notify(&mut self, message: Value) -> Result<(), DynError> {
        let stdin = self.stdin.as_mut().ok_or("MCP stdin closed")?;
        writeln!(stdin, "{}", serde_json::to_string(&message)?)?;
        stdin.flush()?;
        Ok(())
    }

    fn finish(mut self) -> Result<u64, DynError> {
        drop(self.stdin.take());
        drop(self.stdout.take());
        let status = self.child.wait()?;
        let mut stderr = String::new();
        if let Some(mut pipe) = self.stderr.take() {
            pipe.read_to_string(&mut stderr)?;
        }
        if !status.success() {
            return Err("MCP server exited unsuccessfully".into());
        }
        Ok(parse_peak_rss(&stderr))
    }
}

fn structured_content(response: &Value) -> Result<&Value, DynError> {
    if response.get("error").is_some() {
        return Err("MCP transport error".into());
    }
    let structured = &response["result"]["structuredContent"];
    if structured["ok"].as_bool() != Some(true) {
        let code = structured["error"]["code"].as_str().unwrap_or("unknown");
        return Err(format!("MCP tool failed with code {code}").into());
    }
    Ok(structured)
}

struct PublicSnapshotServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PublicSnapshotServer {
    fn start(corpus: &[CorpusRecord]) -> Result<Self, DynError> {
        let responses = Arc::new(build_api_responses(corpus)?);
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let base_url = format!("http://{address}");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(AtomicOrdering::SeqCst) {
                    break;
                }
                if let Ok(stream) = stream {
                    handle_api_connection(stream, &responses);
                }
            }
        });
        Ok(Self {
            base_url,
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for PublicSnapshotServer {
    fn drop(&mut self) {
        self.stop.store(true, AtomicOrdering::SeqCst);
        if let Some(address) = self.base_url.strip_prefix("http://") {
            let _ = TcpStream::connect(address);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn build_api_responses(corpus: &[CorpusRecord]) -> Result<BTreeMap<String, String>, DynError> {
    let mut responses = BTreeMap::new();
    let mut issues_by_repo = BTreeMap::<&str, Vec<&CorpusRecord>>::new();
    let mut comments_by_thread = BTreeMap::<(&str, u64), Vec<&CorpusRecord>>::new();
    for source in corpus {
        if source.entity_type == "issue" {
            issues_by_repo.entry(&source.repo).or_default().push(source);
        } else {
            comments_by_thread
                .entry((&source.repo, source.issue_number))
                .or_default()
                .push(source);
        }
    }
    for (repo, issues) in issues_by_repo {
        let issue_values = issues
            .iter()
            .map(|source| {
                issue_json(
                    source,
                    comments_by_thread
                        .get(&(repo, source.issue_number))
                        .map_or(0, Vec::len),
                )
            })
            .collect::<Result<Vec<_>, DynError>>()?;
        responses.insert(
            format!("/repos/{repo}/issues"),
            serde_json::to_string(&issue_values)?,
        );
        for (source, issue_value) in issues.iter().zip(issue_values) {
            responses.insert(
                format!("/repos/{repo}/issues/{}", source.issue_number),
                serde_json::to_string(&issue_value)?,
            );
            let comments = comments_by_thread
                .get(&(repo, source.issue_number))
                .into_iter()
                .flatten()
                .map(|comment| comment_json(comment))
                .collect::<Result<Vec<_>, DynError>>()?;
            responses.insert(
                format!("/repos/{repo}/issues/{}/comments", source.issue_number),
                serde_json::to_string(&comments)?,
            );
        }
        let repo_comments = comments_by_thread
            .iter()
            .filter(|((comment_repo, _), _)| *comment_repo == repo)
            .flat_map(|(_, comments)| comments)
            .map(|comment| comment_json(comment))
            .collect::<Result<Vec<_>, DynError>>()?;
        responses.insert(
            format!("/repos/{repo}/issues/comments"),
            serde_json::to_string(&repo_comments)?,
        );
    }
    Ok(responses)
}

fn issue_json(source: &CorpusRecord, comment_count: usize) -> Result<Value, DynError> {
    Ok(json!({
        "id": synthetic_issue_id(&source.repo, source.issue_number),
        "node_id": decoded_node_id(&source.source_id)?,
        "number": source.issue_number,
        "title": source.title,
        "body": source.body,
        "state": "open",
        "locked": false,
        "comments": comment_count,
        "html_url": source.canonical_url,
        "created_at": source.github_updated_at,
        "updated_at": source.github_updated_at,
        "closed_at": null,
        "user": {"login": "public-fixture-author"},
        "labels": [],
        "milestone": null,
        "assignees": []
    }))
}

fn comment_json(source: &CorpusRecord) -> Result<Value, DynError> {
    let id = source
        .canonical_url
        .rsplit("issuecomment-")
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("comment id missing")?;
    Ok(json!({
        "id": id,
        "node_id": decoded_node_id(&source.source_id)?,
        "body": source.body,
        "html_url": source.canonical_url,
        "created_at": source.github_updated_at,
        "updated_at": source.github_updated_at,
        "user": {"login": "public-fixture-commenter"}
    }))
}

fn decoded_node_id(source_id: &str) -> Result<String, DynError> {
    let encoded = source_id
        .rsplit('/')
        .next()
        .ok_or("source id path missing")?;
    Ok(percent_decode_str(encoded).decode_utf8()?.into_owned())
}

fn synthetic_issue_id(repo: &str, issue_number: u64) -> u64 {
    let prefix = if repo == "juicyjusung/qgh" {
        1_000_000
    } else {
        2_000_000
    };
    prefix + issue_number
}

fn handle_api_connection(mut stream: TcpStream, responses: &BTreeMap<String, String>) {
    let mut buffer = [0_u8; 16_384];
    let bytes_read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().unwrap_or_default();
    let request_path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    let (status, body) = responses
        .get(request_path)
        .map_or(("404 Not Found", r#"{"message":"not found"}"#), |body| {
            ("200 OK", body.as_str())
        });
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\nx-ratelimit-remaining: 4999\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn command_failure(output: &Output) -> String {
    serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .map_or_else(
            || {
                format!(
                    "qgh command failed with exit status {:?}",
                    output.status.code()
                )
            },
            |value| {
                let code = value["error"]["code"].as_str().unwrap_or("unknown");
                let message = value["error"]["message"]
                    .as_str()
                    .map(safe_error_message)
                    .unwrap_or_else(|| "structured failure".to_string());
                format!("qgh command failed with structured code {code}: {message}")
            },
        )
}

fn safe_error_message(message: &str) -> String {
    if message.contains("query") || message.contains("body") {
        "Live evaluation failed; see the structured blocker code and phase.".to_string()
    } else {
        message.to_string()
    }
}

fn parse_peak_rss(stderr: &str) -> u64 {
    stderr
        .lines()
        .find(|line| line.contains("maximum resident set size"))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

fn percentile(values: &[f64], percentile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
    let index = ((sorted.len() as f64 * percentile).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[index]
}

fn directory_bytes(path: &Path) -> Result<u64, DynError> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total += directory_bytes(&entry.path())?;
        } else if metadata.is_file() {
            total += metadata.len();
        }
    }
    Ok(total)
}

fn eval_binary() -> Result<PathBuf, DynError> {
    let path = std::env::var_os("QGH_LIVE_MODEL_EVAL_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/release/qgh"));
    if !path.is_file() {
        return Err(format!("live eval binary is unavailable: {}", path.display()).into());
    }
    Ok(path.canonicalize()?)
}

fn host_record(binary: &Path) -> HostRecord {
    let os_version = command_output("sw_vers", &["-productVersion"]);
    let hardware_model_identifier = system_profiler_field("Model Identifier");
    let chip = system_profiler_field("Chip");
    let total_cores = system_profiler_field("Total Number of Cores");
    let system_profiler_memory = system_profiler_field("Memory");
    let power_source = current_power_source();
    let ac_power_mode = ac_power_mode();
    let reference_protocol_match = hardware_model_identifier == "Mac16,8"
        && chip == "Apple M4 Pro"
        && total_cores.starts_with("14 ")
        && system_profiler_memory == "48 GB"
        && os_version == "26.5.1"
        && power_source == "AC Power"
        && ac_power_mode.is_some_and(|mode| mode != 1);
    HostRecord {
        os: std::env::consts::OS.to_string(),
        os_version,
        architecture: std::env::consts::ARCH.to_string(),
        cpu: chip,
        hardware_model_identifier,
        total_cores,
        system_profiler_memory,
        ram_bytes: command_output("sysctl", &["-n", "hw.memsize"])
            .parse()
            .unwrap_or_default(),
        rustc: command_output("rustc", &["--version"]),
        cargo: command_output("cargo", &["--version"]),
        fastembed: "5.17.2",
        ort: "2.0.0-rc.12",
        power_source,
        ac_power_mode,
        reference_protocol_match,
        binary: binary
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("qgh")
            .to_string(),
        binary_sha256: command_output("shasum", &["-a", "256", &binary.to_string_lossy()])
            .split_whitespace()
            .next()
            .unwrap_or("unavailable")
            .to_string(),
        git_sha: command_output("git", &["rev-parse", "HEAD"]),
    }
}

fn system_profiler_field(field: &str) -> String {
    command_output("system_profiler", &["SPHardwareDataType"])
        .lines()
        .find_map(|line| {
            let (key, value) = line.trim().split_once(':')?;
            (key == field).then(|| value.trim().to_string())
        })
        .unwrap_or_else(|| "unavailable".to_string())
}

fn current_power_source() -> String {
    let output = command_output("pmset", &["-g", "batt"]);
    for source in ["AC Power", "Battery Power", "UPS Power"] {
        if output
            .lines()
            .next()
            .is_some_and(|line| line.contains(source))
        {
            return source.to_string();
        }
    }
    "unavailable".to_string()
}

fn ac_power_mode() -> Option<u8> {
    let output = command_output("pmset", &["-g", "custom"]);
    let mut in_ac = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.ends_with("Power:") {
            in_ac = trimmed == "AC Power:";
            continue;
        }
        if in_ac {
            let mut fields = trimmed.split_whitespace();
            if fields.next() == Some("powermode") {
                return fields.next().and_then(|value| value.parse().ok());
            }
        }
    }
    None
}

fn host_protocol_failures(host: &HostRecord) -> Vec<String> {
    let mut failures = Vec::new();
    if host.hardware_model_identifier != "Mac16,8" {
        failures.push("reference_host_model_identifier".to_string());
    }
    if host.cpu != "Apple M4 Pro" || !host.total_cores.starts_with("14 ") {
        failures.push("reference_host_cpu".to_string());
    }
    if host.system_profiler_memory != "48 GB" {
        failures.push("reference_host_memory".to_string());
    }
    if host.os_version != "26.5.1" {
        failures.push("reference_host_os".to_string());
    }
    if host.power_source != "AC Power" {
        failures.push("reference_host_ac_power".to_string());
    }
    if host.ac_power_mode.is_none_or(|mode| mode == 1) {
        failures.push("reference_host_low_power_mode".to_string());
    }
    failures
}

fn stale_contract_gate() -> ExternalContractGate {
    let status = std::env::var("QGH_LIVE_MODEL_EVAL_STALE_GATE_STATUS")
        .ok()
        .filter(|value| value == "passed")
        .unwrap_or_else(|| "unverified".to_string());
    let result_sha256 = std::env::var("QGH_LIVE_MODEL_EVAL_STALE_GATE_SHA256")
        .ok()
        .filter(|value| is_sha256(value));
    ExternalContractGate {
        name: "deleted_comment_reconciliation_excludes_tombstone_from_query",
        command: "cargo test --all-features --test issue_body_tracer full_reconciliation_tombstones_deleted_comments_and_updates_status -- --exact",
        status,
        result_sha256,
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn command_output(command: &str, arguments: &[&str]) -> String {
    Command::new(command)
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|| "unavailable".to_string())
}

fn ensure_target_root(root: &Path) -> Result<(), DynError> {
    let normalized = root.to_string_lossy().replace('\\', "/");
    if !normalized.contains("target/qgh-eval") {
        return Err("live eval artifacts must stay under target/qgh-eval".into());
    }
    Ok(())
}

fn absolute_path(path: PathBuf) -> Result<PathBuf, DynError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn remove_dir_if_exists(path: &Path) -> Result<(), DynError> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn write_pretty(path: PathBuf, value: &impl Serialize) -> Result<(), DynError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, with_newline(serde_json::to_vec_pretty(value)?))?;
    Ok(())
}

fn with_newline(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes.push(b'\n');
    bytes
}
