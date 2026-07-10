use super::{
    digest_hex, metrics_for, parse_jsonl, redacted_query_event, CorpusRecord, FixtureProvenance,
    QrelRecord, QueryClass,
};
use percent_encoding::percent_decode_str;
use qgh::chunking::{
    chunk_markdown_with_config, ChunkerConfig, CHUNKER_FINGERPRINT, CHUNKER_VERSION,
};
use qgh::context::{
    embedding_context_hash, prepare_embedding_input, EmbeddingSourceContext,
    METADATA_CONTEXT_TEMPLATE_VERSION,
};
use qgh::embedding::{
    EmbeddingTokenizer, FastembedProviderOptions, FastembedTokenizer, ModelManifestV1, PoolingKind,
    PreparedModelStore, QuantizationKind,
};
use qgh::search_eval::{search_with_lexical_profile_for_eval, EvalLexicalProfile, SearchFilters};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
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
    Arc, Mutex, OnceLock,
};
use std::thread::{self, JoinHandle};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const TOP_K: usize = 20;
const WARMUP_RUNS: usize = 1;
const MEASURED_RUNS: usize = 3;
const COLD_PROCESS_RUNS: usize = 5;
const RRF_K: usize = 60;
const CANDIDATE_WINDOW: usize = TOP_K * 4;
const DEV_DIAGNOSTIC_QUERY_LIMIT: usize = 100;
const DEV_DIAGNOSTIC_RRF_K: [usize; 3] = [20, 60, 100];
const DEV_DIAGNOSTIC_WINDOWS: [usize; 3] = [40, 80, 100];
const REQUIRED_BATCH_SIZE: usize = 8;
const EFFECTIVE_BATCH_SIZE: usize = 16;
const REQUIRED_INTRA_OP_THREADS: usize = 4;
const DRAGONKUE_MODEL_ID: &str = "dragonkue/snowflake-arctic-embed-l-v2.0-ko";
const DRAGONKUE_REVISION: &str = "55ec6e9358a56d56af759bc8372e970caf8c305f";
const TEST_EMBEDDING_QUERY_VECTORS_ENV: &str = "QGH_TEST_EMBEDDING_QUERY_VECTORS";
const TEST_EMBEDDING_DOCUMENT_VECTORS_ENV: &str = "QGH_TEST_EMBEDDING_DOCUMENT_VECTORS";
const FILTER_PROBE_TARGET_REPO: &str = "juicyjusung/qgh";
const FILTER_PROBE_COMPETING_REPO: &str = "competing/filter-probe";
const FILTER_PROBE_TARGET_ISSUE: u64 = 900_001;
const FILTER_PROBE_SENTINEL: &str = "bounded-adversarial-filter-sentinel";
const FILTER_PROBE_PRESET: &str = "arctic-l-v2-fp32";
const CONTRACT_GATE_BUNDLE_FILE: &str = "contract-gate-bundle.json";

struct ContractGateSpec {
    name: &'static str,
    arguments: &'static [&'static str],
}

const REQUIRED_CONTRACT_GATES: [ContractGateSpec; 7] = [
    ContractGateSpec {
        name: "edit_reconciliation",
        arguments: &[
            "test",
            "--all-features",
            "--test",
            "issue_body_tracer",
            "sync_issue_refreshes_target_issue_and_reconciles_comment_diff",
            "--",
            "--exact",
        ],
    },
    ContractGateSpec {
        name: "delete_and_stale_exclusion",
        arguments: &[
            "test",
            "--all-features",
            "--test",
            "issue_body_tracer",
            "full_reconciliation_tombstones_deleted_comments_and_updates_status",
            "--",
            "--exact",
        ],
    },
    ContractGateSpec {
        name: "purge_pending_retry",
        arguments: &[
            "test",
            "--all-features",
            "store::tests::purge_retry_finishes_idempotently_and_clears_pending",
            "--",
            "--exact",
        ],
    },
    ContractGateSpec {
        name: "parent_context_invalidation",
        arguments: &[
            "test",
            "--all-features",
            "embedding::tests::parent_issue_title_change_invalidates_comment_context_hash",
            "--",
            "--exact",
        ],
    },
    ContractGateSpec {
        name: "concurrent_publication_snapshot",
        arguments: &[
            "test",
            "--all-features",
            "--test",
            "issue_body_tracer",
            "concurrent_cli_sync_and_mcp_reads_keep_index_queryable",
            "--",
            "--exact",
        ],
    },
    ContractGateSpec {
        name: "bm25_search_quality",
        arguments: &["test", "--all-features", "--test", "search_quality_eval"],
    },
    ContractGateSpec {
        name: "hard_filter_exclusion",
        arguments: &[
            "test",
            "--all-features",
            "--test",
            "live_model_eval",
            "production_hard_filter_contract_excludes_competing_sources",
            "--",
            "--exact",
        ],
    },
];

type DynError = Box<dyn Error>;
static STDERR_AUDIT: OnceLock<Mutex<Vec<Vec<u8>>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DevQueryProtocol {
    pub(super) primary_query_limit: usize,
    pub(super) primary_candidate_window: usize,
    pub(super) diagnostic_query_limit: usize,
    pub(super) diagnostic_can_select: bool,
}

impl DevQueryProtocol {
    const fn frozen() -> Self {
        Self {
            primary_query_limit: TOP_K,
            primary_candidate_window: CANDIDATE_WINDOW,
            diagnostic_query_limit: DEV_DIAGNOSTIC_QUERY_LIMIT,
            diagnostic_can_select: false,
        }
    }
}

pub(super) fn dev_query_protocol_for_test() -> DevQueryProtocol {
    DevQueryProtocol::frozen()
}

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

#[derive(Debug, Clone, Serialize)]
struct RetrievalMetrics {
    query_count: usize,
    per_class: BTreeMap<QueryClass, ClassMetrics>,
    weighted_ndcg_at_10: f64,
    weighted_mrr_at_10: f64,
    exact_top_1: f64,
    hard_filter_violations: usize,
    get_round_trip: f64,
    stale_leakage_live_fixture: Option<usize>,
    duplicate_crowding_queries: usize,
    hybrid_expected_queries: usize,
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

#[derive(Debug, Clone, Serialize)]
struct ResourceEvidence {
    phase: String,
    complete: bool,
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
    measured_50k_chunk_count: Option<usize>,
    measured_raw_chunk_tokens: Option<usize>,
    measured_contextual_chunk_tokens: Option<usize>,
    measured_50k_embed_and_write_seconds: Option<f64>,
    measured_50k_chunks_per_second: Option<f64>,
    measured_50k_db_growth_bytes_per_chunk: Option<f64>,
    backfill_integrity: Option<BackfillIntegrityEvidence>,
    download_transfer_bytes: Option<u64>,
    required_batch_size: usize,
    effective_batch_size: usize,
    required_intra_op_threads: usize,
    effective_intra_op_threads: Option<usize>,
    effective_ort_inter_op: String,
    effective_ort_execution_mode: String,
    fastembed_version: String,
    protocol_unverified: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct Blocker {
    code: String,
    phase: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BackfillIntegrityEvidence {
    raw_chunks: usize,
    generation_state: String,
    generation_output_dimension: usize,
    generation_total_chunks: usize,
    generation_completed_chunks: usize,
    generation_chunk_rows: usize,
    vector_mapping_rows: usize,
    vector_table: String,
    vector_table_count: usize,
    vec0_rows: usize,
    publication_embedding_generation_id: i64,
    publication_active: bool,
}

#[derive(Debug, Clone, Default)]
struct PartialBackfillEvidence {
    chunk_count: Option<usize>,
    raw_chunk_tokens: Option<usize>,
    contextual_chunk_tokens: Option<usize>,
    seconds: Option<f64>,
    chunks_per_second: Option<f64>,
    db_growth_bytes_per_chunk: Option<f64>,
    peak_rss_bytes: u64,
    integrity: Option<BackfillIntegrityEvidence>,
}

#[derive(Debug)]
struct ResourceRunFailure {
    code: &'static str,
    phase: &'static str,
    partial: PartialBackfillEvidence,
}

#[derive(Debug, Clone, Serialize)]
struct ContextContractEvidence {
    required_template_version: &'static str,
    manifest_template_version: String,
    generation_template_version: String,
    issue_rows_checked: usize,
    comment_rows_checked: usize,
    context_hash_mismatches: usize,
    passed: bool,
}

#[derive(Debug)]
struct ContextContractFailure {
    manifest_hash: String,
    candidate_database_schema_fingerprint: String,
    candidate_tantivy_schema_fingerprint: String,
    evidence: ContextContractEvidence,
}

impl std::fmt::Display for ContextContractFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "stored embedding context contract did not match qgh.context.v1"
        )
    }
}

impl Error for ContextContractFailure {}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ContractGateRecord {
    name: String,
    command: String,
    exit_status: i32,
    result_artifact: String,
    result_sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ContractGateResult {
    schema_version: String,
    name: String,
    git_sha: String,
    binary_sha256: String,
    command: String,
    exit_status: i32,
    observed_test_count: usize,
    command_output_sha256: String,
    result: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ContractGateBundle {
    schema_version: String,
    git_sha: String,
    binary_sha256: String,
    gates: Vec<ContractGateRecord>,
}

#[derive(Debug, Clone, Serialize)]
struct VerifiedContractGateBundle {
    schema_version: &'static str,
    artifact: &'static str,
    sha256: String,
    git_sha: String,
    binary_sha256: String,
    gates: Vec<ContractGateRecord>,
}

fn load_contract_gate_bundle(
    root: &Path,
    expected_git_sha: &str,
    expected_binary_sha256: &str,
    expected_bundle_sha256: Option<&str>,
) -> Result<VerifiedContractGateBundle, DynError> {
    let path = root.join(CONTRACT_GATE_BUNDLE_FILE);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| "canonical live-eval contract gate bundle is unavailable")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("canonical live-eval contract gate bundle must be a regular file".into());
    }
    let bytes =
        fs::read(&path).map_err(|_| "canonical live-eval contract gate bundle is unavailable")?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    if expected_bundle_sha256.is_some_and(|expected| expected != sha256) {
        return Err("canonical live-eval contract gate bundle changed after freeze".into());
    }
    let bundle: ContractGateBundle = serde_json::from_slice(&bytes)
        .map_err(|_| "canonical live-eval contract gate bundle is invalid")?;
    if bundle.schema_version != "qgh.live_model_eval_gate_bundle.v1"
        || bundle.git_sha != expected_git_sha
        || bundle.binary_sha256 != expected_binary_sha256
        || !is_git_object_id(&bundle.git_sha)
        || !is_sha256(&bundle.binary_sha256)
        || bundle.gates.len() != REQUIRED_CONTRACT_GATES.len()
    {
        return Err("canonical live-eval contract gate bundle identity mismatch".into());
    }
    for (actual, expected) in bundle.gates.iter().zip(REQUIRED_CONTRACT_GATES) {
        let expected_command = contract_gate_command(&expected);
        let expected_result_artifact = format!("contract-gates/{}.json", expected.name);
        if actual.name != expected.name
            || actual.command != expected_command
            || actual.exit_status != 0
            || actual.result_artifact != expected_result_artifact
            || !is_sha256(&actual.result_sha256)
        {
            return Err("canonical live-eval contract gate record failed verification".into());
        }
        let result_path = confined_contract_gate_result_path(root, &actual.result_artifact)?;
        let result_bytes = fs::read(result_path)
            .map_err(|_| "canonical live-eval contract gate result is unavailable")?;
        if format!("{:x}", Sha256::digest(&result_bytes)) != actual.result_sha256 {
            return Err("canonical live-eval contract gate result hash mismatch".into());
        }
        let result: ContractGateResult = serde_json::from_slice(&result_bytes)
            .map_err(|_| "canonical live-eval contract gate result is invalid")?;
        if result.schema_version != "qgh.live_model_eval_gate_result.v1"
            || result.name != actual.name
            || result.git_sha != bundle.git_sha
            || result.binary_sha256 != bundle.binary_sha256
            || result.command != actual.command
            || result.exit_status != actual.exit_status
            || result.observed_test_count != 1
            || !is_sha256(&result.command_output_sha256)
            || result.result != "passed"
        {
            return Err("canonical live-eval contract gate result identity mismatch".into());
        }
    }
    Ok(VerifiedContractGateBundle {
        schema_version: "qgh.live_model_eval_verified_gate_bundle.v1",
        artifact: CONTRACT_GATE_BUNDLE_FILE,
        sha256,
        git_sha: bundle.git_sha,
        binary_sha256: bundle.binary_sha256,
        gates: bundle.gates,
    })
}

fn confined_contract_gate_result_path(root: &Path, relative: &str) -> Result<PathBuf, DynError> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || !relative.starts_with("contract-gates")
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("canonical live-eval contract gate result path is invalid".into());
    }
    let mut path = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err("canonical live-eval contract gate result path is invalid".into());
        };
        path.push(component);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| "canonical live-eval contract gate result is unavailable")?;
        if metadata.file_type().is_symlink() {
            return Err("canonical live-eval contract gate result path contains a symlink".into());
        }
    }
    if !fs::symlink_metadata(&path)?.is_file() {
        return Err("canonical live-eval contract gate result must be a regular file".into());
    }
    Ok(path)
}

fn is_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn contract_gate_command(spec: &ContractGateSpec) -> String {
    format!("cargo {}", spec.arguments.join(" "))
}

fn observed_contract_test_count(stdout: &[u8], stderr: &[u8]) -> usize {
    [stdout, stderr]
        .into_iter()
        .map(|stream| {
            let output = String::from_utf8_lossy(stream);
            output
                .lines()
                .filter_map(|line| {
                    let summary = line.split_once("test result:")?.1;
                    let passed = summary.split_once(" passed;")?.0;
                    passed.split_whitespace().last()?.parse::<usize>().ok()
                })
                .sum::<usize>()
        })
        .sum()
}

fn command_output_sha256(output: &Output) -> String {
    let mut digest = Sha256::new();
    digest.update(&output.stdout);
    digest.update([0]);
    digest.update(&output.stderr);
    format!("{:x}", digest.finalize())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), DynError> {
    let parent = path
        .parent()
        .ok_or("atomic artifact parent is unavailable")?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err("atomic artifact parent must be a regular directory".into());
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("atomic artifact name is invalid")?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "atomic artifact clock is unavailable")?
        .as_nanos();
    let temporary = parent.join(format!(
        ".{file_name}.{}.{nonce}.partial",
        std::process::id()
    ));
    let mut options = fs::OpenOptions::new();
    let mut file = options.write(true).create_new(true).open(&temporary)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    drop(file);
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

fn run_contract_gate_bundle(
    root: &Path,
    repo_root: &Path,
    git_sha: &str,
    binary: &Path,
    binary_sha256: &str,
) -> Result<(), DynError> {
    let result_root = root.join("contract-gates");
    if result_root.exists() {
        let metadata = fs::symlink_metadata(&result_root)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("contract gate artifact directory must be a regular directory".into());
        }
    } else {
        fs::create_dir(&result_root)?;
    }

    let mut gates = Vec::with_capacity(REQUIRED_CONTRACT_GATES.len());
    for spec in REQUIRED_CONTRACT_GATES {
        let command = contract_gate_command(&spec);
        let output = Command::new("cargo")
            .args(spec.arguments)
            .current_dir(repo_root)
            .output()
            .map_err(|_| "canonical live-eval contract gate could not be executed")?;
        let observed_test_count = observed_contract_test_count(&output.stdout, &output.stderr);
        if !output.status.success() || output.status.code() != Some(0) || observed_test_count != 1 {
            return Err(format!("canonical live-eval contract gate failed: {}", spec.name).into());
        }
        let result_artifact = format!("contract-gates/{}.json", spec.name);
        let result = ContractGateResult {
            schema_version: "qgh.live_model_eval_gate_result.v1".to_string(),
            name: spec.name.to_string(),
            git_sha: git_sha.to_string(),
            binary_sha256: binary_sha256.to_string(),
            command: command.clone(),
            exit_status: 0,
            observed_test_count,
            command_output_sha256: command_output_sha256(&output),
            result: "passed".to_string(),
        };
        let result_bytes = with_newline(serde_json::to_vec_pretty(&result)?);
        write_atomic(&root.join(&result_artifact), &result_bytes)?;
        gates.push(ContractGateRecord {
            name: spec.name.to_string(),
            command,
            exit_status: 0,
            result_artifact,
            result_sha256: format!("{:x}", Sha256::digest(&result_bytes)),
        });
    }
    let (verified_git_sha, worktree_clean) = repository_identity(repo_root)?;
    if verified_git_sha != git_sha || !worktree_clean || file_sha256(binary)? != binary_sha256 {
        return Err("contract gate run identity changed during execution".into());
    }
    let bundle = ContractGateBundle {
        schema_version: "qgh.live_model_eval_gate_bundle.v1".to_string(),
        git_sha: git_sha.to_string(),
        binary_sha256: binary_sha256.to_string(),
        gates,
    };
    write_atomic(
        &root.join(CONTRACT_GATE_BUNDLE_FILE),
        &with_newline(serde_json::to_vec_pretty(&bundle)?),
    )
}

pub(super) fn observed_contract_test_count_for_test(stdout: &[u8], stderr: &[u8]) -> usize {
    observed_contract_test_count(stdout, stderr)
}

pub(super) fn contract_gate_bundle_json_for_test(
    root: &Path,
    git_sha: &str,
    binary_sha256: &str,
) -> Result<Value, DynError> {
    let mut gates = Vec::new();
    for spec in REQUIRED_CONTRACT_GATES {
        let command = contract_gate_command(&spec);
        let result_artifact = format!("contract-gates/{}.json", spec.name);
        let result = ContractGateResult {
            schema_version: "qgh.live_model_eval_gate_result.v1".to_string(),
            name: spec.name.to_string(),
            git_sha: git_sha.to_string(),
            binary_sha256: binary_sha256.to_string(),
            command: command.clone(),
            exit_status: 0,
            observed_test_count: 1,
            command_output_sha256: "e".repeat(64),
            result: "passed".to_string(),
        };
        let result_bytes = with_newline(serde_json::to_vec_pretty(&result)?);
        let path = root.join(&result_artifact);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, &result_bytes)?;
        gates.push(ContractGateRecord {
            name: spec.name.to_string(),
            command,
            exit_status: 0,
            result_artifact,
            result_sha256: format!("{:x}", Sha256::digest(&result_bytes)),
        });
    }
    Ok(serde_json::to_value(ContractGateBundle {
        schema_version: "qgh.live_model_eval_gate_bundle.v1".to_string(),
        git_sha: git_sha.to_string(),
        binary_sha256: binary_sha256.to_string(),
        gates,
    })
    .expect("contract gate test bundle serializes"))
}

pub(super) fn verify_contract_gate_bundle_path_for_test(
    root: &Path,
    expected_git_sha: &str,
    expected_binary_sha256: &str,
    expected_bundle_sha256: Option<&str>,
) -> Result<String, DynError> {
    Ok(load_contract_gate_bundle(
        root,
        expected_git_sha,
        expected_binary_sha256,
        expected_bundle_sha256,
    )?
    .sha256)
}

#[derive(Debug, Serialize)]
struct RedactionEvidence {
    stderr_streams_checked: usize,
    artifact_files_checked: usize,
    violation_artifacts: Vec<String>,
    passed: bool,
}

#[derive(Debug, Serialize)]
struct CandidateReport {
    candidate: String,
    model_id: String,
    resolved_revision: String,
    runtime: String,
    status: String,
    manifest_hash: Option<String>,
    candidate_database_schema_fingerprint: Option<String>,
    candidate_tantivy_schema_fingerprint: Option<String>,
    context_contract: Option<ContextContractEvidence>,
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
    integrated_git_head: String,
    worktree_clean: bool,
    release_binary_sha256: String,
    contract_gate_bundle_sha256: String,
    model_preparation_provenance_sha256: String,
    candidate_states: Vec<FrozenCandidateState>,
    corpus_sha256: String,
    qrels_dev_sha256: String,
    qrels_test_sha256: String,
    chunker_version: &'static str,
    chunker_fingerprint: &'static str,
    context_profile: &'static str,
    fusion: &'static str,
    rrf_k: usize,
    candidate_window: usize,
    lexical_profile: FrozenLexicalProfile,
    warmup_runs: usize,
    measured_runs: usize,
    cold_process_runs: usize,
    required_50k_chunks: usize,
    required_chunk_tokens: usize,
    required_batch_size: usize,
    required_intra_op_threads: usize,
    database_schema_version: &'static str,
    vector_schema_version: &'static str,
    database_schema_fingerprint: String,
    tantivy_schema_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FrozenCandidateState {
    candidate: String,
    model_id: String,
    resolved_revision: String,
    dev_state: String,
    blocker_code: Option<String>,
    dev_metrics_sha256: Option<String>,
    offline_dev_diagnostics_sha256: Option<String>,
    manifest_relative_path: Option<String>,
    manifest_hash: Option<String>,
    manifest_file_sha256: Option<String>,
    artifact_set_sha256: Option<String>,
    prepared_snapshot_sha256: Option<String>,
    prepared_snapshot_bytes: Option<u64>,
    prepared_snapshot_file_count: Option<usize>,
    download_transfer_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PreparedSnapshotDigest {
    sha256: String,
    bytes: u64,
    file_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum FrozenLexicalProfileName {
    ProductionV1,
    MetadataBoostV1,
}

impl FrozenLexicalProfileName {
    fn eval_profile(self) -> EvalLexicalProfile {
        match self {
            Self::ProductionV1 => EvalLexicalProfile::ProductionV1,
            Self::MetadataBoostV1 => EvalLexicalProfile::MetadataBoostV1,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct FrozenLexicalProfile {
    production_profile: &'static str,
    comparison_candidate: &'static str,
    selected_profile: FrozenLexicalProfileName,
    selection_reasons: Vec<String>,
    dev_report_sha256: String,
    corpus_sha256: String,
    qrels_dev_sha256: String,
    active_tantivy_generation: i64,
}

#[derive(Debug, Clone, Serialize)]
struct LexicalProfileSelection {
    selected_profile: FrozenLexicalProfileName,
    reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct LexicalProfileAbReport {
    schema_version: &'static str,
    integrated_git_head: String,
    corpus_sha256: String,
    qrels_dev_sha256: String,
    active_tantivy_generation: i64,
    tantivy_schema_fingerprint: String,
    production_v1: RetrievalMetrics,
    metadata_boost_v1: RetrievalMetrics,
    selection: LexicalProfileSelection,
    redaction_passed: bool,
}

pub(super) fn lexical_profile_freeze_for_test() -> Value {
    serde_json::to_value(FrozenLexicalProfile {
        production_profile: "production_v1",
        comparison_candidate: "metadata_boost_v1",
        selected_profile: FrozenLexicalProfileName::ProductionV1,
        selection_reasons: vec!["weighted_ndcg_not_strictly_improved".to_string()],
        dev_report_sha256: "report-sha256".to_string(),
        corpus_sha256: "corpus-sha256".to_string(),
        qrels_dev_sha256: "dev-sha256".to_string(),
        active_tantivy_generation: 7,
    })
    .expect("frozen lexical profile serializes")
}

#[derive(Debug, Clone)]
struct LexicalSelectionSignals {
    weighted_ndcg_at_10: f64,
    exact_top_1: f64,
    hard_filter_violations: usize,
    get_round_trip: f64,
    stale_leakage: usize,
    comment_only: [f64; 4],
}

fn lexical_selection_signals(value: &Value) -> LexicalSelectionSignals {
    let comment_only = value["comment_only"]
        .as_array()
        .expect("comment-only metrics are an array")
        .iter()
        .map(|metric| metric.as_f64().expect("comment-only metric is numeric"))
        .collect::<Vec<_>>()
        .try_into()
        .expect("comment-only metrics contain four values");
    LexicalSelectionSignals {
        weighted_ndcg_at_10: value["weighted_ndcg_at_10"]
            .as_f64()
            .expect("weighted nDCG is numeric"),
        exact_top_1: value["exact_top_1"]
            .as_f64()
            .expect("exact top-1 is numeric"),
        hard_filter_violations: value["hard_filter_violations"]
            .as_u64()
            .expect("hard-filter violations are numeric") as usize,
        get_round_trip: value["get_round_trip"]
            .as_f64()
            .expect("round-trip rate is numeric"),
        stale_leakage: value["stale_leakage"]
            .as_u64()
            .expect("stale leakage is numeric") as usize,
        comment_only,
    }
}

fn lexical_selection_reasons(
    baseline: &LexicalSelectionSignals,
    candidate: &LexicalSelectionSignals,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if candidate.weighted_ndcg_at_10 <= baseline.weighted_ndcg_at_10 {
        reasons.push("weighted_ndcg_not_strictly_improved".to_string());
    }
    if candidate.exact_top_1 < baseline.exact_top_1 || candidate.exact_top_1 < 0.95 {
        reasons.push("exact_identifier_regression".to_string());
    }
    if candidate.hard_filter_violations > baseline.hard_filter_violations
        || candidate.hard_filter_violations != 0
    {
        reasons.push("hard_filter_regression".to_string());
    }
    if candidate.get_round_trip < baseline.get_round_trip || candidate.get_round_trip < 1.0 {
        reasons.push("query_get_round_trip_regression".to_string());
    }
    if candidate.stale_leakage > baseline.stale_leakage || candidate.stale_leakage != 0 {
        reasons.push("stale_leakage_regression".to_string());
    }
    if candidate
        .comment_only
        .iter()
        .zip(baseline.comment_only)
        .any(|(candidate, baseline)| *candidate < baseline)
    {
        reasons.push("comment_only_regression".to_string());
    }
    reasons
}

pub(super) fn lexical_profile_selection_for_test(baseline: Value, candidate: Value) -> Value {
    let baseline = lexical_selection_signals(&baseline);
    let candidate = lexical_selection_signals(&candidate);
    let reasons = lexical_selection_reasons(&baseline, &candidate);
    json!({
        "selected_profile": if reasons.is_empty() {
            "metadata_boost_v1"
        } else {
            "production_v1"
        },
        "reasons": reasons,
    })
}

fn select_lexical_profile(
    baseline: &RetrievalMetrics,
    candidate: &RetrievalMetrics,
) -> LexicalProfileSelection {
    let signals = |metrics: &RetrievalMetrics| LexicalSelectionSignals {
        weighted_ndcg_at_10: metrics.weighted_ndcg_at_10,
        exact_top_1: metrics.exact_top_1,
        hard_filter_violations: metrics.hard_filter_violations,
        get_round_trip: metrics.get_round_trip,
        stale_leakage: metrics.stale_leakage_live_fixture.unwrap_or_default(),
        comment_only: metrics
            .per_class
            .get(&QueryClass::CommentOnly)
            .map_or([0.0; 4], |class| {
                [
                    class.ndcg_at_10,
                    class.mrr_at_10,
                    class.recall_at_5,
                    class.recall_at_10,
                ]
            }),
    };
    let reasons = lexical_selection_reasons(&signals(baseline), &signals(candidate));
    LexicalProfileSelection {
        selected_profile: if reasons.is_empty() {
            FrozenLexicalProfileName::MetadataBoostV1
        } else {
            FrozenLexicalProfileName::ProductionV1
        },
        reasons,
    }
}

#[derive(Debug, Serialize)]
struct SnapshotFileDigest {
    relative_path: String,
    byte_size: u64,
    sha256: String,
}

struct FrozenRunGuard {
    repo_root: PathBuf,
    frozen_config_sha256: String,
    integrated_git_head: String,
    release_binary_sha256: String,
    contract_gate_bundle_sha256: String,
    model_preparation_provenance_sha256: String,
    candidate_states: Vec<FrozenCandidateState>,
    lexical_profile_report_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelPreparationProvenance {
    schema_version: String,
    prepared: Vec<PreparedModelProvenance>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedModelProvenance {
    candidate: String,
    model_id: String,
    resolved_revision: String,
    manifest_file: String,
    manifest_sha256: String,
    prepared_snapshot_sha256: String,
    snapshot_bytes: u64,
    download_transfer_bytes: u64,
    cache_source_bytes: u64,
    existing_snapshot_bytes: u64,
    artifact_acquisition: Vec<ArtifactAcquisitionProvenance>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactAcquisitionProvenance {
    relative_path: String,
    source: String,
    source_bytes: u64,
    download_transfer_bytes: u64,
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
    lexical_profile_dev: LexicalProfileAbReport,
    lexical_profile_heldout: RetrievalMetrics,
    candidates: Vec<CandidateReport>,
    selected_light_candidate: Option<String>,
    selected_quality_candidate: Option<String>,
    raw_query_or_body_logged: bool,
    redaction: RedactionEvidence,
    evaluation_state: String,
    promotion_eligible: bool,
    promotion_blockers: Vec<String>,
    host_protocol_failures: Vec<String>,
    contract_gate_bundle: VerifiedContractGateBundle,
}

#[derive(Debug, Serialize)]
struct SmokeReport {
    schema_version: &'static str,
    corpus_sha256: String,
    corpus_source_count: usize,
    database_schema_fingerprint: String,
    tantivy_schema_fingerprint: String,
    query_id: String,
    query_sha256: String,
    ranked_source_count: usize,
    get_round_trip: f64,
    raw_query_or_body_logged: bool,
}

#[derive(Debug, Serialize)]
struct ContextProbeSmokeReport {
    schema_version: &'static str,
    corpus_source_count: usize,
    manifest_hash: String,
    candidate_database_schema_fingerprint: String,
    candidate_tantivy_schema_fingerprint: String,
    context_contract: ContextContractEvidence,
    evaluation_state: &'static str,
    raw_query_or_body_logged: bool,
    redaction: RedactionEvidence,
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

#[derive(Clone)]
struct QueryEvidence {
    rankings: BTreeMap<String, Vec<String>>,
    branch_observations: BTreeMap<String, Vec<BranchObservation>>,
    get_total: usize,
    get_success: usize,
    stale_failures: usize,
    hybrid_required: bool,
    hybrid_expected_queries: usize,
    hybrid_path_queries: usize,
}

struct DevRunEvidence {
    primary: QueryEvidence,
    diagnostic: QueryEvidence,
    peak_rss_bytes: u64,
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

fn context_input(
    entity_type: &str,
    host: &str,
    repo: &str,
    issue_number: u64,
    title: &str,
    chunk: &str,
) -> String {
    let Ok(issue_number) = i64::try_from(issue_number) else {
        return String::new();
    };
    let repository = format!("{host}/{repo}");
    match entity_type {
        "issue" => prepare_embedding_input(
            EmbeddingSourceContext::Issue {
                repository: &repository,
                issue_number,
                title,
            },
            chunk,
        )
        .as_str()
        .to_string(),
        "issue_comment" => prepare_embedding_input(
            EmbeddingSourceContext::Comment {
                repository: &repository,
                parent_issue_number: issue_number,
                parent_issue_title: title,
            },
            chunk,
        )
        .as_str()
        .to_string(),
        _ => String::new(),
    }
}

fn probe_context_contract(
    db_path: &Path,
    manifest_template_version: &str,
) -> Result<ContextContractEvidence, DynError> {
    let connection = Connection::open(db_path)?;
    let (generation_id, model_manifest_hash, generation_template_version): (i64, String, String) =
        connection.query_row(
            "SELECT eg.id, eg.model_manifest_hash, eg.context_template_version
         FROM retrieval_publication_pointer pointer
         JOIN retrieval_publications publication
           ON publication.publication_id = pointer.publication_id
         JOIN embedding_generations eg
           ON eg.id = publication.embedding_generation_id
         WHERE pointer.id = 1
           AND publication.active = 1
           AND eg.state IN ('ready', 'active')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    let mut statement = connection.prepare(
        "SELECT egc.context_hash, c.chunker_fingerprint, c.body,
                se.entity_type, se.host, se.repo,
                coalesce(im.issue_number, cm.issue_number),
                CASE WHEN se.entity_type = 'issue' THEN im.title
                     ELSE cm.parent_issue_title END
         FROM embedding_generation_chunks egc
         JOIN chunks c ON c.id = egc.chunk_id
         JOIN source_entities se ON se.source_id = c.source_id
         LEFT JOIN issue_metadata im ON im.source_id = c.source_id
         LEFT JOIN comment_metadata cm ON cm.source_id = c.source_id
         WHERE egc.generation_id = ?1
         ORDER BY se.entity_type, c.id",
    )?;
    let rows = statement.query_map(params![generation_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)? as u64,
            row.get::<_, String>(7)?,
        ))
    })?;
    let mut issue_rows_checked = 0usize;
    let mut comment_rows_checked = 0usize;
    let mut context_hash_mismatches = 0usize;
    for row in rows {
        let (
            stored_context_hash,
            chunker_fingerprint,
            chunk,
            entity_type,
            host,
            repo,
            issue_number,
            title,
        ) = row?;
        issue_rows_checked += usize::from(entity_type == "issue");
        comment_rows_checked += usize::from(entity_type == "issue_comment");
        let embedding_input =
            context_input(&entity_type, &host, &repo, issue_number, &title, &chunk);
        let expected_context_hash = embedding_context_hash(
            &model_manifest_hash,
            &chunker_fingerprint,
            &generation_template_version,
            &embedding_input,
        );
        context_hash_mismatches +=
            usize::from(embedding_input.is_empty() || stored_context_hash != expected_context_hash);
    }
    let passed = manifest_template_version == METADATA_CONTEXT_TEMPLATE_VERSION
        && generation_template_version == METADATA_CONTEXT_TEMPLATE_VERSION
        && issue_rows_checked > 0
        && comment_rows_checked > 0
        && context_hash_mismatches == 0;
    Ok(ContextContractEvidence {
        required_template_version: METADATA_CONTEXT_TEMPLATE_VERSION,
        manifest_template_version: manifest_template_version.to_string(),
        generation_template_version,
        issue_rows_checked,
        comment_rows_checked,
        context_hash_mismatches,
        passed,
    })
}

pub(super) fn context_input_for_test(
    entity_type: &str,
    host: &str,
    repo: &str,
    issue_number: u64,
    title: &str,
    chunk: &str,
) -> String {
    context_input(entity_type, host, repo, issue_number, title, chunk)
}

pub(super) fn context_contract_for_test(
    db_path: &Path,
    manifest_template_version: &str,
) -> Result<Value, DynError> {
    Ok(serde_json::to_value(probe_context_contract(
        db_path,
        manifest_template_version,
    )?)?)
}

pub(super) fn select_tier_for_test(
    candidates: &[(&str, u64, f64, f64, bool)],
    light: bool,
) -> Option<String> {
    let mut eligible = candidates
        .iter()
        .filter(|candidate| candidate.4)
        .collect::<Vec<_>>();
    let best_ndcg = eligible
        .iter()
        .map(|candidate| candidate.2)
        .reduce(f64::max)?;
    if light {
        eligible.retain(|candidate| best_ndcg - candidate.2 <= 0.02 + f64::EPSILON);
        eligible.sort_by_key(|candidate| (candidate.1, candidate.0));
    } else {
        eligible.retain(|candidate| best_ndcg - candidate.2 <= 0.005 + f64::EPSILON);
        eligible.sort_by(|left, right| {
            right
                .3
                .partial_cmp(&left.3)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.0.cmp(right.0))
        });
    }
    eligible.first().map(|candidate| candidate.0.to_string())
}

pub(super) fn hybrid_gate_for_test(expected: usize, observed: usize) -> bool {
    expected == observed
}

fn weighted_score(values: [f64; 6]) -> f64 {
    0.50 * values[0]
        + 0.20 * values[1]
        + 0.15 * values[2]
        + 0.10 * values[3]
        + 0.025 * values[4]
        + 0.025 * values[5]
}

pub(super) fn weighted_score_for_test(values: [f64; 6]) -> f64 {
    weighted_score(values)
}

pub(super) struct HardFilterProbeEvidence {
    pub(super) active_competing_sources: usize,
    pub(super) bm25_filtered_queries: usize,
    pub(super) hybrid_filtered_queries: usize,
    pub(super) hybrid_ranked_results: usize,
    pub(super) hybrid_results_with_both_branches: usize,
    pub(super) exact_issue_queries: usize,
}

#[derive(Clone, Copy)]
enum ExpectedRankingPath {
    Bm25,
    Hybrid,
    Exact,
}

#[derive(Default)]
struct HardFilterQuerySetEvidence {
    query_count: usize,
    ranked_results: usize,
    results_with_both_branches: usize,
}

pub(super) fn run_hard_filter_contract_probe(
    binary: &Path,
    corpus_raw: &str,
) -> Result<HardFilterProbeEvidence, DynError> {
    let corpus = parse_jsonl::<CorpusRecord>(corpus_raw);
    let issue_seed = corpus
        .iter()
        .find(|source| source.entity_type == "issue")
        .ok_or("hard-filter issue seed missing")?;
    let comment_seed = corpus
        .iter()
        .find(|source| source.entity_type == "issue_comment")
        .ok_or("hard-filter comment seed missing")?;

    let issue = |repo: &str, issue_number: u64, node_id: &str| {
        let mut source = issue_seed.clone();
        source.source_id = format!("qgh://github.com/issue/{node_id}");
        source.entity_type = "issue".to_string();
        source.repo = repo.to_string();
        source.issue_number = issue_number;
        source.canonical_url = format!("https://github.com/{repo}/issues/{issue_number}");
        source.title = format!("Filter probe {node_id}");
        source.body = format!("{FILTER_PROBE_SENTINEL} {node_id}");
        source.body_sha256 = digest_hex(&source.body);
        source
    };
    let mut competing_comment = comment_seed.clone();
    competing_comment.source_id =
        "qgh://github.com/issue-comment/IC_FILTER_TARGET_COMMENT".to_string();
    competing_comment.entity_type = "issue_comment".to_string();
    competing_comment.repo = FILTER_PROBE_TARGET_REPO.to_string();
    competing_comment.issue_number = FILTER_PROBE_TARGET_ISSUE;
    competing_comment.canonical_url = format!(
        "https://github.com/{FILTER_PROBE_TARGET_REPO}/issues/{FILTER_PROBE_TARGET_ISSUE}#issuecomment-9900001"
    );
    competing_comment.title = "Comment on filter probe target".to_string();
    competing_comment.body = format!("{FILTER_PROBE_SENTINEL} IC_FILTER_TARGET_COMMENT");
    competing_comment.body_sha256 = digest_hex(&competing_comment.body);

    let sources = vec![
        issue(
            FILTER_PROBE_TARGET_REPO,
            FILTER_PROBE_TARGET_ISSUE,
            "I_FILTER_TARGET",
        ),
        issue(
            FILTER_PROBE_TARGET_REPO,
            FILTER_PROBE_TARGET_ISSUE + 1,
            "I_FILTER_WRONG_LABEL",
        ),
        issue(
            FILTER_PROBE_TARGET_REPO,
            FILTER_PROBE_TARGET_ISSUE + 2,
            "I_FILTER_WRONG_STATE",
        ),
        issue(
            FILTER_PROBE_TARGET_REPO,
            FILTER_PROBE_TARGET_ISSUE + 3,
            "I_FILTER_WRONG_AUTHOR",
        ),
        issue(
            FILTER_PROBE_TARGET_REPO,
            FILTER_PROBE_TARGET_ISSUE + 4,
            "I_FILTER_OTHER_ISSUE",
        ),
        issue(
            FILTER_PROBE_COMPETING_REPO,
            FILTER_PROBE_TARGET_ISSUE,
            "I_FILTER_OTHER_REPO",
        ),
        competing_comment,
    ];
    let metadata = |state: &str, author: &str, labels: &[&str]| IssueApiMetadata {
        state: state.to_string(),
        author: author.to_string(),
        labels: labels.iter().map(|label| (*label).to_string()).collect(),
    };
    let issue_metadata = BTreeMap::from([
        (
            "qgh://github.com/issue/I_FILTER_TARGET".to_string(),
            metadata("open", "target-author", &["target-label"]),
        ),
        (
            "qgh://github.com/issue/I_FILTER_WRONG_LABEL".to_string(),
            metadata("open", "target-author", &["wrong-label"]),
        ),
        (
            "qgh://github.com/issue/I_FILTER_WRONG_STATE".to_string(),
            metadata("closed", "target-author", &["target-label"]),
        ),
        (
            "qgh://github.com/issue/I_FILTER_WRONG_AUTHOR".to_string(),
            metadata("open", "wrong-author", &["target-label"]),
        ),
        (
            "qgh://github.com/issue/I_FILTER_OTHER_ISSUE".to_string(),
            metadata("open", "target-author", &["target-label"]),
        ),
        (
            "qgh://github.com/issue/I_FILTER_OTHER_REPO".to_string(),
            metadata("open", "target-author", &["target-label"]),
        ),
    ]);
    let server = PublicSnapshotServer::start_with_issue_metadata(&sources, &issue_metadata)?;
    let root = PathBuf::from("target/qgh-eval/hard-filter-contract");
    ensure_target_root(&root)?;
    let fixture = CliFixture::new(root, binary.to_path_buf(), server.base_url.clone())?;
    fixture.init_git_worktree()?;
    fixture.write_hard_filter_probe_config(
        FILTER_PROBE_TARGET_REPO,
        FILTER_PROBE_COMPETING_REPO,
        false,
    )?;
    fixture.sync()?;

    let connection = Connection::open(fixture.db_path())?;
    let source_count: usize = connection.query_row(
        "SELECT COUNT(*) FROM source_entities WHERE lifecycle_state = 'active'",
        [],
        |row| row.get(0),
    )?;
    if source_count != sources.len() {
        return Err(format!(
            "hard-filter probe expected {} competing sources, found {source_count}",
            sources.len()
        )
        .into());
    }
    drop(connection);

    let bm25 = run_hard_filter_query_set(&fixture, None, ExpectedRankingPath::Bm25)?;

    fixture.write_hard_filter_probe_config(
        FILTER_PROBE_TARGET_REPO,
        FILTER_PROBE_COMPETING_REPO,
        true,
    )?;
    let query_vectors_json = serde_json::to_string(&BTreeMap::from([
        ("prepare vector schema", vec![0.0_f32, 1.0, 0.0, 0.0]),
        (FILTER_PROBE_SENTINEL, vec![1.0_f32, 0.0, 0.0, 0.0]),
    ]))?;
    let _ = fixture.qgh_with_test_vectors(
        &[
            "query",
            "prepare vector schema",
            "--repo",
            FILTER_PROBE_TARGET_REPO,
            "--json",
        ],
        Some(&query_vectors_json),
        None,
    )?;
    fixture.seed_hard_filter_chunks(&sources)?;
    let document_vectors_json = hard_filter_document_vectors_json(&sources)?;
    let embed = fixture.qgh_with_test_vectors(
        &["embed", "--force", "--json"],
        None,
        Some(&document_vectors_json),
    )?;
    let embed: Value = serde_json::from_slice(&embed.stdout)?;
    if embed["data"]["embedding_state"].as_str() != Some("refreshed")
        || embed["data"]["chunks"]["embedded"].as_u64() != Some(sources.len() as u64)
    {
        return Err("deterministic local embed did not publish every filter source".into());
    }
    let hybrid = run_hard_filter_query_set(
        &fixture,
        Some(&query_vectors_json),
        ExpectedRankingPath::Hybrid,
    )?;
    let exact_issue_queries = run_exact_issue_filter_queries(&fixture, &query_vectors_json)?;

    Ok(HardFilterProbeEvidence {
        active_competing_sources: source_count,
        bm25_filtered_queries: bm25.query_count,
        hybrid_filtered_queries: hybrid.query_count,
        hybrid_ranked_results: hybrid.ranked_results,
        hybrid_results_with_both_branches: hybrid.results_with_both_branches,
        exact_issue_queries,
    })
}

fn hard_filter_document_vectors_json(sources: &[CorpusRecord]) -> Result<String, DynError> {
    let issue_titles = sources
        .iter()
        .filter(|source| source.entity_type == "issue")
        .map(|source| {
            (
                (source.repo.as_str(), source.issue_number),
                source.title.as_str(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let document_vectors = sources
        .iter()
        .map(|source| -> Result<(String, Vec<f32>), DynError> {
            let vector = match source.source_id.as_str() {
                "qgh://github.com/issue/I_FILTER_TARGET" => vec![0.8_f32, 0.2, 0.0, 0.0],
                "qgh://github.com/issue/I_FILTER_OTHER_ISSUE" => {
                    vec![0.7_f32, 0.3, 0.0, 0.0]
                }
                _ => vec![1.0_f32, 0.0, 0.0, 0.0],
            };
            let repository = format!("github.com/{}", source.repo);
            let issue_number = i64::try_from(source.issue_number)?;
            let chunk = hard_filter_chunk_body(&source.source_id);
            let context = match source.entity_type.as_str() {
                "issue" => EmbeddingSourceContext::Issue {
                    repository: &repository,
                    issue_number,
                    title: &source.title,
                },
                "issue_comment" => EmbeddingSourceContext::Comment {
                    repository: &repository,
                    parent_issue_number: issue_number,
                    parent_issue_title: issue_titles
                        .get(&(source.repo.as_str(), source.issue_number))
                        .copied()
                        .ok_or("hard-filter comment parent title missing")?,
                },
                _ => return Err("hard-filter source entity type unsupported".into()),
            };
            Ok((
                prepare_embedding_input(context, &chunk)
                    .as_str()
                    .to_string(),
                vector,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, DynError>>()?;
    Ok(serde_json::to_string(&document_vectors)?)
}

fn hard_filter_chunk_body(source_id: &str) -> String {
    format!("hard-filter vector chunk for {source_id}")
}

fn run_hard_filter_query_set(
    fixture: &CliFixture,
    query_vectors_json: Option<&str>,
    expected_ranking: ExpectedRankingPath,
) -> Result<HardFilterQuerySetEvidence, DynError> {
    let mut evidence = HardFilterQuerySetEvidence::default();
    for result in [
        assert_hard_filter_results(
            fixture,
            &[
                "query",
                FILTER_PROBE_SENTINEL,
                "--repo",
                FILTER_PROBE_TARGET_REPO,
                "--label",
                "target-label",
                "--limit",
                "10",
                "--json",
            ],
            &[
                (
                    "qgh://github.com/issue/I_FILTER_TARGET",
                    FILTER_PROBE_TARGET_ISSUE,
                ),
                (
                    "qgh://github.com/issue/I_FILTER_WRONG_STATE",
                    FILTER_PROBE_TARGET_ISSUE + 2,
                ),
                (
                    "qgh://github.com/issue/I_FILTER_WRONG_AUTHOR",
                    FILTER_PROBE_TARGET_ISSUE + 3,
                ),
                (
                    "qgh://github.com/issue/I_FILTER_OTHER_ISSUE",
                    FILTER_PROBE_TARGET_ISSUE + 4,
                ),
            ],
            query_vectors_json,
            expected_ranking,
        ),
        assert_hard_filter_results(
            fixture,
            &[
                "query",
                FILTER_PROBE_SENTINEL,
                "--repo",
                FILTER_PROBE_TARGET_REPO,
                "--state",
                "open",
                "--limit",
                "10",
                "--json",
            ],
            &[
                (
                    "qgh://github.com/issue/I_FILTER_TARGET",
                    FILTER_PROBE_TARGET_ISSUE,
                ),
                (
                    "qgh://github.com/issue/I_FILTER_WRONG_LABEL",
                    FILTER_PROBE_TARGET_ISSUE + 1,
                ),
                (
                    "qgh://github.com/issue/I_FILTER_WRONG_AUTHOR",
                    FILTER_PROBE_TARGET_ISSUE + 3,
                ),
                (
                    "qgh://github.com/issue/I_FILTER_OTHER_ISSUE",
                    FILTER_PROBE_TARGET_ISSUE + 4,
                ),
            ],
            query_vectors_json,
            expected_ranking,
        ),
        assert_hard_filter_results(
            fixture,
            &[
                "query",
                FILTER_PROBE_SENTINEL,
                "--repo",
                FILTER_PROBE_TARGET_REPO,
                "--author",
                "target-author",
                "--limit",
                "10",
                "--json",
            ],
            &[
                (
                    "qgh://github.com/issue/I_FILTER_TARGET",
                    FILTER_PROBE_TARGET_ISSUE,
                ),
                (
                    "qgh://github.com/issue/I_FILTER_WRONG_LABEL",
                    FILTER_PROBE_TARGET_ISSUE + 1,
                ),
                (
                    "qgh://github.com/issue/I_FILTER_WRONG_STATE",
                    FILTER_PROBE_TARGET_ISSUE + 2,
                ),
                (
                    "qgh://github.com/issue/I_FILTER_OTHER_ISSUE",
                    FILTER_PROBE_TARGET_ISSUE + 4,
                ),
            ],
            query_vectors_json,
            expected_ranking,
        ),
        assert_hard_filter_results(
            fixture,
            &[
                "query",
                FILTER_PROBE_SENTINEL,
                "--repo",
                FILTER_PROBE_TARGET_REPO,
                "--limit",
                "10",
                "--json",
            ],
            &[
                (
                    "qgh://github.com/issue/I_FILTER_TARGET",
                    FILTER_PROBE_TARGET_ISSUE,
                ),
                (
                    "qgh://github.com/issue/I_FILTER_WRONG_LABEL",
                    FILTER_PROBE_TARGET_ISSUE + 1,
                ),
                (
                    "qgh://github.com/issue/I_FILTER_WRONG_STATE",
                    FILTER_PROBE_TARGET_ISSUE + 2,
                ),
                (
                    "qgh://github.com/issue/I_FILTER_WRONG_AUTHOR",
                    FILTER_PROBE_TARGET_ISSUE + 3,
                ),
                (
                    "qgh://github.com/issue/I_FILTER_OTHER_ISSUE",
                    FILTER_PROBE_TARGET_ISSUE + 4,
                ),
            ],
            query_vectors_json,
            expected_ranking,
        ),
    ] {
        let result = result?;
        evidence.query_count += 1;
        evidence.ranked_results += result.ranked_results;
        evidence.results_with_both_branches += result.results_with_both_branches;
    }
    Ok(evidence)
}

fn run_exact_issue_filter_queries(
    fixture: &CliFixture,
    query_vectors_json: &str,
) -> Result<usize, DynError> {
    let expected = [(
        "qgh://github.com/issue/I_FILTER_TARGET",
        FILTER_PROBE_TARGET_ISSUE,
    )];
    let mut query_count = 0usize;
    for result in [
        assert_hard_filter_results(
            fixture,
            &[
                "query",
                FILTER_PROBE_SENTINEL,
                "--repo",
                FILTER_PROBE_TARGET_REPO,
                "--issue",
                "900001",
                "--limit",
                "10",
                "--json",
            ],
            &expected,
            Some(query_vectors_json),
            ExpectedRankingPath::Exact,
        ),
        assert_hard_filter_results(
            fixture,
            &[
                "query",
                FILTER_PROBE_SENTINEL,
                "--repo",
                FILTER_PROBE_TARGET_REPO,
                "--label",
                "target-label",
                "--state",
                "open",
                "--author",
                "target-author",
                "--issue",
                "900001",
                "--limit",
                "10",
                "--json",
            ],
            &expected,
            Some(query_vectors_json),
            ExpectedRankingPath::Exact,
        ),
    ] {
        let result = result?;
        if result.ranked_results != 1 || result.results_with_both_branches != 0 {
            return Err("issue filter did not stay on the exact locator path".into());
        }
        query_count += 1;
    }
    Ok(query_count)
}

fn assert_hard_filter_results(
    fixture: &CliFixture,
    arguments: &[&str],
    expected: &[(&str, u64)],
    query_vectors_json: Option<&str>,
    expected_ranking: ExpectedRankingPath,
) -> Result<HardFilterQuerySetEvidence, DynError> {
    let output = fixture.qgh_with_test_vectors(arguments, query_vectors_json, None)?;
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    let results = envelope["data"]["results"]
        .as_array()
        .ok_or("hard-filter result array missing")?;
    let mut observed = BTreeMap::new();
    for result in results {
        let source_id = result["source_id"]
            .as_str()
            .ok_or("hard-filter result source_id missing")?;
        let repo = result["repo"]
            .as_str()
            .ok_or("hard-filter result repo missing")?;
        let entity_type = result["entity_type"]
            .as_str()
            .ok_or("hard-filter result entity_type missing")?;
        let issue_number = result["issue_number"]
            .as_u64()
            .ok_or("hard-filter result issue_number missing")?;
        if repo != FILTER_PROBE_TARGET_REPO || entity_type != "issue" {
            return Err(format!(
                "repo/source-type filter admitted source={source_id} repo={repo} entity_type={entity_type} arguments={arguments:?}"
            )
            .into());
        }
        let ranking = &result["ranking"];
        let ranking_kind = ranking["kind"]
            .as_str()
            .ok_or("hard-filter result ranking.kind missing")?;
        match expected_ranking {
            ExpectedRankingPath::Bm25 => {
                if ranking_kind != "bm25" || ranking["lexical_score"].as_f64().is_none() {
                    return Err(format!(
                        "BM25 filter probe did not use the lexical candidate path: kind={ranking_kind}"
                    )
                    .into());
                }
            }
            ExpectedRankingPath::Hybrid => {
                if ranking_kind != "hybrid"
                    || ranking["lexical_score"].as_f64().is_none()
                    || ranking["vector_distance"].as_f64().is_none()
                {
                    return Err("hybrid filter probe did not use both candidate generators".into());
                }
            }
            ExpectedRankingPath::Exact => {
                if ranking_kind != "exact"
                    || !ranking["lexical_score"].is_null()
                    || !ranking["vector_distance"].is_null()
                {
                    return Err("issue filter did not use the exact locator path".into());
                }
            }
        }
        observed.insert(source_id.to_string(), issue_number);
    }
    let expected = expected
        .iter()
        .map(|(source_id, issue_number)| ((*source_id).to_string(), *issue_number))
        .collect::<BTreeMap<_, _>>();
    if observed != expected {
        return Err(format!(
            "hard-filter result mismatch: observed {observed:?}, expected {expected:?}"
        )
        .into());
    }
    Ok(HardFilterQuerySetEvidence {
        query_count: 1,
        ranked_results: results.len(),
        results_with_both_branches: results
            .iter()
            .filter(|result| {
                result["ranking"]["kind"].as_str() == Some("hybrid")
                    && result["ranking"]["lexical_score"].as_f64().is_some()
                    && result["ranking"]["vector_distance"].as_f64().is_some()
            })
            .count(),
    })
}

fn target_root_allowed(cwd: &Path, root: &Path) -> bool {
    if root
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
    }
    let allowed = cwd.join("target/qgh-eval");
    let candidate = if root.is_absolute() {
        root.to_path_buf()
    } else {
        cwd.join(root)
    };
    candidate.starts_with(allowed)
}

pub(super) fn target_root_allowed_for_test(cwd: &Path, root: &Path) -> bool {
    target_root_allowed(cwd, root)
}

fn reset_stderr_audit() {
    STDERR_AUDIT
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("stderr audit lock")
        .clear();
}

fn record_stderr(stderr: &[u8]) {
    STDERR_AUDIT
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("stderr audit lock")
        .push(stderr.to_vec());
}

fn verify_redaction(
    root: &Path,
    corpus: &[CorpusRecord],
    qrels: &[&QrelRecord],
) -> Result<RedactionEvidence, DynError> {
    let stderr = STDERR_AUDIT
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .map_err(|_| "stderr audit lock poisoned")?;
    let mut violation_artifacts = Vec::new();
    for (index, stream) in stderr.iter().enumerate() {
        if contains_sensitive_payload(stream, corpus, qrels) {
            violation_artifacts.push(format!("stderr-stream-{index}"));
        }
    }
    let mut artifact_files = Vec::new();
    collect_redaction_artifacts(root, &mut artifact_files)?;
    artifact_files.sort();
    for path in &artifact_files {
        let bytes = fs::read(path)?;
        if contains_sensitive_payload(&bytes, corpus, qrels) {
            let relative = path.strip_prefix(root).unwrap_or(path);
            violation_artifacts.push(relative.to_string_lossy().to_string());
        }
    }
    Ok(RedactionEvidence {
        stderr_streams_checked: stderr.len(),
        artifact_files_checked: artifact_files.len(),
        passed: violation_artifacts.is_empty(),
        violation_artifacts,
    })
}

fn contains_sensitive_payload(
    bytes: &[u8],
    corpus: &[CorpusRecord],
    qrels: &[&QrelRecord],
) -> bool {
    let rendered = String::from_utf8_lossy(bytes);
    corpus
        .iter()
        .map(|source| source.body.as_str())
        .chain(qrels.iter().map(|qrel| qrel.query.as_str()))
        .filter(|value| !value.is_empty())
        .any(|value| rendered.contains(value))
}

fn collect_redaction_artifacts(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), DynError> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_redaction_artifacts(&path, files)?;
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let extension = path.extension().and_then(|extension| extension.to_str());
        if matches!(extension, Some("json" | "jsonl" | "partial" | "fragment"))
            || name.contains("events")
            || name.contains("report")
            || name.contains("canary")
        {
            files.push(path);
        }
    }
    Ok(())
}

pub(super) fn redaction_file_scan_for_test(
    root: &Path,
    sensitive: &str,
) -> Result<Value, DynError> {
    let mut files = Vec::new();
    collect_redaction_artifacts(root, &mut files)?;
    files.sort();
    let violation_artifacts = files
        .iter()
        .filter_map(|path| {
            let bytes = fs::read(path).ok()?;
            String::from_utf8_lossy(&bytes)
                .contains(sensitive)
                .then(|| {
                    path.strip_prefix(root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .to_string()
                })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::to_value(RedactionEvidence {
        stderr_streams_checked: 0,
        artifact_files_checked: files.len(),
        passed: violation_artifacts.is_empty(),
        violation_artifacts,
    })?)
}

struct WarmEvidence {
    held_out: QueryEvidence,
    diagnostic: QueryEvidence,
    latencies_ms: Vec<f64>,
    peak_rss_bytes: u64,
}

struct PreparedCandidate {
    candidate: String,
    model_id: String,
    revision: String,
    manifest_hash: String,
    candidate_database_schema_fingerprint: String,
    candidate_tantivy_schema_fingerprint: String,
    context_contract: ContextContractEvidence,
    fixture: CliFixture,
    manifest_path: PathBuf,
    snapshot_bytes: u64,
    download_transfer_bytes: Option<u64>,
    chunk_count: usize,
    embed_seconds: f64,
    db_growth_bytes_per_chunk: f64,
    cold_samples_ms: Vec<f64>,
    isolated_peak_rss: u64,
    dev_metrics: RetrievalMetrics,
    offline_dev_diagnostics: Vec<OfflineFusionDiagnostic>,
}

fn frozen_candidate_state(
    root: &Path,
    state: &Result<PreparedCandidate, Box<CandidateReport>>,
) -> Result<FrozenCandidateState, DynError> {
    match state {
        Ok(prepared) => frozen_prepared_candidate_state(root, prepared),
        Err(report) => frozen_blocked_candidate_state(root, report),
    }
}

fn frozen_prepared_candidate_state(
    root: &Path,
    prepared: &PreparedCandidate,
) -> Result<FrozenCandidateState, DynError> {
    let manifest_bytes = fs::read(&prepared.manifest_path)?;
    let manifest = ModelManifestV1::from_json_slice(&manifest_bytes)?;
    let snapshot_root = prepared
        .manifest_path
        .parent()
        .ok_or("prepared candidate manifest parent missing")?;
    let snapshot = prepared_snapshot_digest(snapshot_root)?;
    let relative = prepared
        .manifest_path
        .strip_prefix(root)?
        .to_str()
        .ok_or("prepared candidate manifest path is not UTF-8")?
        .replace(std::path::MAIN_SEPARATOR, "/");
    Ok(FrozenCandidateState {
        candidate: prepared.candidate.clone(),
        model_id: prepared.model_id.clone(),
        resolved_revision: prepared.revision.clone(),
        dev_state: "prepared".to_string(),
        blocker_code: None,
        dev_metrics_sha256: Some(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&prepared.dev_metrics)?)
        )),
        offline_dev_diagnostics_sha256: Some(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&prepared.offline_dev_diagnostics)?)
        )),
        manifest_relative_path: Some(relative),
        manifest_hash: Some(manifest.hash()),
        manifest_file_sha256: Some(format!("{:x}", Sha256::digest(&manifest_bytes))),
        artifact_set_sha256: Some(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&manifest.artifacts)?)
        )),
        prepared_snapshot_sha256: Some(snapshot.sha256),
        prepared_snapshot_bytes: Some(snapshot.bytes),
        prepared_snapshot_file_count: Some(snapshot.file_count),
        download_transfer_bytes: prepared.download_transfer_bytes,
    })
}

fn frozen_blocked_candidate_state(
    root: &Path,
    report: &CandidateReport,
) -> Result<FrozenCandidateState, DynError> {
    let dev_metrics_sha256 = report
        .dev_metrics
        .as_ref()
        .map(|metrics| {
            serde_json::to_vec(metrics).map(|bytes| format!("{:x}", Sha256::digest(bytes)))
        })
        .transpose()?;
    let offline_dev_diagnostics_sha256 = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&report.offline_dev_diagnostics)?)
    );
    let mut frozen = FrozenCandidateState {
        candidate: report.candidate.clone(),
        model_id: report.model_id.clone(),
        resolved_revision: report.resolved_revision.clone(),
        dev_state: report.status.clone(),
        blocker_code: report.blocker.as_ref().map(|blocker| blocker.code.clone()),
        dev_metrics_sha256,
        offline_dev_diagnostics_sha256: Some(offline_dev_diagnostics_sha256),
        manifest_relative_path: None,
        manifest_hash: report.manifest_hash.clone(),
        manifest_file_sha256: None,
        artifact_set_sha256: None,
        prepared_snapshot_sha256: None,
        prepared_snapshot_bytes: None,
        prepared_snapshot_file_count: None,
        download_transfer_bytes: None,
    };
    let manifest_path = root
        .join("models")
        .join(&report.candidate)
        .join("manifest.json");
    if manifest_path.is_file() {
        let manifest_bytes = fs::read(&manifest_path)?;
        let manifest = ModelManifestV1::from_json_slice(&manifest_bytes)?;
        let snapshot = prepared_snapshot_digest(
            manifest_path
                .parent()
                .ok_or("prepared candidate manifest parent missing")?,
        )?;
        frozen.manifest_relative_path = Some(
            manifest_path
                .strip_prefix(root)?
                .to_str()
                .ok_or("prepared candidate manifest path is not UTF-8")?
                .replace(std::path::MAIN_SEPARATOR, "/"),
        );
        frozen.manifest_hash = Some(manifest.hash());
        frozen.manifest_file_sha256 = Some(format!("{:x}", Sha256::digest(&manifest_bytes)));
        frozen.artifact_set_sha256 = Some(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&manifest.artifacts)?)
        ));
        frozen.prepared_snapshot_sha256 = Some(snapshot.sha256);
        frozen.prepared_snapshot_bytes = Some(snapshot.bytes);
        frozen.prepared_snapshot_file_count = Some(snapshot.file_count);
        frozen.download_transfer_bytes = Some(prepared_model_download_bytes(
            root,
            &report.candidate,
            &report.model_id,
            &report.resolved_revision,
            &manifest_path,
        )?);
    }
    Ok(frozen)
}

fn prepared_model_download_bytes(
    root: &Path,
    candidate: &str,
    model_id: &str,
    revision: &str,
    manifest_path: &Path,
) -> Result<u64, DynError> {
    let provenance: ModelPreparationProvenance =
        serde_json::from_slice(&fs::read(model_preparation_provenance_path(root)?)?)?;
    if provenance.schema_version != "qgh.live_model_preparation.v1" {
        return Err("model preparation provenance schema is invalid".into());
    }
    let record = provenance
        .prepared
        .iter()
        .find(|record| record.candidate == candidate)
        .ok_or("model preparation candidate provenance is missing")?;
    let relative_manifest = manifest_path
        .strip_prefix(root.join("models"))?
        .to_str()
        .ok_or("model preparation manifest path is not UTF-8")?
        .replace(std::path::MAIN_SEPARATOR, "/");
    let manifest = ModelManifestV1::from_json_slice(&fs::read(manifest_path)?)?;
    let snapshot = prepared_snapshot_digest(
        manifest_path
            .parent()
            .ok_or("prepared candidate manifest parent missing")?,
    )?;
    let artifact_paths = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.relative_path.as_str())
        .collect::<BTreeSet<_>>();
    let recorded_paths = record
        .artifact_acquisition
        .iter()
        .map(|artifact| artifact.relative_path.as_str())
        .collect::<BTreeSet<_>>();
    let recorded_download: u64 = record
        .artifact_acquisition
        .iter()
        .map(|artifact| artifact.download_transfer_bytes)
        .sum();
    let recorded_cache: u64 = record
        .artifact_acquisition
        .iter()
        .filter(|artifact| artifact.source == "local_cache")
        .map(|artifact| artifact.source_bytes)
        .sum();
    let recorded_existing: u64 = record
        .artifact_acquisition
        .iter()
        .filter(|artifact| artifact.source == "existing_snapshot")
        .map(|artifact| artifact.source_bytes)
        .sum();
    if record.model_id != model_id
        || record.resolved_revision != revision
        || record.manifest_file != relative_manifest
        || record.manifest_sha256 != file_sha256(manifest_path)?
        || record.prepared_snapshot_sha256 != snapshot.sha256
        || record.snapshot_bytes != snapshot.bytes
        || record.artifact_acquisition.len() != manifest.artifacts.len()
        || artifact_paths != recorded_paths
        || record.artifact_acquisition.iter().any(|artifact| {
            artifact.source_bytes == 0
                || !matches!(
                    artifact.source.as_str(),
                    "curl" | "local_cache" | "existing_snapshot"
                )
                || (artifact.source == "curl" && artifact.download_transfer_bytes == 0)
                || (artifact.source != "curl" && artifact.download_transfer_bytes != 0)
        })
        || record.download_transfer_bytes != recorded_download
        || record.cache_source_bytes != recorded_cache
        || record.existing_snapshot_bytes != recorded_existing
    {
        return Err("model preparation provenance failed verification".into());
    }
    Ok(record.download_transfer_bytes)
}

fn model_preparation_provenance_path(root: &Path) -> Result<PathBuf, DynError> {
    let models = root.join("models");
    let models_metadata =
        fs::symlink_metadata(&models).map_err(|_| "model preparation provenance is unavailable")?;
    if models_metadata.file_type().is_symlink() || !models_metadata.is_dir() {
        return Err("model preparation directory must be a regular directory".into());
    }
    let path = models.join("preparation-provenance.json");
    let metadata =
        fs::symlink_metadata(&path).map_err(|_| "model preparation provenance is unavailable")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("model preparation provenance must be a regular file".into());
    }
    Ok(path)
}

pub(super) fn prepared_model_download_bytes_for_test(
    root: &Path,
    candidate: &str,
    model_id: &str,
    revision: &str,
) -> Result<u64, DynError> {
    prepared_model_download_bytes(
        root,
        candidate,
        model_id,
        revision,
        &root.join("models").join(candidate).join("manifest.json"),
    )
}

impl FrozenRunGuard {
    fn revalidate_before_heldout(&self, root: &Path, binary: &Path) -> Result<(), DynError> {
        self.revalidate(root, binary, "heldout")
    }

    fn revalidate_before_50k(&self, root: &Path, binary: &Path) -> Result<(), DynError> {
        self.revalidate(root, binary, "50k")
    }

    fn revalidate_after_50k(&self, root: &Path, binary: &Path) -> Result<(), DynError> {
        self.revalidate(root, binary, "post_50k")
    }

    fn revalidate_before_final_report(&self, root: &Path, binary: &Path) -> Result<(), DynError> {
        self.revalidate(root, binary, "final_report")
    }

    fn revalidate(&self, root: &Path, binary: &Path, _phase: &str) -> Result<(), DynError> {
        let frozen_bytes = fs::read(root.join("frozen-config.json"))?;
        if format!("{:x}", Sha256::digest(&frozen_bytes)) != self.frozen_config_sha256 {
            return Err("frozen live-eval configuration changed after dev".into());
        }
        if file_sha256(&root.join("lexical-profile-ab-report.json"))?
            != self.lexical_profile_report_sha256
        {
            return Err("frozen lexical profile A/B report changed after dev".into());
        }
        let (git_head, worktree_clean) = repository_identity(&self.repo_root)?;
        if git_head != self.integrated_git_head || !worktree_clean {
            return Err(
                "integrated git identity changed or worktree became dirty after dev".into(),
            );
        }
        if file_sha256(binary)? != self.release_binary_sha256 {
            return Err("release binary changed after dev".into());
        }
        load_contract_gate_bundle(
            root,
            &self.integrated_git_head,
            &self.release_binary_sha256,
            Some(&self.contract_gate_bundle_sha256),
        )?;
        if file_sha256(&model_preparation_provenance_path(root)?)?
            != self.model_preparation_provenance_sha256
        {
            return Err("model preparation provenance changed after dev".into());
        }
        for frozen in &self.candidate_states {
            let Some(relative) = frozen.manifest_relative_path.as_deref() else {
                continue;
            };
            let manifest_path = root.join(relative);
            let manifest_bytes = fs::read(&manifest_path)?;
            let manifest = ModelManifestV1::from_json_slice(&manifest_bytes)?;
            let snapshot = prepared_snapshot_digest(
                manifest_path
                    .parent()
                    .ok_or("prepared candidate manifest parent missing")?,
            )?;
            let actual_manifest_hash = manifest.hash();
            let actual_manifest_file_sha256 = format!("{:x}", Sha256::digest(&manifest_bytes));
            let actual_artifact_set_sha256 = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&manifest.artifacts)?)
            );
            if frozen.manifest_hash.as_deref() != Some(actual_manifest_hash.as_str())
                || frozen.manifest_file_sha256.as_deref()
                    != Some(actual_manifest_file_sha256.as_str())
                || frozen.artifact_set_sha256.as_deref()
                    != Some(actual_artifact_set_sha256.as_str())
                || frozen.prepared_snapshot_sha256.as_deref() != Some(snapshot.sha256.as_str())
                || frozen.prepared_snapshot_bytes != Some(snapshot.bytes)
                || frozen.prepared_snapshot_file_count != Some(snapshot.file_count)
            {
                return Err("prepared candidate snapshot changed after dev".into());
            }
        }
        Ok(())
    }
}

fn repository_identity(repo_root: &Path) -> Result<(String, bool), DynError> {
    let git_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()?;
    if !git_head.status.success() {
        return Err("integrated git HEAD is unavailable".into());
    }
    let git_head = String::from_utf8(git_head.stdout)?.trim().to_string();
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .current_dir(repo_root)
        .output()?;
    if !status.status.success() {
        return Err("integrated git worktree state is unavailable".into());
    }
    Ok((git_head, status.stdout.is_empty()))
}

pub(super) fn run(
    root: &Path,
    corpus_raw: &str,
    dev_raw: &str,
    test_raw: &str,
    provenance_raw: &str,
) -> Result<(), DynError> {
    ensure_target_root(root)?;
    reset_stderr_audit();
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
    let corpus_sha256 = digest_hex(corpus_raw);
    let qrels_dev_sha256 = digest_hex(dev_raw);
    let provenance: FixtureProvenance = serde_json::from_str(provenance_raw)?;
    let repo_root = std::env::current_dir()?.canonicalize()?;
    let binary = eval_binary()?;
    let host = host_record(&binary)?;
    let (integrated_git_head, worktree_clean) = repository_identity(&repo_root)?;
    if integrated_git_head != host.git_sha || !worktree_clean {
        return Err("live evaluation requires the integrated clean git HEAD".into());
    }
    let host_protocol_failures = host_protocol_failures(&host);
    run_contract_gate_bundle(
        root,
        &repo_root,
        &host.git_sha,
        &binary,
        &host.binary_sha256,
    )?;
    let contract_gate_bundle =
        load_contract_gate_bundle(root, &host.git_sha, &host.binary_sha256, None)?;
    let model_preparation_provenance_sha256 =
        file_sha256(&model_preparation_provenance_path(root)?)?;
    let judgment_pool_verified = provenance.judgment_pool.complete
        && provenance.judgment_pool.multi_source_query_count >= 10;
    let server = PublicSnapshotServer::start(&corpus)?;
    eprintln!("live-eval phase=bm25-dev-real-store status=running");
    let bm25_fixture = CliFixture::new(
        root.join("bm25-live"),
        binary.clone(),
        server.base_url.clone(),
    )?;
    bm25_fixture.write_config(None)?;
    bm25_fixture.sync()?;
    let (database_schema_fingerprint, tantivy_schema_fingerprint) =
        bm25_fixture.schema_fingerprints()?;
    let bm25_dev_evidence = run_single_pass(&bm25_fixture, &dev)?;
    let bm25_dev = evaluate_rankings(
        &corpus,
        &dev,
        &bm25_dev_evidence,
        &root.join("bm25-live/dev-events.jsonl"),
    )?;
    let (lexical_profile_dev, frozen_lexical_profile) = run_lexical_profile_dev_ab(
        root,
        &bm25_fixture,
        &corpus,
        &dev,
        &bm25_dev_evidence,
        &integrated_git_head,
        &corpus_sha256,
        &qrels_dev_sha256,
        &tantivy_schema_fingerprint,
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
            root,
            &server,
            &binary,
            candidate,
            model_id,
            revision,
            &manifest,
            &corpus,
            &dev,
            frozen_lexical_profile.selected_profile,
        ));
    }
    let dragonkue = dragonkue_blocker(root);
    let mut frozen_candidate_states = candidate_states
        .iter()
        .map(|state| frozen_candidate_state(root, state))
        .collect::<Result<Vec<_>, _>>()?;
    frozen_candidate_states.push(frozen_blocked_candidate_state(root, &dragonkue)?);

    // Production exposes neither k nor candidate-window knobs.  The only
    // deployable frozen values are the actual source constants: k=60 and
    // TOP_K(20) * overfetch(4) = 80.  The k/window grid above is diagnostic.
    let frozen = FrozenConfig {
        schema_version: "qgh.live_model_eval_config.v2",
        integrated_git_head: integrated_git_head.clone(),
        worktree_clean,
        release_binary_sha256: host.binary_sha256.clone(),
        contract_gate_bundle_sha256: contract_gate_bundle.sha256.clone(),
        model_preparation_provenance_sha256: model_preparation_provenance_sha256.clone(),
        candidate_states: frozen_candidate_states.clone(),
        corpus_sha256: corpus_sha256.clone(),
        qrels_dev_sha256: qrels_dev_sha256.clone(),
        qrels_test_sha256: digest_hex(test_raw),
        chunker_version: CHUNKER_VERSION,
        chunker_fingerprint: CHUNKER_FINGERPRINT,
        context_profile: "qgh.context.v1",
        fusion: "production_equal_rrf",
        rrf_k: RRF_K,
        candidate_window: CANDIDATE_WINDOW,
        lexical_profile: frozen_lexical_profile.clone(),
        warmup_runs: WARMUP_RUNS,
        measured_runs: MEASURED_RUNS,
        cold_process_runs: COLD_PROCESS_RUNS,
        required_50k_chunks: 50_000,
        required_chunk_tokens: 900,
        required_batch_size: REQUIRED_BATCH_SIZE,
        required_intra_op_threads: REQUIRED_INTRA_OP_THREADS,
        database_schema_version: "qgh.db.v1",
        vector_schema_version: "qgh.vector.v1",
        database_schema_fingerprint,
        tantivy_schema_fingerprint,
    };
    let frozen_bytes = with_newline(serde_json::to_vec_pretty(&frozen)?);
    let frozen_config_hash = format!("{:x}", Sha256::digest(&frozen_bytes));
    fs::write(root.join("frozen-config.json"), &frozen_bytes)?;
    let frozen_guard = FrozenRunGuard {
        repo_root,
        frozen_config_sha256: frozen_config_hash.clone(),
        integrated_git_head,
        release_binary_sha256: host.binary_sha256.clone(),
        contract_gate_bundle_sha256: contract_gate_bundle.sha256.clone(),
        model_preparation_provenance_sha256,
        candidate_states: frozen_candidate_states,
        lexical_profile_report_sha256: frozen_lexical_profile.dev_report_sha256.clone(),
    };

    frozen_guard.revalidate_before_heldout(root, &binary)?;
    let held_out = parse_jsonl::<QrelRecord>(test_raw);
    eprintln!("live-eval phase=heldout-open-once status=running");
    let bm25_evidence = run_single_pass(&bm25_fixture, &held_out)?;
    let bm25 = evaluate_rankings(
        &corpus,
        &held_out,
        &bm25_evidence,
        &root.join("bm25-live/heldout-events.jsonl"),
    )?;
    let (_, lexical_heldout_evidence) = run_lexical_profile_pass(
        &bm25_fixture,
        &held_out,
        &bm25_evidence,
        frozen_lexical_profile.selected_profile,
        TOP_K,
        true,
    )?;
    let lexical_profile_heldout = evaluate_rankings(
        &corpus,
        &held_out,
        &lexical_heldout_evidence,
        &root.join("bm25-live/lexical-selected-heldout-events.jsonl"),
    )?;
    write_pretty(root.join("bm25-live/dev-report.json"), &bm25_dev)?;
    write_pretty(root.join("bm25-live/heldout-report.json"), &bm25)?;

    let mut candidates = vec![dragonkue];
    for state in candidate_states {
        candidates.push(match state {
            Ok(prepared) => finish_candidate(
                root,
                &server,
                &binary,
                &frozen_guard,
                prepared,
                &corpus,
                &held_out,
                frozen_lexical_profile.selected_profile,
            ),
            Err(report) => *report,
        });
    }
    let audited_qrels = dev.iter().chain(&held_out).collect::<Vec<_>>();
    let redaction = verify_redaction(root, &corpus, &audited_qrels)?;
    for candidate in &mut candidates {
        candidate
            .light_gate_failures
            .extend(host_protocol_failures.iter().cloned());
        candidate
            .quality_resource_gate_failures
            .extend(host_protocol_failures.iter().cloned());
        if !judgment_pool_verified {
            candidate
                .light_gate_failures
                .push("pooled_judgment_coverage_required".to_string());
            candidate
                .quality_resource_gate_failures
                .push("pooled_judgment_coverage_required".to_string());
        }
        if !redaction.passed {
            candidate
                .light_gate_failures
                .push("raw_query_or_body_logged".to_string());
            candidate
                .quality_resource_gate_failures
                .push("raw_query_or_body_logged".to_string());
        }
    }

    let selected_light_candidate = select_candidate(&candidates, true);
    let selected_quality_candidate = select_candidate(&candidates, false);
    let promotion_eligible =
        selected_light_candidate.is_some() || selected_quality_candidate.is_some();
    let context_blocked = candidates.iter().any(|candidate| {
        candidate
            .context_contract
            .as_ref()
            .is_some_and(|evidence| !evidence.passed)
    });
    let mut promotion_blockers = host_protocol_failures.clone();
    if !judgment_pool_verified {
        promotion_blockers.push("pooled_judgment_coverage_required".to_string());
    }
    if context_blocked {
        promotion_blockers.push("context_contract_failed".to_string());
    }
    if !redaction.passed {
        promotion_blockers.push("raw_query_or_body_logged".to_string());
    }
    if !promotion_eligible && promotion_blockers.is_empty() {
        promotion_blockers.push("no_passing_candidate".to_string());
    }
    let evaluation_state = if promotion_eligible {
        "promotion_eligible"
    } else if context_blocked {
        "blocked_context_contract"
    } else {
        "completed_not_eligible"
    }
    .to_string();
    frozen_guard.revalidate_before_final_report(root, &binary)?;
    let mut report = FullReport {
        schema_version: "qgh.live_model_eval_report.v1",
        run_finished_at: command_output("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]),
        corpus_snapshot_at: provenance.snapshot_at,
        host,
        frozen_config_hash,
        bm25_dev,
        bm25,
        lexical_profile_dev,
        lexical_profile_heldout,
        candidates,
        selected_light_candidate,
        selected_quality_candidate,
        raw_query_or_body_logged: !redaction.passed,
        redaction,
        evaluation_state,
        promotion_eligible,
        promotion_blockers,
        host_protocol_failures,
        contract_gate_bundle,
    };
    let report_path = root.join("live-model-eval-report.json");
    let preflight_bytes = with_newline(serde_json::to_vec_pretty(&report)?);
    if contains_sensitive_payload(&preflight_bytes, &corpus, &audited_qrels) {
        return Err("final report redaction preflight failed".into());
    }
    report.redaction.artifact_files_checked += 1;
    let report_bytes = with_newline(serde_json::to_vec_pretty(&report)?);
    if contains_sensitive_payload(&report_bytes, &corpus, &audited_qrels) {
        return Err("final report redaction verification failed".into());
    }
    fs::write(&report_path, report_bytes)?;
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
    reset_stderr_audit();
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
    let (database_schema_fingerprint, tantivy_schema_fingerprint) =
        fixture.schema_fingerprints()?;
    let evidence = run_single_pass(&fixture, std::slice::from_ref(qrel))?;
    let ranked_source_count = evidence.rankings.get(&qrel.query_id).map_or(0, Vec::len);
    if ranked_source_count == 0
        || evidence.get_total == 0
        || evidence.get_success != evidence.get_total
    {
        return Err("qgh-only runtime smoke did not query -> get round-trip".into());
    }
    let redaction = verify_redaction(root, &corpus, &[qrel])?;
    let report = SmokeReport {
        schema_version: "qgh.live_model_eval_smoke.v1",
        corpus_sha256: digest_hex(corpus_raw),
        corpus_source_count: corpus.len(),
        database_schema_fingerprint,
        tantivy_schema_fingerprint,
        query_id: qrel.query_id.clone(),
        query_sha256: digest_hex(&qrel.query),
        ranked_source_count,
        get_round_trip: evidence.get_success as f64 / evidence.get_total as f64,
        raw_query_or_body_logged: !redaction.passed,
    };
    write_pretty(root.join("qgh-only-runtime-smoke.json"), &report)?;
    Ok(())
}

pub(super) fn run_context_probe_smoke(
    root: &Path,
    corpus_raw: &str,
    manifest_path: &Path,
) -> Result<(), DynError> {
    ensure_target_root(root)?;
    reset_stderr_audit();
    fs::create_dir_all(root)?;
    let corpus = parse_jsonl::<CorpusRecord>(corpus_raw);
    let comment = corpus
        .iter()
        .find(|source| source.entity_type == "issue_comment")
        .ok_or("context smoke comment missing")?;
    let issue = corpus
        .iter()
        .find(|source| {
            source.entity_type == "issue"
                && source.repo == comment.repo
                && source.issue_number == comment.issue_number
        })
        .ok_or("context smoke parent issue missing")?;
    let subset = vec![issue.clone(), comment.clone()];
    let manifest = ModelManifestV1::from_json_slice(&fs::read(manifest_path)?)?;
    let manifest_hash = manifest.hash();
    let binary = eval_binary()?;
    let server = PublicSnapshotServer::start(&subset)?;
    let fixture = CliFixture::new(
        root.join("context-contract-runtime-smoke"),
        binary,
        server.base_url.clone(),
    )?;
    fixture.write_config(None)?;
    fixture.sync()?;
    fixture.write_config(Some(manifest_path))?;
    let embed = fixture.qgh(&["embed", "--force", "--json"])?;
    let envelope: Value = serde_json::from_slice(&embed.stdout)?;
    if envelope["data"]["chunks"]["embedded"]
        .as_u64()
        .unwrap_or_default()
        == 0
    {
        return Err("context smoke embedded no chunks".into());
    }
    let (candidate_database_schema_fingerprint, candidate_tantivy_schema_fingerprint) =
        fixture.schema_fingerprints()?;
    let context_contract =
        probe_context_contract(&fixture.db_path(), &manifest.context_template_version)?;
    let redaction = verify_redaction(root, &subset, &[])?;
    let report = ContextProbeSmokeReport {
        schema_version: "qgh.live_model_eval_context_probe.v1",
        corpus_source_count: subset.len(),
        manifest_hash,
        candidate_database_schema_fingerprint,
        candidate_tantivy_schema_fingerprint,
        evaluation_state: if context_contract.passed {
            "context_contract_passed"
        } else {
            "blocked_context_contract"
        },
        raw_query_or_body_logged: !redaction.passed,
        redaction,
        context_contract,
    };
    write_pretty(root.join("context-contract-runtime-smoke.json"), &report)?;
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
    lexical_profile: FrozenLexicalProfileName,
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
        lexical_profile,
    ) {
        Ok(prepared) => Ok(prepared),
        Err(error) => {
            let context_failure = error.downcast_ref::<ContextContractFailure>();
            let code = if context_failure.is_some() {
                "eval.context_contract_failed"
            } else {
                "eval.runtime_failed"
            };
            eprintln!("live-eval candidate={candidate} status=blocked code={code}");
            let mut report = blocked_candidate(
                candidate,
                model_id,
                revision,
                code,
                if context_failure.is_some() {
                    "context_contract"
                } else {
                    "dev_preparation"
                },
            );
            if let Some(failure) = context_failure {
                report.manifest_hash = Some(failure.manifest_hash.clone());
                report.candidate_database_schema_fingerprint =
                    Some(failure.candidate_database_schema_fingerprint.clone());
                report.candidate_tantivy_schema_fingerprint =
                    Some(failure.candidate_tantivy_schema_fingerprint.clone());
                report.context_contract = Some(failure.evidence.clone());
                report.status = "blocked_context_contract".to_string();
            }
            Err(Box::new(report))
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
    lexical_profile: FrozenLexicalProfileName,
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
    let download_transfer_bytes = Some(prepared_model_download_bytes(
        root,
        candidate,
        model_id,
        revision,
        manifest_path,
    )?);

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
    let (candidate_database_schema_fingerprint, candidate_tantivy_schema_fingerprint) =
        fixture.schema_fingerprints()?;
    let context_contract =
        probe_context_contract(&fixture.db_path(), &manifest.context_template_version)?;
    if !context_contract.passed {
        return Err(Box::new(ContextContractFailure {
            manifest_hash,
            candidate_database_schema_fingerprint,
            candidate_tantivy_schema_fingerprint,
            evidence: context_contract,
        }));
    }

    eprintln!("live-eval candidate={candidate} phase=cold-processes status=running");
    let mut cold_samples_ms = Vec::with_capacity(COLD_PROCESS_RUNS);
    let mut isolated_peak_rss = quality_embed.peak_rss_bytes;
    for _ in 0..COLD_PROCESS_RUNS {
        let sample = fixture.timed_query(&dev[0])?;
        cold_samples_ms.push(sample.elapsed_ms);
        isolated_peak_rss = isolated_peak_rss.max(sample.peak_rss_bytes);
    }

    eprintln!("live-eval candidate={candidate} phase=dev-mcp status=running");
    let dev_run = run_dev_mcp(&fixture, dev)?;
    isolated_peak_rss = isolated_peak_rss.max(dev_run.peak_rss_bytes);
    let selected_dev_evidence = selected_hybrid_evidence(
        &fixture,
        dev,
        &dev_run.primary,
        &dev_run.diagnostic,
        lexical_profile,
    )?;
    let dev_metrics = evaluate_rankings(
        corpus,
        dev,
        &selected_dev_evidence,
        &fixture.root.join("dev-events.jsonl"),
    )?;
    let offline_dev_diagnostics = offline_fusion_diagnostics(dev, &dev_run.diagnostic)?;
    Ok(PreparedCandidate {
        candidate: candidate.to_string(),
        model_id: model_id.to_string(),
        revision: revision.to_string(),
        manifest_hash,
        candidate_database_schema_fingerprint,
        candidate_tantivy_schema_fingerprint,
        context_contract,
        fixture,
        manifest_path: manifest_path.to_path_buf(),
        snapshot_bytes,
        download_transfer_bytes,
        chunk_count,
        embed_seconds,
        db_growth_bytes_per_chunk: db_growth as f64 / chunk_count as f64,
        cold_samples_ms,
        isolated_peak_rss,
        dev_metrics,
        offline_dev_diagnostics,
    })
}

#[allow(clippy::too_many_arguments)]
fn finish_candidate(
    root: &Path,
    server: &PublicSnapshotServer,
    binary: &Path,
    frozen_guard: &FrozenRunGuard,
    mut prepared: PreparedCandidate,
    corpus: &[CorpusRecord],
    held_out: &[QrelRecord],
    lexical_profile: FrozenLexicalProfileName,
) -> CandidateReport {
    eprintln!(
        "live-eval candidate={} phase=heldout-warm-mcp status=running",
        prepared.candidate
    );
    let warm = match run_heldout_mcp(&prepared.fixture, held_out) {
        Ok(warm) => warm,
        Err(_) => {
            return prepared_candidate_report(
                prepared,
                "blocked_after_dev",
                None,
                None,
                vec!["runtime_unavailable".to_string()],
                vec!["runtime_unavailable".to_string()],
                Some(Blocker {
                    code: "eval.runtime_failed".to_string(),
                    phase: "heldout_query".to_string(),
                }),
            );
        }
    };
    prepared.isolated_peak_rss = prepared.isolated_peak_rss.max(warm.peak_rss_bytes);
    let selected_heldout = match selected_hybrid_evidence(
        &prepared.fixture,
        held_out,
        &warm.held_out,
        &warm.diagnostic,
        lexical_profile,
    ) {
        Ok(evidence) => evidence,
        Err(_) => {
            return prepared_candidate_report(
                prepared,
                "blocked_after_dev",
                None,
                None,
                vec!["runtime_unavailable".to_string()],
                vec!["runtime_unavailable".to_string()],
                Some(Blocker {
                    code: "eval.runtime_failed".to_string(),
                    phase: "heldout_selected_lexical_profile".to_string(),
                }),
            );
        }
    };
    let held_out_metrics = match evaluate_rankings(
        corpus,
        held_out,
        &selected_heldout,
        &prepared.fixture.root.join("heldout-events.jsonl"),
    ) {
        Ok(metrics) => metrics,
        Err(_) => {
            return prepared_candidate_report(
                prepared,
                "blocked_after_dev",
                None,
                None,
                vec!["runtime_unavailable".to_string()],
                vec!["runtime_unavailable".to_string()],
                Some(Blocker {
                    code: "eval.runtime_failed".to_string(),
                    phase: "heldout_quality".to_string(),
                }),
            );
        }
    };
    eprintln!(
        "live-eval candidate={} phase=50k-effective-runtime status=running",
        prepared.candidate
    );
    if frozen_guard.revalidate_before_50k(root, binary).is_err() {
        let resources = resource_evidence(
            &prepared,
            &warm,
            "frozen_revalidation",
            false,
            &PartialBackfillEvidence::default(),
        );
        return prepared_candidate_report(
            prepared,
            "blocked_after_heldout",
            Some(held_out_metrics),
            Some(resources),
            vec!["resource_evidence_incomplete".to_string()],
            vec!["resource_evidence_incomplete".to_string()],
            Some(Blocker {
                code: "eval.frozen_identity_changed".to_string(),
                phase: "frozen_revalidation".to_string(),
            }),
        );
    }
    let backfill = match measure_50k_backfill(
        root,
        server,
        binary,
        &prepared.candidate,
        &prepared.manifest_path,
        corpus,
        &prepared.fixture.cache_home,
    ) {
        Ok(backfill) => backfill,
        Err(error) => {
            prepared.isolated_peak_rss =
                prepared.isolated_peak_rss.max(error.partial.peak_rss_bytes);
            let identity_changed = frozen_guard.revalidate_after_50k(root, binary).is_err();
            let failure_phase = if identity_changed {
                "frozen_revalidation_after_50k"
            } else {
                error.phase
            };
            let failure_code = if identity_changed {
                "eval.frozen_identity_changed"
            } else {
                error.code
            };
            let resources =
                resource_evidence(&prepared, &warm, failure_phase, false, &error.partial);
            return prepared_candidate_report(
                prepared,
                "blocked_after_heldout",
                Some(held_out_metrics),
                Some(resources),
                vec!["resource_evidence_incomplete".to_string()],
                vec!["resource_evidence_incomplete".to_string()],
                Some(Blocker {
                    code: failure_code.to_string(),
                    phase: failure_phase.to_string(),
                }),
            );
        }
    };
    prepared.isolated_peak_rss = prepared.isolated_peak_rss.max(backfill.peak_rss_bytes);
    let partial = PartialBackfillEvidence {
        chunk_count: Some(backfill.chunk_count),
        raw_chunk_tokens: Some(backfill.raw_chunk_tokens),
        contextual_chunk_tokens: Some(backfill.contextual_chunk_tokens),
        seconds: Some(backfill.seconds),
        chunks_per_second: Some(backfill.chunks_per_second),
        db_growth_bytes_per_chunk: Some(backfill.db_growth_bytes_per_chunk),
        peak_rss_bytes: backfill.peak_rss_bytes,
        integrity: Some(backfill.integrity),
    };
    if frozen_guard.revalidate_after_50k(root, binary).is_err() {
        let resources = resource_evidence(
            &prepared,
            &warm,
            "frozen_revalidation_after_50k",
            false,
            &partial,
        );
        return prepared_candidate_report(
            prepared,
            "blocked_after_heldout",
            Some(held_out_metrics),
            Some(resources),
            vec!["resource_evidence_incomplete".to_string()],
            vec!["resource_evidence_incomplete".to_string()],
            Some(Blocker {
                code: "eval.frozen_identity_changed".to_string(),
                phase: "frozen_revalidation_after_50k".to_string(),
            }),
        );
    }
    let resources = resource_evidence(&prepared, &warm, "complete", true, &partial);
    let light_gate_failures = live_resource_failures(&resources, true);
    let quality_resource_gate_failures = live_resource_failures(&resources, false);
    let candidate_eligible = held_out_metrics.quality_gate_failures.is_empty()
        && (light_gate_failures.is_empty() || quality_resource_gate_failures.is_empty());
    let status = if candidate_eligible {
        "completed_eligible"
    } else {
        "completed_not_eligible"
    };
    prepared_candidate_report(
        prepared,
        status,
        Some(held_out_metrics),
        Some(resources),
        light_gate_failures,
        quality_resource_gate_failures,
        None,
    )
}

fn resource_evidence(
    prepared: &PreparedCandidate,
    warm: &WarmEvidence,
    phase: &str,
    complete: bool,
    backfill: &PartialBackfillEvidence,
) -> ResourceEvidence {
    ResourceEvidence {
        phase: phase.to_string(),
        complete,
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
        measured_raw_chunk_tokens: backfill.raw_chunk_tokens,
        measured_contextual_chunk_tokens: backfill.contextual_chunk_tokens,
        measured_50k_embed_and_write_seconds: backfill.seconds,
        measured_50k_chunks_per_second: backfill.chunks_per_second,
        measured_50k_db_growth_bytes_per_chunk: backfill.db_growth_bytes_per_chunk,
        backfill_integrity: backfill.integrity.clone(),
        download_transfer_bytes: prepared.download_transfer_bytes,
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
    }
}

pub(super) fn resource_failure_contract_for_test() -> Value {
    let resources = ResourceEvidence {
        phase: "50k_embed".to_string(),
        complete: false,
        cold_process_samples_ms: vec![1.0],
        cold_start_p95_ms: 1.0,
        warm_query_sample_count: 1,
        warm_query_p50_ms: 1.0,
        warm_query_p95_ms: 1.0,
        warm_path_includes_manifest_artifact_rehash: true,
        isolated_peak_rss_bytes: 1,
        complete_model_snapshot_bytes: 1,
        quality_corpus_chunk_count: 1,
        quality_corpus_embed_and_write_seconds: 1.0,
        quality_corpus_db_growth_bytes_per_chunk: 1.0,
        measured_50k_chunk_count: Some(12_500),
        measured_raw_chunk_tokens: Some(900),
        measured_contextual_chunk_tokens: Some(915),
        measured_50k_embed_and_write_seconds: Some(1.0),
        measured_50k_chunks_per_second: Some(12_500.0),
        measured_50k_db_growth_bytes_per_chunk: None,
        backfill_integrity: None,
        download_transfer_bytes: Some(1),
        required_batch_size: REQUIRED_BATCH_SIZE,
        effective_batch_size: EFFECTIVE_BATCH_SIZE,
        required_intra_op_threads: REQUIRED_INTRA_OP_THREADS,
        effective_intra_op_threads: None,
        effective_ort_inter_op: "unverified".to_string(),
        effective_ort_execution_mode: "unverified".to_string(),
        fastembed_version: "5.17.2".to_string(),
        protocol_unverified: vec!["resource_evidence_incomplete".to_string()],
    };
    json!({
        "held_out_metrics": {"query_count": 80},
        "resources": resources,
        "blocker": Blocker {
            code: "eval.resource_failed".to_string(),
            phase: "50k_embed".to_string(),
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn prepared_candidate_report(
    prepared: PreparedCandidate,
    status: &str,
    held_out_metrics: Option<RetrievalMetrics>,
    resources: Option<ResourceEvidence>,
    light_gate_failures: Vec<String>,
    quality_resource_gate_failures: Vec<String>,
    blocker: Option<Blocker>,
) -> CandidateReport {
    let report_path = prepared.fixture.root.join("report.json");
    let report = CandidateReport {
        candidate: prepared.candidate,
        model_id: prepared.model_id,
        resolved_revision: prepared.revision,
        runtime: "qgh release binary / fastembed UserDefinedEmbeddingModel".to_string(),
        status: status.to_string(),
        manifest_hash: Some(prepared.manifest_hash),
        candidate_database_schema_fingerprint: Some(prepared.candidate_database_schema_fingerprint),
        candidate_tantivy_schema_fingerprint: Some(prepared.candidate_tantivy_schema_fingerprint),
        context_contract: Some(prepared.context_contract),
        dev_metrics: Some(prepared.dev_metrics),
        held_out_metrics,
        offline_dev_diagnostics: prepared.offline_dev_diagnostics,
        resources,
        light_gate_failures,
        quality_resource_gate_failures,
        blocker,
        synthetic_substitution: false,
    };
    let _ = write_pretty(report_path, &report);
    report
}

fn blocked_candidate(
    candidate: &str,
    model_id: &str,
    revision: &str,
    code: &str,
    phase: &str,
) -> CandidateReport {
    CandidateReport {
        candidate: candidate.to_string(),
        model_id: model_id.to_string(),
        resolved_revision: revision.to_string(),
        runtime: "qgh release binary / fastembed UserDefinedEmbeddingModel".to_string(),
        status: "blocked".to_string(),
        manifest_hash: None,
        candidate_database_schema_fingerprint: None,
        candidate_tantivy_schema_fingerprint: None,
        context_contract: None,
        dev_metrics: None,
        held_out_metrics: None,
        offline_dev_diagnostics: Vec::new(),
        resources: None,
        light_gate_failures: vec!["runtime_unavailable".to_string()],
        quality_resource_gate_failures: vec!["runtime_unavailable".to_string()],
        blocker: Some(Blocker {
            code: code.to_string(),
            phase: phase.to_string(),
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
            phase: "candidate_acquisition".to_string(),
        },
        Err(error) => Blocker {
            code: error.code().to_string(),
            phase: "candidate_acquisition".to_string(),
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
        candidate_database_schema_fingerprint: None,
        candidate_tantivy_schema_fingerprint: None,
        context_contract: None,
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
) -> Result<BackfillEvidence, Box<ResourceRunFailure>> {
    let mut partial = PartialBackfillEvidence::default();
    let fixture = CliFixture::new_with_cache(
        root.join(format!("{candidate}-resource-live")),
        binary.to_path_buf(),
        server.base_url.clone(),
        shared_cache.to_path_buf(),
    )
    .map_err(|_| resource_run_failure("50k_setup", &partial))?;
    fixture
        .write_config(None)
        .map_err(|_| resource_run_failure("50k_setup", &partial))?;
    fixture
        .sync()
        .map_err(|_| resource_run_failure("50k_setup", &partial))?;
    fixture
        .write_config(Some(manifest_path))
        .map_err(|_| resource_run_failure("50k_setup", &partial))?;
    let _ = fixture
        .qgh(&[
            "query",
            "resource schema initialization",
            "--repo",
            "juicyjusung/qgh",
            "--json",
        ])
        .map_err(|_| resource_run_failure("50k_setup", &partial))?;
    let chunk = public_900_token_chunk(manifest_path, corpus)
        .map_err(|_| resource_run_failure("50k_tokenize", &partial))?;
    partial.raw_chunk_tokens = Some(chunk.raw_token_count);
    partial.contextual_chunk_tokens = Some(chunk.contextual_token_count);
    seed_50k_chunks(&fixture.db_path(), &chunk.body, chunk.raw_token_count)
        .map_err(|_| resource_run_failure("50k_seed", &partial))?;
    let seeded_chunk_count =
        chunk_count(&fixture.db_path()).map_err(|_| resource_run_failure("50k_seed", &partial))?;
    partial.chunk_count = Some(seeded_chunk_count);
    if seeded_chunk_count != 50_000 {
        return Err(resource_run_failure("50k_seed", &partial));
    }
    let bytes_before = checkpoint_and_storage_bytes(&fixture.db_path())
        .map_err(|_| resource_run_failure("50k_seed", &partial))?;
    let started_at = command_output("date", &["+%Y-%m-%dT%H:%M:%S%z"]);
    eprintln!(
        "live-eval candidate={candidate} phase=50k-production-embed status=running chunks={seeded_chunk_count} started_at={started_at}"
    );
    let embed = match fixture.timed_qgh_with_start(&["embed", "--force", "--json"], candidate) {
        Ok(embed) => embed,
        Err(failure) => {
            partial.peak_rss_bytes = failure.peak_rss_bytes;
            partial.seconds = Some(failure.elapsed_ms / 1_000.0);
            partial.chunk_count = failure.embedded_chunks;
            if let (Some(chunks), Some(seconds)) = (partial.chunk_count, partial.seconds) {
                if seconds > 0.0 {
                    partial.chunks_per_second = Some(chunks as f64 / seconds);
                }
            }
            return Err(resource_run_failure("50k_embed", &partial));
        }
    };
    partial.peak_rss_bytes = embed.peak_rss_bytes;
    let seconds = embed.elapsed_ms / 1_000.0;
    partial.seconds = Some(seconds);
    let envelope: Value = serde_json::from_slice(&embed.output.stdout)
        .map_err(|_| resource_run_failure("50k_embed", &partial))?;
    let embedded = envelope["data"]["chunks"]["embedded"]
        .as_u64()
        .unwrap_or_default() as usize;
    partial.chunk_count = Some(embedded);
    if seconds > 0.0 {
        partial.chunks_per_second = Some(embedded as f64 / seconds);
    }
    if embedded != 50_000 {
        return Err(resource_run_failure("50k_embed", &partial));
    }
    let bytes_after = checkpoint_and_storage_bytes(&fixture.db_path())
        .map_err(|_| resource_run_failure("50k_verify", &partial))?;
    let db_growth = bytes_after.saturating_sub(bytes_before);
    partial.db_growth_bytes_per_chunk = Some(db_growth as f64 / embedded as f64);
    let integrity = verify_backfill_integrity(&fixture.db_path(), 50_000)
        .map_err(|_| resource_run_failure("50k_verify", &partial))?;
    partial.integrity = Some(integrity.clone());
    Ok(BackfillEvidence {
        chunk_count: embedded,
        raw_chunk_tokens: chunk.raw_token_count,
        contextual_chunk_tokens: chunk.contextual_token_count,
        seconds,
        chunks_per_second: embedded as f64 / seconds,
        db_growth_bytes_per_chunk: db_growth as f64 / embedded as f64,
        peak_rss_bytes: embed.peak_rss_bytes,
        integrity,
    })
}

fn resource_run_failure(
    phase: &'static str,
    partial: &PartialBackfillEvidence,
) -> Box<ResourceRunFailure> {
    Box::new(ResourceRunFailure {
        code: "eval.resource_failed",
        phase,
        partial: partial.clone(),
    })
}

fn public_900_token_chunk(
    manifest_path: &Path,
    corpus: &[CorpusRecord],
) -> Result<ResourceChunk, DynError> {
    let store = PreparedModelStore::new(PathBuf::new());
    let snapshot = store.load_manifest(manifest_path)?;
    let tokenizer = FastembedTokenizer::from_prepared_snapshot(&snapshot)?;
    let source = corpus
        .iter()
        .filter(|source| source.repo == "juicyjusung/qgh" && source.entity_type == "issue")
        .min_by(|left, right| left.source_id.cmp(&right.source_id))
        .ok_or("public English source missing")?;
    let repeated = std::iter::repeat_n(source.body.as_str(), 32)
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
    let contextual = context_input(
        "issue",
        "github.com",
        &source.repo,
        source.issue_number,
        &source.title,
        &chunk.body,
    );
    let contextual_token_count = tokenizer.count_tokens(&contextual)?;
    Ok(ResourceChunk {
        body: chunk.body,
        raw_token_count: chunk.token_count,
        contextual_token_count,
    })
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

fn verify_backfill_integrity(
    db_path: &Path,
    expected_chunks: usize,
) -> Result<BackfillIntegrityEvidence, DynError> {
    let connection = Connection::open(db_path)?;
    register_eval_sqlite_vec(&connection)?;
    let raw_chunks: usize =
        connection.query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))?;
    let (
        publication_embedding_generation_id,
        publication_active,
        generation_state,
        generation_output_dimension,
        generation_total_chunks,
        generation_completed_chunks,
    ): (i64, bool, String, usize, usize, usize) = connection.query_row(
        "SELECT eg.id, rp.active, eg.state, eg.output_dimension,
                eg.total_chunks, eg.completed_chunks
         FROM retrieval_publication_pointer p
         JOIN retrieval_publications rp ON rp.publication_id = p.publication_id
         JOIN embedding_generations eg ON eg.id = rp.embedding_generation_id
         WHERE p.id = 1",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    let generation_chunk_rows: usize = connection.query_row(
        "SELECT COUNT(*) FROM embedding_generation_chunks WHERE generation_id = ?1",
        [publication_embedding_generation_id],
        |row| row.get(0),
    )?;
    let vector_mapping_rows: usize = connection.query_row(
        "SELECT COUNT(*) FROM embedding_generation_vector_rows WHERE generation_id = ?1",
        [publication_embedding_generation_id],
        |row| row.get(0),
    )?;
    let vector_table_count: usize = connection.query_row(
        "SELECT COUNT(DISTINCT vector_table)
         FROM embedding_generation_vector_rows WHERE generation_id = ?1",
        [publication_embedding_generation_id],
        |row| row.get(0),
    )?;
    if vector_table_count != 1 {
        return Err("50k backfill must use exactly one dimension-specific vector table".into());
    }
    let vector_table: String = connection.query_row(
        "SELECT vector_table FROM embedding_generation_vector_rows
         WHERE generation_id = ?1 ORDER BY vector_table LIMIT 1",
        [publication_embedding_generation_id],
        |row| row.get(0),
    )?;
    let expected_vector_table =
        format!("embedding_generation_vectors_d{generation_output_dimension}");
    if vector_table != expected_vector_table {
        return Err("50k backfill vector table does not match the generation dimension".into());
    }
    let vec0_rows: usize = connection.query_row(
        &format!(
            "SELECT COUNT(*) FROM {vector_table} v
             JOIN embedding_generation_vector_rows m ON m.vector_rowid = v.rowid
             WHERE m.generation_id = ?1 AND m.vector_table = ?2"
        ),
        params![publication_embedding_generation_id, vector_table],
        |row| row.get(0),
    )?;
    let evidence = BackfillIntegrityEvidence {
        raw_chunks,
        generation_state,
        generation_output_dimension,
        generation_total_chunks,
        generation_completed_chunks,
        generation_chunk_rows,
        vector_mapping_rows,
        vector_table,
        vector_table_count,
        vec0_rows,
        publication_embedding_generation_id,
        publication_active,
    };
    if evidence.raw_chunks != expected_chunks
        || evidence.generation_state != "active"
        || evidence.generation_total_chunks != expected_chunks
        || evidence.generation_completed_chunks != expected_chunks
        || evidence.generation_chunk_rows != expected_chunks
        || evidence.vector_mapping_rows != expected_chunks
        || evidence.vector_table_count != 1
        || evidence.vec0_rows != expected_chunks
        || !evidence.publication_active
    {
        return Err("50k backfill generation integrity verification failed".into());
    }
    Ok(evidence)
}

fn register_eval_sqlite_vec(connection: &Connection) -> Result<(), DynError> {
    type SqliteVecEntryPoint = unsafe extern "C" fn(
        db: *mut rusqlite::ffi::sqlite3,
        pz_err_msg: *mut *mut std::os::raw::c_char,
        p_api: *const rusqlite::ffi::sqlite3_api_routines,
    ) -> std::os::raw::c_int;
    let entry_point = unsafe {
        std::mem::transmute::<unsafe extern "C" fn(), SqliteVecEntryPoint>(
            sqlite_vec::sqlite3_vec_init,
        )
    };
    let rc = unsafe { entry_point(connection.handle(), std::ptr::null_mut(), std::ptr::null()) };
    if rc != rusqlite::ffi::SQLITE_OK {
        return Err("sqlite-vec registration failed for 50k verification".into());
    }
    Ok(())
}

pub(super) fn backfill_integrity_for_test(
    db_path: &Path,
    expected_chunks: usize,
) -> Result<Value, DynError> {
    Ok(serde_json::to_value(verify_backfill_integrity(
        db_path,
        expected_chunks,
    )?)?)
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

fn run_single_pass(fixture: &CliFixture, qrels: &[QrelRecord]) -> Result<QueryEvidence, DynError> {
    let mut client = McpClient::start(fixture)?;
    let evidence = query_pass(&mut client, qrels, true, TOP_K, false)?.0;
    let _ = client.finish()?;
    Ok(evidence)
}

#[derive(Debug, Clone)]
struct ActiveTantivySnapshot {
    generation: i64,
    path: PathBuf,
}

fn verify_rankings_with_get(
    fixture: &CliFixture,
    rankings: &BTreeMap<String, Vec<String>>,
) -> Result<(usize, usize, usize), DynError> {
    let mut client = McpClient::start(fixture)?;
    let mut total = 0usize;
    let mut success = 0usize;
    let mut stale = 0usize;
    for source_id in rankings.values().flat_map(|ranked| ranked.iter()) {
        total += 1;
        let response = client.call_tool("get", json!({ "source_id": source_id }))?;
        match structured_content(&response) {
            Ok(get) if get["data"]["source"]["source_id"].as_str() == Some(source_id) => {
                success += 1;
            }
            Ok(get) => {
                stale += usize::from(get["error"]["code"].as_str() == Some("source.tombstoned"));
            }
            Err(_) => {}
        }
    }
    let _ = client.finish()?;
    Ok((total, success, stale))
}

fn run_lexical_profile_pass(
    fixture: &CliFixture,
    qrels: &[QrelRecord],
    production_exact_evidence: &QueryEvidence,
    profile: FrozenLexicalProfileName,
    limit: usize,
    verify_get: bool,
) -> Result<(ActiveTantivySnapshot, QueryEvidence), DynError> {
    let snapshot = fixture.active_tantivy_snapshot()?;
    let mut rankings = BTreeMap::new();
    let mut branch_observations = BTreeMap::new();
    for qrel in qrels {
        if qrel.query_class == QueryClass::ExactIdentifier {
            let ranked = production_exact_evidence
                .rankings
                .get(&qrel.query_id)
                .cloned()
                .ok_or("production exact ranking missing")?;
            rankings.insert(qrel.query_id.clone(), ranked);
            branch_observations.insert(qrel.query_id.clone(), Vec::new());
            continue;
        }
        let filters = SearchFilters {
            repo: Some(qrel.filters.repo.clone()),
            issue: qrel
                .filters
                .issue_number
                .map(i64::try_from)
                .transpose()
                .map_err(|_| "qrel issue number does not fit i64")?,
            source_types: qrel.filters.source_type.as_ref().map_or_else(
                || vec!["issue".to_string(), "issue_comment".to_string()],
                |source_type| vec![source_type.clone()],
            ),
            ..SearchFilters::default()
        };
        let hits = search_with_lexical_profile_for_eval(
            &snapshot.path,
            &qrel.query,
            &filters,
            profile.eval_profile(),
            limit,
        )
        .map_err(|_| "lexical profile search failed")?;
        rankings.insert(
            qrel.query_id.clone(),
            hits.iter().map(|hit| hit.source_id.clone()).collect(),
        );
        branch_observations.insert(
            qrel.query_id.clone(),
            hits.into_iter()
                .map(|hit| BranchObservation {
                    source_id: hit.source_id,
                    lexical_score: Some(f64::from(hit.score)),
                    vector_distance: None,
                })
                .collect(),
        );
    }
    let (get_total, get_success, stale_failures) = if verify_get {
        verify_rankings_with_get(fixture, &rankings)?
    } else {
        (0, 0, 0)
    };
    Ok((
        snapshot,
        QueryEvidence {
            rankings,
            branch_observations,
            get_total,
            get_success,
            stale_failures,
            hybrid_required: false,
            hybrid_expected_queries: 0,
            hybrid_path_queries: 0,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
fn run_lexical_profile_dev_ab(
    root: &Path,
    fixture: &CliFixture,
    corpus: &[CorpusRecord],
    dev: &[QrelRecord],
    production_evidence: &QueryEvidence,
    integrated_git_head: &str,
    corpus_sha256: &str,
    qrels_dev_sha256: &str,
    tantivy_schema_fingerprint: &str,
) -> Result<(LexicalProfileAbReport, FrozenLexicalProfile), DynError> {
    eprintln!("live-eval phase=lexical-profile-dev-ab status=running");
    let (production_snapshot, production_v1_evidence) = run_lexical_profile_pass(
        fixture,
        dev,
        production_evidence,
        FrozenLexicalProfileName::ProductionV1,
        TOP_K,
        true,
    )?;
    if production_v1_evidence.rankings != production_evidence.rankings {
        return Err("eval lexical V1 seam diverged from the production query protocol".into());
    }
    let production_v1 = evaluate_rankings(
        corpus,
        dev,
        &production_v1_evidence,
        &root.join("bm25-live/lexical-production-v1-dev-events.jsonl"),
    )?;
    let (candidate_snapshot, metadata_evidence) = run_lexical_profile_pass(
        fixture,
        dev,
        production_evidence,
        FrozenLexicalProfileName::MetadataBoostV1,
        TOP_K,
        true,
    )?;
    if candidate_snapshot.generation != production_snapshot.generation
        || candidate_snapshot.path != production_snapshot.path
    {
        return Err("lexical A/B profiles did not use the same active Tantivy generation".into());
    }
    let metadata_boost_v1 = evaluate_rankings(
        corpus,
        dev,
        &metadata_evidence,
        &root.join("bm25-live/lexical-metadata-boost-v1-dev-events.jsonl"),
    )?;
    let selection = select_lexical_profile(&production_v1, &metadata_boost_v1);
    let dev_refs = dev.iter().collect::<Vec<_>>();
    let redaction = verify_redaction(root, corpus, &dev_refs)?;
    let mut report = LexicalProfileAbReport {
        schema_version: "qgh.lexical_profile_ab.v1",
        integrated_git_head: integrated_git_head.to_string(),
        corpus_sha256: corpus_sha256.to_string(),
        qrels_dev_sha256: qrels_dev_sha256.to_string(),
        active_tantivy_generation: production_snapshot.generation,
        tantivy_schema_fingerprint: tantivy_schema_fingerprint.to_string(),
        production_v1,
        metadata_boost_v1,
        selection,
        redaction_passed: redaction.passed,
    };
    let mut report_bytes = with_newline(serde_json::to_vec_pretty(&report)?);
    if contains_sensitive_payload(&report_bytes, corpus, &dev_refs) {
        report.redaction_passed = false;
        report_bytes = with_newline(serde_json::to_vec_pretty(&report)?);
    }
    if !report.redaction_passed {
        return Err("lexical profile A/B report redaction failed".into());
    }
    let report_sha256 = format!("{:x}", Sha256::digest(&report_bytes));
    write_atomic(&root.join("lexical-profile-ab-report.json"), &report_bytes)?;
    let frozen = FrozenLexicalProfile {
        production_profile: "production_v1",
        comparison_candidate: "metadata_boost_v1",
        selected_profile: report.selection.selected_profile,
        selection_reasons: report.selection.reasons.clone(),
        dev_report_sha256: report_sha256,
        corpus_sha256: corpus_sha256.to_string(),
        qrels_dev_sha256: qrels_dev_sha256.to_string(),
        active_tantivy_generation: production_snapshot.generation,
    };
    Ok((report, frozen))
}

fn selected_hybrid_evidence(
    fixture: &CliFixture,
    qrels: &[QrelRecord],
    production_primary: &QueryEvidence,
    diagnostic: &QueryEvidence,
    profile: FrozenLexicalProfileName,
) -> Result<QueryEvidence, DynError> {
    if profile == FrozenLexicalProfileName::ProductionV1 {
        return Ok(production_primary.clone());
    }
    let (_, lexical) = run_lexical_profile_pass(
        fixture,
        qrels,
        production_primary,
        profile,
        CANDIDATE_WINDOW,
        false,
    )?;
    let mut rankings = BTreeMap::new();
    let mut branch_observations = BTreeMap::new();
    let mut hybrid_expected_queries = 0usize;
    let mut hybrid_path_queries = 0usize;
    for qrel in qrels {
        if qrel.query_class == QueryClass::ExactIdentifier {
            rankings.insert(
                qrel.query_id.clone(),
                production_primary
                    .rankings
                    .get(&qrel.query_id)
                    .cloned()
                    .ok_or("selected hybrid exact ranking missing")?,
            );
            branch_observations.insert(qrel.query_id.clone(), Vec::new());
            continue;
        }
        let lexical_hits = lexical
            .branch_observations
            .get(&qrel.query_id)
            .ok_or("selected lexical branch observations missing")?;
        let model_scored = qrel.query_class != QueryClass::Negative;
        hybrid_expected_queries += usize::from(model_scored);
        let mut combined = BTreeMap::<String, BranchObservation>::new();
        for hit in lexical_hits {
            combined.insert(hit.source_id.clone(), hit.clone());
        }
        for hit in diagnostic
            .branch_observations
            .get(&qrel.query_id)
            .ok_or("selected hybrid vector observations missing")?
        {
            if let Some(distance) = hit.vector_distance {
                combined
                    .entry(hit.source_id.clone())
                    .or_insert(BranchObservation {
                        source_id: hit.source_id.clone(),
                        lexical_score: None,
                        vector_distance: None,
                    })
                    .vector_distance = Some(distance);
            }
        }
        let combined = combined.into_values().collect::<Vec<_>>();
        hybrid_path_queries += usize::from(
            model_scored
                && combined
                    .iter()
                    .any(|hit| hit.lexical_score.is_some() && hit.vector_distance.is_some()),
        );
        rankings.insert(
            qrel.query_id.clone(),
            fuse_branch_observations(&combined, RRF_K, CANDIDATE_WINDOW)
                .into_iter()
                .take(TOP_K)
                .collect(),
        );
        branch_observations.insert(qrel.query_id.clone(), combined);
    }
    let (get_total, get_success, stale_failures) = verify_rankings_with_get(fixture, &rankings)?;
    Ok(QueryEvidence {
        rankings,
        branch_observations,
        get_total,
        get_success,
        stale_failures,
        hybrid_required: true,
        hybrid_expected_queries,
        hybrid_path_queries,
    })
}

fn run_dev_mcp(fixture: &CliFixture, dev: &[QrelRecord]) -> Result<DevRunEvidence, DynError> {
    let protocol = DevQueryProtocol::frozen();
    let mut client = McpClient::start(fixture)?;
    let (primary, _) = query_pass(&mut client, dev, true, protocol.primary_query_limit, true)?;
    let (diagnostic, _) = query_pass(
        &mut client,
        dev,
        false,
        protocol.diagnostic_query_limit,
        true,
    )?;
    let peak_rss_bytes = client.finish()?;
    Ok(DevRunEvidence {
        primary,
        diagnostic,
        peak_rss_bytes,
    })
}

fn run_heldout_mcp(
    fixture: &CliFixture,
    held_out: &[QrelRecord],
) -> Result<WarmEvidence, DynError> {
    let mut client = McpClient::start(fixture)?;
    for _ in 0..WARMUP_RUNS {
        let _ = query_pass(&mut client, held_out, false, TOP_K, true)?;
    }
    let mut latencies = Vec::with_capacity(held_out.len() * MEASURED_RUNS);
    let mut held_out_evidence = None;
    for measured in 0..MEASURED_RUNS {
        let capture_get = measured + 1 == MEASURED_RUNS;
        let (evidence, mut pass_latencies) =
            query_pass(&mut client, held_out, capture_get, TOP_K, true)?;
        latencies.append(&mut pass_latencies);
        if capture_get {
            held_out_evidence = Some(evidence);
        }
    }
    let (diagnostic, _) = query_pass(
        &mut client,
        held_out,
        false,
        DEV_DIAGNOSTIC_QUERY_LIMIT,
        true,
    )?;
    let peak_rss_bytes = client.finish()?;
    Ok(WarmEvidence {
        held_out: held_out_evidence.ok_or("held-out evidence missing")?,
        diagnostic,
        latencies_ms: latencies,
        peak_rss_bytes,
    })
}

fn query_pass(
    client: &mut McpClient,
    qrels: &[QrelRecord],
    verify_get: bool,
    query_limit: usize,
    expect_hybrid: bool,
) -> Result<(QueryEvidence, Vec<f64>), DynError> {
    let mut rankings = BTreeMap::new();
    let mut branch_observations = BTreeMap::new();
    let mut latencies = Vec::with_capacity(qrels.len());
    let mut get_total = 0usize;
    let mut get_success = 0usize;
    let mut stale_failures = 0usize;
    let mut hybrid_expected_queries = 0usize;
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
        let model_scored = !matches!(
            qrel.query_class,
            QueryClass::ExactIdentifier | QueryClass::Negative
        );
        hybrid_expected_queries += usize::from(expect_hybrid && model_scored);
        hybrid_path_queries += usize::from(
            expect_hybrid
                && model_scored
                && results
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
            hybrid_required: expect_hybrid,
            hybrid_expected_queries,
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
                    Some(weighted_score([
                        mean(QueryClass::EnglishSemantic),
                        mean(QueryClass::KoreanSemantic),
                        mean(QueryClass::KoQueryEnSource),
                        mean(QueryClass::EnQueryKoSource),
                        mean(QueryClass::CommentOnly),
                        mean(QueryClass::LongContext),
                    ])),
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
            hard_filter_violations += usize::from(
                qrel.filters
                    .source_type
                    .as_deref()
                    .is_some_and(|source_type| source.entity_type != source_type),
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
    let weighted_mrr_at_10 = weighted_mrr(&per_class);
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
    if evidence.hybrid_required
        && !hybrid_gate_for_test(
            evidence.hybrid_expected_queries,
            evidence.hybrid_path_queries,
        )
    {
        quality_gate_failures.push("hybrid_path_coverage".to_string());
    }
    Ok(RetrievalMetrics {
        query_count: qrels.len(),
        per_class,
        weighted_ndcg_at_10,
        weighted_mrr_at_10,
        exact_top_1,
        hard_filter_violations,
        get_round_trip,
        stale_leakage_live_fixture: Some(evidence.stale_failures),
        duplicate_crowding_queries,
        hybrid_expected_queries: evidence.hybrid_expected_queries,
        hybrid_path_queries: evidence.hybrid_path_queries,
        quality_gate_failures,
    })
}

fn weighted_ndcg(per_class: &BTreeMap<QueryClass, ClassMetrics>) -> f64 {
    let metric = |class| per_class.get(&class).map_or(0.0, |value| value.ndcg_at_10);
    weighted_score([
        metric(QueryClass::EnglishSemantic),
        metric(QueryClass::KoreanSemantic),
        metric(QueryClass::KoQueryEnSource),
        metric(QueryClass::EnQueryKoSource),
        metric(QueryClass::CommentOnly),
        metric(QueryClass::LongContext),
    ])
}

fn weighted_mrr(per_class: &BTreeMap<QueryClass, ClassMetrics>) -> f64 {
    let metric = |class| per_class.get(&class).map_or(0.0, |value| value.mrr_at_10);
    weighted_score([
        metric(QueryClass::EnglishSemantic),
        metric(QueryClass::KoreanSemantic),
        metric(QueryClass::KoQueryEnSource),
        metric(QueryClass::EnQueryKoSource),
        metric(QueryClass::CommentOnly),
        metric(QueryClass::LongContext),
    ])
}

fn live_resource_failures(resources: &ResourceEvidence, light: bool) -> Vec<String> {
    let mut failures = resources.protocol_unverified.clone();
    if !resources.complete {
        failures.push("resource_evidence_incomplete".to_string());
    }
    if resources.measured_50k_chunk_count != Some(50_000) {
        failures.push("measured_50k_chunk_count".to_string());
    }
    if resources.measured_raw_chunk_tokens != Some(900) {
        failures.push("measured_raw_chunk_tokens".to_string());
    }
    if resources
        .measured_contextual_chunk_tokens
        .is_none_or(|tokens| tokens <= 900)
    {
        failures.push("measured_contextual_chunk_tokens".to_string());
    }
    if resources.backfill_integrity.is_none() {
        failures.push("backfill_integrity".to_string());
    }
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
        if resources
            .measured_50k_chunks_per_second
            .is_none_or(|value| value < 10.0)
        {
            failures.push("measured_50k_chunks_per_second".to_string());
        }
        if resources
            .measured_50k_db_growth_bytes_per_chunk
            .is_none_or(|value| value > 3.0 * 1024.0)
        {
            failures.push("measured_50k_db_growth_bytes_per_chunk".to_string());
        }
    } else {
        if resources.cold_start_p95_ms > 10_000.0 {
            failures.push("cold_start_p95_ms".to_string());
        }
        if resources.isolated_peak_rss_bytes > 5 * gib / 2 {
            failures.push("isolated_peak_rss_bytes".to_string());
        }
        if resources
            .measured_50k_chunks_per_second
            .is_none_or(|value| value < 3.0)
        {
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
    let best_ndcg = eligible
        .iter()
        .filter_map(|candidate| candidate.held_out_metrics.as_ref())
        .map(|metrics| metrics.weighted_ndcg_at_10)
        .reduce(f64::max)?;
    if light {
        eligible.retain(|candidate| {
            candidate.held_out_metrics.as_ref().is_some_and(|metrics| {
                best_ndcg - metrics.weighted_ndcg_at_10 <= 0.02 + f64::EPSILON
            })
        });
        eligible.sort_by(|left, right| {
            left.resources
                .as_ref()
                .map_or(u64::MAX, |resources| {
                    resources.complete_model_snapshot_bytes
                })
                .cmp(&right.resources.as_ref().map_or(u64::MAX, |resources| {
                    resources.complete_model_snapshot_bytes
                }))
                .then_with(|| left.candidate.cmp(&right.candidate))
        });
    } else {
        eligible.retain(|candidate| {
            candidate.held_out_metrics.as_ref().is_some_and(|metrics| {
                best_ndcg - metrics.weighted_ndcg_at_10 <= 0.005 + f64::EPSILON
            })
        });
        eligible.sort_by(|left, right| {
            let left_mrr = left
                .held_out_metrics
                .as_ref()
                .map_or(0.0, |metrics| metrics.weighted_mrr_at_10);
            let right_mrr = right
                .held_out_metrics
                .as_ref()
                .map_or(0.0, |metrics| metrics.weighted_mrr_at_10);
            right_mrr
                .partial_cmp(&left_mrr)
                .unwrap_or(Ordering::Equal)
                .then_with(|| {
                    left.resources
                        .as_ref()
                        .map_or(u64::MAX, |resources| {
                            resources.complete_model_snapshot_bytes
                        })
                        .cmp(&right.resources.as_ref().map_or(u64::MAX, |resources| {
                            resources.complete_model_snapshot_bytes
                        }))
                })
                .then_with(|| left.candidate.cmp(&right.candidate))
        });
    }
    eligible
        .first()
        .map(|candidate| candidate.candidate.clone())
}

struct TimedQuery {
    elapsed_ms: f64,
    peak_rss_bytes: u64,
}

#[derive(Debug)]
struct TimedOutput {
    output: Output,
    elapsed_ms: f64,
    peak_rss_bytes: u64,
}

#[derive(Debug, Serialize)]
struct TimedCommandFailure {
    elapsed_ms: f64,
    peak_rss_bytes: u64,
    embedded_chunks: Option<usize>,
}

fn run_timed_command(
    mut command: Command,
    candidate: &str,
) -> Result<TimedOutput, TimedCommandFailure> {
    let started = Instant::now();
    let child = command.spawn().map_err(|_| TimedCommandFailure {
        elapsed_ms: started.elapsed().as_secs_f64() * 1_000.0,
        peak_rss_bytes: 0,
        embedded_chunks: None,
    })?;
    eprintln!(
        "live-eval candidate={candidate} phase=50k-production-embed time_wrapper_pid={}",
        child.id()
    );
    let output = child.wait_with_output().map_err(|_| TimedCommandFailure {
        elapsed_ms: started.elapsed().as_secs_f64() * 1_000.0,
        peak_rss_bytes: 0,
        embedded_chunks: None,
    })?;
    record_stderr(&output.stderr);
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let peak_rss_bytes = parse_peak_rss(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return Err(TimedCommandFailure {
            elapsed_ms,
            peak_rss_bytes,
            embedded_chunks: serde_json::from_slice::<Value>(&output.stdout)
                .ok()
                .and_then(|value| value["data"]["chunks"]["embedded"].as_u64())
                .map(|count| count as usize),
        });
    }
    Ok(TimedOutput {
        output,
        elapsed_ms,
        peak_rss_bytes,
    })
}

pub(super) fn timed_failure_evidence_for_test() -> Result<Value, DynError> {
    let mut command = Command::new("/usr/bin/time");
    command
        .arg("-l")
        .args([
            "/bin/sh",
            "-c",
            "printf '{\"data\":{\"chunks\":{\"embedded\":12500}}}'; exit 7",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let failure =
        run_timed_command(command, "typed-failure-test").expect_err("test child must exit nonzero");
    Ok(serde_json::to_value(failure)?)
}

struct BackfillEvidence {
    chunk_count: usize,
    raw_chunk_tokens: usize,
    contextual_chunk_tokens: usize,
    seconds: f64,
    chunks_per_second: f64,
    db_growth_bytes_per_chunk: f64,
    peak_rss_bytes: u64,
    integrity: BackfillIntegrityEvidence,
}

struct ResourceChunk {
    body: String,
    raw_token_count: usize,
    contextual_token_count: usize,
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

    fn init_git_worktree(&self) -> Result<(), DynError> {
        let output = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&self.root)
            .output()?;
        if !output.status.success() {
            return Err("hard-filter fixture git init failed".into());
        }
        Ok(())
    }

    fn write_hard_filter_probe_config(
        &self,
        target_repo: &str,
        competing_repo: &str,
        embedding_enabled: bool,
    ) -> Result<(), DynError> {
        let embedding = if embedding_enabled {
            format!("\n[embedding]\nprovider = \"local\"\nmodel = \"{FILTER_PROBE_PRESET}\"\n")
        } else {
            String::new()
        };
        let config = format!(
            r#"schema_version = "qgh.config.v1"

[profiles.work]
host = "github.com"
api_base_url = "{}"
web_base_url = "https://github.com"
repos = ["{target_repo}", "{competing_repo}"]

[profiles.work.token_source]
type = "env"
env = "QGH_PUBLIC_FIXTURE_AUTH"
{}"#,
            self.api_base_url, embedding
        );
        fs::write(self.config_home.join("qgh/config.toml"), config)?;
        let policy = format!(
            r#"schema_version = "qgh.repo.v1"

[repo]
github = "{target_repo}"

[defaults]
scope = "repo"
state = "all"
source_types = ["issue"]
labels = []

[query]
limit = 10
"#
        );
        fs::write(self.root.join(".qgh.toml"), policy)?;
        Ok(())
    }

    fn sync(&self) -> Result<(), DynError> {
        let _ = self.qgh(&["sync", "--all", "--json"])?;
        Ok(())
    }

    fn qgh(&self, arguments: &[&str]) -> Result<Output, DynError> {
        self.qgh_with_test_vectors(arguments, None, None)
    }

    fn qgh_with_test_vectors(
        &self,
        arguments: &[&str],
        query_vectors_json: Option<&str>,
        document_vectors_json: Option<&str>,
    ) -> Result<Output, DynError> {
        let mut command = self.base_command();
        command.args(arguments);
        if let Some(query_vectors_json) = query_vectors_json {
            command.env(TEST_EMBEDDING_QUERY_VECTORS_ENV, query_vectors_json);
        }
        if let Some(document_vectors_json) = document_vectors_json {
            command.env(TEST_EMBEDDING_DOCUMENT_VECTORS_ENV, document_vectors_json);
        }
        let output = command.output()?;
        record_stderr(&output.stderr);
        if !output.status.success() {
            return Err(command_failure(&output).into());
        }
        Ok(output)
    }

    fn seed_hard_filter_chunks(&self, sources: &[CorpusRecord]) -> Result<(), DynError> {
        let connection = Connection::open(self.db_path())?;
        connection.execute("DELETE FROM chunks", [])?;
        for source in sources {
            let source_version_id: i64 = connection.query_row(
                "SELECT coalesce(im.latest_version_id, cm.latest_version_id)
                 FROM source_entities se
                 LEFT JOIN issue_metadata im ON im.source_id = se.source_id
                 LEFT JOIN comment_metadata cm ON cm.source_id = se.source_id
                 WHERE se.source_id = ?1 AND se.lifecycle_state = 'active'",
                [&source.source_id],
                |row| row.get(0),
            )?;
            connection.execute(
                "INSERT INTO chunks (source_id, source_version_id, body)
                 VALUES (?1, ?2, ?3)",
                params![
                    source.source_id,
                    source_version_id,
                    hard_filter_chunk_body(&source.source_id),
                ],
            )?;
        }
        Ok(())
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
        record_stderr(&output.stderr);
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
    ) -> Result<TimedOutput, TimedCommandFailure> {
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
        run_timed_command(command, candidate)
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
        record_stderr(&output.stderr);
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

    fn active_tantivy_snapshot(&self) -> Result<ActiveTantivySnapshot, DynError> {
        let connection = Connection::open(self.db_path())?;
        let (generation, path): (i64, String) = connection.query_row(
            "SELECT publication.tantivy_generation, generation.path
             FROM retrieval_publication_pointer pointer
             JOIN retrieval_publications publication
               ON publication.publication_id = pointer.publication_id
             JOIN index_generations generation
               ON generation.generation = publication.tantivy_generation
             WHERE pointer.id = 1
               AND publication.active = 1
               AND generation.active = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let path = PathBuf::from(path);
        if !path.is_dir() {
            return Err("active publication Tantivy generation is unavailable".into());
        }
        Ok(ActiveTantivySnapshot { generation, path })
    }

    fn schema_fingerprints(&self) -> Result<(String, String), DynError> {
        let connection = Connection::open(self.db_path())?;
        let mut statement = connection.prepare(
            "SELECT type, name, coalesce(sql, '')
             FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%'
             ORDER BY type, name",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut database_schema = String::new();
        for row in rows {
            let (kind, name, sql) = row?;
            database_schema.push_str(&kind);
            database_schema.push('\0');
            database_schema.push_str(&name);
            database_schema.push('\0');
            database_schema.push_str(&sql);
            database_schema.push('\n');
        }
        let active_index_path = self.active_tantivy_snapshot()?.path;
        let metadata: Value =
            serde_json::from_slice(&fs::read(active_index_path.join("meta.json"))?)?;
        let schema = metadata
            .get("schema")
            .ok_or("Tantivy meta.json schema missing")?;
        Ok((
            digest_hex(&database_schema),
            digest_hex(&serde_json::to_string(schema)?),
        ))
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
        record_stderr(stderr.as_bytes());
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

#[derive(Clone)]
struct IssueApiMetadata {
    state: String,
    author: String,
    labels: Vec<String>,
}

impl PublicSnapshotServer {
    fn start(corpus: &[CorpusRecord]) -> Result<Self, DynError> {
        Self::start_with_issue_metadata(corpus, &BTreeMap::new())
    }

    fn start_with_issue_metadata(
        corpus: &[CorpusRecord],
        issue_metadata: &BTreeMap<String, IssueApiMetadata>,
    ) -> Result<Self, DynError> {
        let responses = Arc::new(build_api_responses(corpus, issue_metadata)?);
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

fn build_api_responses(
    corpus: &[CorpusRecord],
    issue_metadata: &BTreeMap<String, IssueApiMetadata>,
) -> Result<BTreeMap<String, String>, DynError> {
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
                    issue_metadata.get(&source.source_id),
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

fn issue_json(
    source: &CorpusRecord,
    comment_count: usize,
    metadata: Option<&IssueApiMetadata>,
) -> Result<Value, DynError> {
    let state = metadata.map_or("open", |value| value.state.as_str());
    let author = metadata.map_or("public-fixture-author", |value| value.author.as_str());
    let labels = metadata.map_or_else(Vec::new, |value| {
        value
            .labels
            .iter()
            .map(|name| json!({"name": name}))
            .collect::<Vec<_>>()
    });
    Ok(json!({
        "id": synthetic_issue_id(&source.repo, source.issue_number),
        "node_id": decoded_node_id(&source.source_id)?,
        "number": source.issue_number,
        "title": source.title,
        "body": source.body,
        "state": state,
        "locked": false,
        "comments": comment_count,
        "html_url": source.canonical_url,
        "created_at": source.github_updated_at,
        "updated_at": source.github_updated_at,
        "closed_at": null,
        "user": {"login": author},
        "labels": labels,
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
    Ok(prepared_snapshot_digest(path)?.bytes)
}

fn prepared_snapshot_digest(root: &Path) -> Result<PreparedSnapshotDigest, DynError> {
    let root_metadata = fs::symlink_metadata(root)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err("prepared model snapshot root must be a regular directory".into());
    }
    let mut files = Vec::new();
    collect_snapshot_file_digests(root, root, &mut files)?;
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    if files.is_empty() {
        return Err("prepared model snapshot contains no regular files".into());
    }
    let bytes = files.iter().map(|file| file.byte_size).sum();
    let canonical = serde_json::to_vec(&files)?;
    Ok(PreparedSnapshotDigest {
        sha256: format!("{:x}", Sha256::digest(canonical)),
        bytes,
        file_count: files.len(),
    })
}

fn collect_snapshot_file_digests(
    root: &Path,
    current: &Path,
    files: &mut Vec<SnapshotFileDigest>,
) -> Result<(), DynError> {
    let mut entries = fs::read_dir(current)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err("prepared model snapshot contains a symbolic link".into());
        }
        if metadata.is_dir() {
            collect_snapshot_file_digests(root, &path, files)?;
            continue;
        }
        if !metadata.is_file() {
            return Err("prepared model snapshot contains a non-regular entry".into());
        }
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .ok_or("prepared model snapshot path is not UTF-8")?
            .replace(std::path::MAIN_SEPARATOR, "/");
        files.push(SnapshotFileDigest {
            relative_path: relative,
            byte_size: metadata.len(),
            sha256: file_sha256(&path)?,
        });
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String, DynError> {
    Ok(format!("{:x}", Sha256::digest(fs::read(path)?)))
}

pub(super) fn prepared_snapshot_digest_for_test(root: &Path) -> Result<Value, DynError> {
    Ok(serde_json::to_value(prepared_snapshot_digest(root)?)?)
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

fn host_record(binary: &Path) -> Result<HostRecord, DynError> {
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
    Ok(HostRecord {
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
        binary_sha256: file_sha256(binary)?,
        git_sha: command_output("git", &["rev-parse", "HEAD"]),
    })
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
    let cwd = std::env::current_dir()?.canonicalize()?;
    ensure_target_root_from_cwd(&cwd, root)
}

fn ensure_target_root_from_cwd(cwd: &Path, root: &Path) -> Result<(), DynError> {
    let cwd = cwd.canonicalize()?;
    if !target_root_allowed(&cwd, root) {
        return Err("live eval artifacts must stay under target/qgh-eval".into());
    }
    let target = cwd.join("target");
    if target.exists() {
        let metadata = fs::symlink_metadata(&target)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("live eval target directory must not be a symlink".into());
        }
    }
    let allowed = cwd.join("target/qgh-eval");
    if allowed.exists() {
        let metadata = fs::symlink_metadata(&allowed)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("live eval canonical artifact root must not be a symlink".into());
        }
    }
    fs::create_dir_all(&allowed)?;
    let allowed = allowed.canonicalize()?;
    if !allowed.starts_with(&cwd) {
        return Err("live eval canonical artifact root escapes the repository".into());
    }
    let candidate = if root.is_absolute() {
        root.to_path_buf()
    } else {
        cwd.join(root)
    };
    let relative = candidate.strip_prefix(cwd.join("target/qgh-eval"))?;
    let mut cursor = allowed.clone();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err("live eval root contains a non-normal path component".into());
        };
        cursor.push(component);
        if cursor.exists() {
            let metadata = fs::symlink_metadata(&cursor)?;
            if metadata.file_type().is_symlink() || !cursor.canonicalize()?.starts_with(&allowed) {
                return Err("live eval root escapes target/qgh-eval through a symlink".into());
            }
        }
    }
    Ok(())
}

pub(super) fn ensure_target_root_from_cwd_for_test(
    cwd: &Path,
    root: &Path,
) -> Result<(), DynError> {
    ensure_target_root_from_cwd(cwd, root)
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
