#![cfg(all(feature = "fastembed-provider", target_os = "macos"))]

use fastembed::{
    ExecutionProviderDispatch, InitOptionsUserDefined, QuantizationMode, TextEmbedding,
    TokenizerFiles, UserDefinedEmbeddingModel,
};
use ort::ep::{
    coreml::{ComputeUnits, ModelFormat, SpecializationStrategy},
    CoreML,
};
use qgh::embedding::{ArtifactRole, ModelManifestV1, PoolingKind, QuantizationKind};
use serde::Serialize;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
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

    let manifest = ModelManifestV1::from_json_slice(
        &fs::read(&manifest_path).expect("prepared model manifest is readable"),
    )
    .expect("prepared model manifest is valid");
    assert_eq!(manifest.quantization, QuantizationKind::None);
    assert_eq!(manifest.output_dimension, 384);

    let (mut cpu, cpu_init_ms) = build_model(&manifest_path, &manifest, Vec::new())
        .expect("CPU reference model initializes");
    let coreml_provider = CoreML::default()
        .with_model_format(ModelFormat::MLProgram)
        .with_specialization_strategy(SpecializationStrategy::FastPrediction)
        .with_compute_units(ComputeUnits::CPUAndGPU)
        .build()
        .error_on_failure();
    let (mut coreml, coreml_init_ms) =
        build_model(&manifest_path, &manifest, vec![coreml_provider])
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
        schema_version: "qgh.coreml_model_eval.v1",
        model_id: manifest_model_id(&manifest),
        resolved_revision: manifest_revision(&manifest),
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
    manifest_path: &Path,
    manifest: &ModelManifestV1,
    providers: Vec<ExecutionProviderDispatch>,
) -> Result<(TextEmbedding, f64), DynError> {
    let root = manifest_path.parent().ok_or("manifest root is missing")?;
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: artifact(root, manifest, ArtifactRole::Tokenizer)?,
        config_file: artifact(root, manifest, ArtifactRole::Config)?,
        special_tokens_map_file: artifact(root, manifest, ArtifactRole::SpecialTokensMap)?,
        tokenizer_config_file: artifact(root, manifest, ArtifactRole::TokenizerConfig)?,
    };
    let model = UserDefinedEmbeddingModel::new(
        artifact(root, manifest, ArtifactRole::OnnxModel)?,
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

fn artifact(
    root: &Path,
    manifest: &ModelManifestV1,
    role: ArtifactRole,
) -> Result<Vec<u8>, DynError> {
    let matches = manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.role == role)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err("prepared model artifact role is not unique".into());
    }
    Ok(fs::read(root.join(&matches[0].relative_path))?)
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
