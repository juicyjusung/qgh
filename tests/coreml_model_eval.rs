#![cfg(all(feature = "fastembed-provider", target_os = "macos"))]

use fastembed::{
    ExecutionProviderDispatch, InitOptionsUserDefined, QuantizationMode, TextEmbedding,
    TokenizerFiles, UserDefinedEmbeddingModel,
};
use ort::ep::{
    coreml::{ComputeUnits, ModelFormat, SpecializationStrategy},
    CoreML,
};
use qgh::embedding::{
    ArtifactRole, ModelManifestV1, PoolingKind, PreparedModelSnapshot, PreparedModelStore,
    QuantizationKind,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

type DynError = Box<dyn Error>;

fn coreml_probe_opt_in(value: Option<&str>) -> bool {
    value == Some("1")
}

#[test]
fn coreml_probe_is_explicitly_opt_in() {
    assert!(!coreml_probe_opt_in(None));
    assert!(!coreml_probe_opt_in(Some("true")));
    assert!(coreml_probe_opt_in(Some("1")));
}

#[derive(Serialize)]
struct CoreMlProbeReport {
    schema_version: &'static str,
    model_id: String,
    resolved_revision: String,
    manifest_sha256: String,
    prepared_snapshot_sha256: String,
    prepared_artifacts: Vec<PreparedArtifactIdentity>,
    git_head: String,
    git_worktree_clean: bool,
    test_binary_sha256: String,
    execution_provider: &'static str,
    cpu_fallback_allowed: bool,
    batch_size: usize,
    measured_runs: usize,
    output_dimension: usize,
    cpu_init_ms: f64,
    coreml_init_ms: f64,
    cpu_warm_p50_ms: f64,
    cpu_warm_p95_ms: f64,
    coreml_warm_p50_ms: f64,
    coreml_warm_p95_ms: f64,
    coreml_speedup_at_p50: f64,
    minimum_cpu_coreml_cosine: f64,
    parity_passed: bool,
}

#[derive(Clone, Serialize)]
struct PreparedArtifactIdentity {
    role: ArtifactRole,
    relative_path: String,
    sha256: String,
    byte_size: u64,
}

#[test]
fn prepared_snapshot_digest_changes_with_artifact_identity() {
    let artifact = PreparedArtifactIdentity {
        role: ArtifactRole::OnnxModel,
        relative_path: "onnx/model.onnx".to_string(),
        sha256: "11".repeat(32),
        byte_size: 42,
    };
    let first = prepared_snapshot_digest(std::slice::from_ref(&artifact));
    let mut changed = artifact;
    changed.byte_size += 1;

    assert_ne!(first, prepared_snapshot_digest(&[changed]));
}

#[test]
#[ignore = "loads a local ONNX model through the macOS CoreML execution provider"]
fn coreml_cpu_gpu_probe() {
    assert!(
        coreml_probe_opt_in(std::env::var("QGH_COREML_MODEL_EVAL").ok().as_deref()),
        "set QGH_COREML_MODEL_EVAL=1 to run the CoreML probe"
    );
    let manifest_path = PathBuf::from(
        std::env::var_os("QGH_COREML_MODEL_MANIFEST")
            .expect("QGH_COREML_MODEL_MANIFEST must point to a prepared public model"),
    );
    let output_root = std::env::var_os("QGH_COREML_MODEL_EVAL_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/qgh-eval"));

    let manifest_bytes = fs::read(&manifest_path).expect("prepared model manifest is readable");
    let snapshot = PreparedModelStore::new(PathBuf::new())
        .load_manifest(&manifest_path)
        .expect("prepared model snapshot passes checksum and graph verification");
    let manifest = &snapshot.manifest;
    assert_eq!(manifest.quantization, QuantizationKind::None);
    assert_eq!(manifest.output_dimension, 384);
    let prepared_artifacts = prepared_artifact_identities(manifest);
    let git_head = command_text("git", &["rev-parse", "HEAD"]).expect("Git HEAD is available");
    let git_worktree_clean = command_text("git", &["status", "--porcelain"])
        .expect("Git status is available")
        .is_empty();
    assert!(
        git_worktree_clean,
        "CoreML evidence requires a clean worktree"
    );
    let test_binary_sha256 =
        sha256_file(&std::env::current_exe().expect("CoreML test binary path is available"))
            .expect("CoreML test binary is hashable");

    let (mut cpu, cpu_init_ms) =
        build_model(&snapshot, Vec::new()).expect("CPU reference model initializes");
    let coreml_provider = CoreML::default()
        .with_model_format(ModelFormat::MLProgram)
        .with_specialization_strategy(SpecializationStrategy::FastPrediction)
        .with_compute_units(ComputeUnits::CPUAndGPU)
        .build()
        .error_on_failure();
    let (mut coreml, coreml_init_ms) = build_model(&snapshot, vec![coreml_provider])
        .expect("CoreML CPU+GPU execution provider initializes");

    let prefix = manifest.query_prefix.as_deref().unwrap_or_default();
    let texts = [
        "local issue retrieval without lexical overlap",
        "permission loss cleanup policy",
        "한국어로 검색한 영문 이슈 원인",
        "댓글에만 기록된 동기화 오류",
        "repository allowlist reconciliation",
        "stale publication snapshot",
        "교차언어 검색 보완",
        "exact identifier stays lexical",
    ]
    .into_iter()
    .map(|text| format!("{prefix}{text}"))
    .collect::<Vec<_>>();
    let (cpu_vectors, cpu_samples) = measure(&mut cpu, &texts).expect("CPU probe completes");
    let (coreml_vectors, coreml_samples) =
        measure(&mut coreml, &texts).expect("CoreML probe completes");
    let minimum_cosine = cpu_vectors
        .iter()
        .zip(&coreml_vectors)
        .map(|(cpu, coreml)| cosine(cpu, coreml))
        .fold(1.0_f64, f64::min);
    let cpu_p50 = percentile(&cpu_samples, 0.50);
    let coreml_p50 = percentile(&coreml_samples, 0.50);
    let report = CoreMlProbeReport {
        schema_version: "qgh.coreml_model_eval.v2",
        model_id: manifest_model_id(manifest),
        resolved_revision: manifest_revision(manifest),
        manifest_sha256: digest_hex(&manifest_bytes),
        prepared_snapshot_sha256: prepared_snapshot_digest(&prepared_artifacts),
        prepared_artifacts,
        git_head,
        git_worktree_clean,
        test_binary_sha256,
        execution_provider: "CoreMLExecutionProvider/CPUAndGPU",
        cpu_fallback_allowed: true,
        batch_size: texts.len(),
        measured_runs: cpu_samples.len(),
        output_dimension: manifest.output_dimension,
        cpu_init_ms,
        coreml_init_ms,
        cpu_warm_p50_ms: cpu_p50,
        cpu_warm_p95_ms: percentile(&cpu_samples, 0.95),
        coreml_warm_p50_ms: coreml_p50,
        coreml_warm_p95_ms: percentile(&coreml_samples, 0.95),
        coreml_speedup_at_p50: cpu_p50 / coreml_p50,
        minimum_cpu_coreml_cosine: minimum_cosine,
        parity_passed: minimum_cosine >= 0.99,
    };
    assert!(report.parity_passed, "CoreML vectors diverged from CPU");
    fs::create_dir_all(&output_root).expect("eval output root is writable");
    fs::write(
        output_root.join("coreml-model-eval.json"),
        serde_json::to_vec_pretty(&report).expect("CoreML report serializes"),
    )
    .expect("CoreML report is written");
    println!("{{\"artifact\":\"coreml-model-eval.json\"}}");
}

fn build_model(
    snapshot: &PreparedModelSnapshot,
    providers: Vec<ExecutionProviderDispatch>,
) -> Result<(TextEmbedding, f64), DynError> {
    let manifest = &snapshot.manifest;
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: artifact(snapshot, ArtifactRole::Tokenizer)?,
        config_file: artifact(snapshot, ArtifactRole::Config)?,
        special_tokens_map_file: artifact(snapshot, ArtifactRole::SpecialTokensMap)?,
        tokenizer_config_file: artifact(snapshot, ArtifactRole::TokenizerConfig)?,
    };
    let model = UserDefinedEmbeddingModel::new(
        artifact(snapshot, ArtifactRole::OnnxModel)?,
        tokenizer_files,
    )
    .with_pooling(match manifest.pooling {
        PoolingKind::Cls => fastembed::Pooling::Cls,
        PoolingKind::Mean => fastembed::Pooling::Mean,
    })
    .with_quantization(QuantizationMode::None);
    let started = Instant::now();
    let model = TextEmbedding::try_new_from_user_defined(
        model,
        InitOptionsUserDefined::new()
            .with_max_length(manifest.max_length)
            .with_intra_threads(4)
            .with_execution_providers(providers),
    )?;
    Ok((model, started.elapsed().as_secs_f64() * 1_000.0))
}

fn artifact(snapshot: &PreparedModelSnapshot, role: ArtifactRole) -> Result<Vec<u8>, DynError> {
    let paths = snapshot.paths_for_role(role).collect::<Vec<_>>();
    if paths.len() != 1 {
        return Err("prepared model artifact role is not unique".into());
    }
    Ok(fs::read(paths[0])?)
}

fn prepared_artifact_identities(manifest: &ModelManifestV1) -> Vec<PreparedArtifactIdentity> {
    let mut identities = manifest
        .artifacts
        .iter()
        .map(|artifact| PreparedArtifactIdentity {
            role: artifact.role,
            relative_path: artifact.relative_path.clone(),
            sha256: artifact.sha256.clone(),
            byte_size: artifact.byte_size,
        })
        .collect::<Vec<_>>();
    identities.sort_by(|left, right| {
        left.relative_path
            .cmp(&right.relative_path)
            .then_with(|| left.sha256.cmp(&right.sha256))
    });
    identities
}

fn prepared_snapshot_digest(artifacts: &[PreparedArtifactIdentity]) -> String {
    digest_hex(&serde_json::to_vec(artifacts).expect("artifact identities serialize"))
}

fn sha256_file(path: &std::path::Path) -> Result<String, DynError> {
    Ok(digest_hex(&fs::read(path)?))
}

fn digest_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn command_text(program: &str, arguments: &[&str]) -> Result<String, DynError> {
    let output = Command::new(program).args(arguments).output()?;
    if !output.status.success() {
        return Err("identity command failed".into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn measure(
    model: &mut TextEmbedding,
    texts: &[String],
) -> Result<(Vec<Vec<f32>>, Vec<f64>), DynError> {
    const MEASURED_RUNS: usize = 10;
    model.embed(texts, Some(8))?;
    let mut samples = Vec::with_capacity(MEASURED_RUNS);
    let mut vectors = Vec::new();
    for run in 0..MEASURED_RUNS {
        let started = Instant::now();
        let current = model.embed(texts, Some(8))?;
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
        if run == 0 {
            vectors = current;
        }
    }
    Ok((vectors, samples))
}

fn cosine(left: &[f32], right: &[f32]) -> f64 {
    assert_eq!(left.len(), right.len());
    let dot = left
        .iter()
        .zip(right)
        .map(|(left, right)| f64::from(*left) * f64::from(*right))
        .sum::<f64>();
    let left_norm = left
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    let right_norm = right
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    dot / (left_norm * right_norm)
}

fn percentile(values: &[f64], percentile: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let index = ((sorted.len() - 1) as f64 * percentile).ceil() as usize;
    sorted[index]
}

fn manifest_model_id(manifest: &ModelManifestV1) -> String {
    match &manifest.model_source {
        qgh::embedding::ModelSourceV1::Hf { model_id, .. } => model_id.clone(),
        qgh::embedding::ModelSourceV1::Local { declared_id } => declared_id.clone(),
    }
}

fn manifest_revision(manifest: &ModelManifestV1) -> String {
    match &manifest.model_source {
        qgh::embedding::ModelSourceV1::Hf {
            resolved_revision, ..
        } => resolved_revision.clone(),
        qgh::embedding::ModelSourceV1::Local { .. } => "local".to_string(),
    }
}
