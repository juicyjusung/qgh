use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(feature = "fastembed-provider")]
use std::io::{Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
#[cfg(feature = "fastembed-provider")]
use std::sync::Arc;
use std::sync::Mutex;

#[cfg(test)]
thread_local! {
    static TOKENIZER_ONLY_ARTIFACT_BYTES: std::cell::RefCell<BTreeMap<ArtifactRole, u64>> =
        const { std::cell::RefCell::new(BTreeMap::new()) };
}

pub use crate::context::{
    prepare_embedding_input, EmbeddingSourceContext, PreparedEmbeddingInput,
    METADATA_CONTEXT_TEMPLATE_VERSION,
};

pub type EmbeddingVector = Vec<f32>;

pub const DEFAULT_HF_MODEL_ID: &str = "Snowflake/snowflake-arctic-embed-l-v2.0";
/// Pinned commit of the default Hugging Face model. A mutable revision
/// (e.g. "main") would let upstream file changes alter inferred pooling,
/// query prefix, and dimension without changing the fingerprint's
/// (model_id, model_revision) pair, silently invalidating stored
/// embeddings. Users can opt into a moving revision via `@<revision>`.
pub const DEFAULT_HF_MODEL_REVISION: &str = "ac6544c8a46e00af67e330e85a9028c66b8cfd9a";
// fp32 rather than model_quantized.onnx: fastembed refuses batching for
// dynamically quantized models (per-batch quantization ranges make
// embeddings incompatible across batches), and single-batch inference over
// a full corpus needs tens of GB. fp32 embeds with a small bounded batch.
pub const DEFAULT_HF_MODEL_FILE: &str = "onnx/model.onnx";
pub const DEFAULT_QUERY_PREFIX: &str = "query: ";
pub const BUILTIN_PRESET_IDS: [&str; 4] = [
    "arctic-m-v2-fp32",
    "granite-97m-multilingual-r2-int8-static",
    "granite-311m-multilingual-r2-int8-static",
    "arctic-l-v2-fp32",
];
const ARCTIC_M_V2_REVISION: &str = "95c2741480856aa9666782eb4afe11959938017f";
const GRANITE_97M_R2_REVISION: &str = "835ad14087e140460703cf0fae09f97d469d65c2";
const GRANITE_311M_R2_REVISION: &str = "44399559930365213510b1ee2eb15ded83374f0e";
pub const HUGGINGFACE_ENDPOINT: &str = "https://huggingface.co";
pub const EMBEDDING_FINGERPRINT_SCHEMA_VERSION: &str = "qgh.embedding_fingerprint.v1";
pub const MODEL_MANIFEST_SCHEMA_VERSION: &str = "qgh.model_manifest.v1";
const MAX_PREPARED_ALIAS_BYTES: u64 = 64 * 1024;
const MAX_MODEL_MANIFEST_BYTES: u64 = 1024 * 1024;
// v2: chunks slice the tokenizer's canonical (normalized) text instead of
// the raw source body, so v1 chunk bodies and embeddings are not comparable.
pub const CHUNKER_VERSION: &str = "qgh.chunker.v2";
pub const SOURCE_SCHEMA_VERSION: &str = "qgh.source_schema.v1";
pub const LOCAL_MODEL_REVISION: &str = "local_path";
const MODULES_FILE: &str = "modules.json";
const DEFAULT_POOLING_CONFIG_FILE: &str = "1_Pooling/config.json";
const TOKENIZER_FILES: [&str; 4] = [
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];
const TOKENIZER_ARTIFACT_ROLES: [ArtifactRole; 4] = [
    ArtifactRole::Tokenizer,
    ArtifactRole::Config,
    ArtifactRole::SpecialTokensMap,
    ArtifactRole::TokenizerConfig,
];
const MAX_TOKENIZER_ARTIFACT_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TOKENIZER_SNAPSHOT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenSpan {
    pub start: usize,
    pub end: usize,
}

pub trait EmbeddingProvider: Send + Sync {
    fn embed_documents(
        &self,
        texts: &[&str],
    ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError>;
    fn embed_query(&self, text: &str) -> Result<EmbeddingVector, EmbeddingProviderError>;
}

pub trait EmbeddingEngine: Send + Sync {
    fn embed_texts(&self, texts: &[String])
        -> Result<Vec<EmbeddingVector>, EmbeddingProviderError>;
}

/// Tokenization result whose `spans` are byte offsets into `text`, which may
/// be a normalized form of the input rather than the input itself.
pub struct TokenizedText {
    pub text: String,
    pub spans: Vec<TokenSpan>,
    /// Original source text and one source span for every normalized token.
    /// The spans are byte offsets into `original_text`.
    pub original_text: String,
    pub original_spans: Vec<TokenSpan>,
}

pub trait EmbeddingTokenizer: Send + Sync {
    fn tokenize(&self, text: &str) -> Result<Vec<TokenSpan>, EmbeddingProviderError>;

    /// Tokenize and return the canonical text the spans are valid against.
    /// Tokenizers whose offsets do not map back to the original input (e.g.
    /// normalizers that drop bytes) must override this and return the
    /// normalized text the offsets refer to.
    fn tokenize_canonical(&self, text: &str) -> Result<TokenizedText, EmbeddingProviderError> {
        let spans = self.tokenize(text)?;
        Ok(TokenizedText {
            text: text.to_string(),
            spans: spans.clone(),
            original_text: text.to_string(),
            original_spans: spans,
        })
    }

    fn count_tokens(&self, text: &str) -> Result<usize, EmbeddingProviderError> {
        Ok(self.tokenize(text)?.len())
    }
}

pub struct LocalEmbeddingProvider<E> {
    engine: E,
    query_prefix: String,
    document_prefix: String,
    normalization: NormalizationKind,
    native_dimension: Option<usize>,
    output_dimension: Option<usize>,
    dimension: Mutex<Option<usize>>,
}

impl<E> LocalEmbeddingProvider<E> {
    pub fn new(engine: E, query_prefix: impl Into<String>) -> Self {
        Self {
            engine,
            query_prefix: query_prefix.into(),
            document_prefix: String::new(),
            normalization: NormalizationKind::None,
            native_dimension: None,
            output_dimension: None,
            dimension: Mutex::new(None),
        }
    }

    pub fn with_contract(
        engine: E,
        contract: EmbeddingRuntimeContract,
    ) -> Result<Self, EmbeddingProviderError> {
        contract.validate()?;
        Ok(Self {
            engine,
            query_prefix: contract.query_prefix.unwrap_or_default(),
            document_prefix: contract.document_prefix.unwrap_or_default(),
            normalization: contract.normalization,
            native_dimension: Some(contract.native_dimension),
            output_dimension: Some(contract.output_dimension),
            dimension: Mutex::new(None),
        })
    }

    pub fn dimension(&self) -> Option<usize> {
        *self.dimension.lock().expect("dimension mutex poisoned")
    }

    pub fn query_prefix(&self) -> &str {
        &self.query_prefix
    }
}

pub fn validate_batch_comparability(
    provider: &dyn EmbeddingProvider,
    text: &str,
) -> Result<(), EmbeddingProviderError> {
    let single = provider
        .embed_documents(&[text])?
        .into_iter()
        .next()
        .ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.empty_result",
                "Embedding engine returned no smoke vector.",
            )
        })?;
    let middle_batch = provider.embed_documents(&["qgh smoke left", text, "qgh smoke right"])?;
    let front_batch = provider.embed_documents(&[text, "qgh smoke tail"])?;
    let comparisons = [
        middle_batch.get(1).ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.empty_result",
                "Embedding engine returned an incomplete smoke batch.",
            )
        })?,
        front_batch.first().ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.empty_result",
                "Embedding engine returned an incomplete smoke batch.",
            )
        })?,
    ];
    for comparison in comparisons {
        let cosine = cosine_similarity(&single, comparison)?;
        if cosine < 0.99999 {
            return Err(EmbeddingProviderError::structured(
                "embedding.batch_incomparable",
                "Embedding artifact produced batch-dependent vectors.",
            )
            .with_details(json!({ "minimum_cosine": 0.99999, "actual_cosine": cosine })));
        }
    }
    Ok(())
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> Result<f32, EmbeddingProviderError> {
    if left.is_empty() || left.len() != right.len() {
        return Err(EmbeddingProviderError::structured(
            "embedding.batch_incomparable",
            "Embedding smoke vectors have incompatible dimensions.",
        ));
    }
    let dot = left
        .iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum::<f32>();
    let left_norm = left.iter().map(|value| value * value).sum::<f32>().sqrt();
    let right_norm = right.iter().map(|value| value * value).sum::<f32>().sqrt();
    if left_norm <= f32::EPSILON || right_norm <= f32::EPSILON {
        return Err(EmbeddingProviderError::structured(
            "embedding.batch_incomparable",
            "Embedding smoke vector has zero norm.",
        ));
    }
    Ok(dot / (left_norm * right_norm))
}

impl<E: EmbeddingEngine> EmbeddingProvider for LocalEmbeddingProvider<E> {
    fn embed_documents(
        &self,
        texts: &[&str],
    ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
        let prepared = texts
            .iter()
            .map(|text| format!("{}{}", self.document_prefix, text))
            .collect::<Vec<_>>();
        let vectors = self.process_vectors(self.engine.embed_texts(&prepared)?)?;
        self.record_dimension(&vectors)?;
        Ok(vectors)
    }

    fn embed_query(&self, text: &str) -> Result<EmbeddingVector, EmbeddingProviderError> {
        let prepared = vec![format!("{}{}", self.query_prefix, text)];
        let mut vectors = self.process_vectors(self.engine.embed_texts(&prepared)?)?;
        self.record_dimension(&vectors)?;
        vectors.pop().ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.empty_result",
                "Embedding engine returned no query vector.",
            )
        })
    }
}

impl<E> LocalEmbeddingProvider<E> {
    fn process_vectors(
        &self,
        mut vectors: Vec<EmbeddingVector>,
    ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
        for vector in &mut vectors {
            if let Some(native_dimension) = self.native_dimension {
                if vector.len() != native_dimension {
                    return Err(EmbeddingProviderError::structured(
                        "embedding.native_dimension_mismatch",
                        "Embedding engine output does not match the manifest native dimension.",
                    )
                    .with_details(json!({
                        "expected_dimension": native_dimension,
                        "actual_dimension": vector.len()
                    })));
                }
            }
            if let Some(output_dimension) = self.output_dimension {
                vector.truncate(output_dimension);
            }
            if self.normalization == NormalizationKind::L2 {
                let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
                if !norm.is_finite() || norm <= f32::EPSILON {
                    return Err(EmbeddingProviderError::structured(
                        "embedding.normalization_failed",
                        "Embedding vector cannot be L2 normalized.",
                    ));
                }
                for value in vector {
                    *value /= norm;
                }
            }
        }
        Ok(vectors)
    }

    fn record_dimension(&self, vectors: &[EmbeddingVector]) -> Result<(), EmbeddingProviderError> {
        for vector in vectors {
            if vector.is_empty() {
                return Err(EmbeddingProviderError::structured(
                    "embedding.empty_vector",
                    "Embedding engine returned an empty vector.",
                ));
            }
            let mut dimension = self.dimension.lock().expect("dimension mutex poisoned");
            match *dimension {
                Some(expected) if expected != vector.len() => {
                    return Err(EmbeddingProviderError::structured(
                        "embedding.dimension_mismatch",
                        "Embedding engine returned inconsistent dimensions.",
                    )
                    .with_details(json!({
                        "expected_dimension": expected,
                        "actual_dimension": vector.len()
                    })));
                }
                Some(_) => {}
                None => {
                    *dimension = Some(vector.len());
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolingKind {
    Cls,
    Mean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizationKind {
    None,
    L2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuantizationKind {
    None,
    Static,
    Dynamic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProviderKind {
    Fastembed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenizerKind {
    HfTokenizerJson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRole {
    OnnxModel,
    OnnxExternalData,
    Tokenizer,
    Config,
    SpecialTokensMap,
    TokenizerConfig,
}

#[cfg(test)]
pub(crate) fn reset_tokenizer_only_artifact_bytes() {
    TOKENIZER_ONLY_ARTIFACT_BYTES.with(|bytes| bytes.borrow_mut().clear());
}

#[cfg(test)]
pub(crate) fn tokenizer_only_artifact_bytes() -> BTreeMap<ArtifactRole, u64> {
    TOKENIZER_ONLY_ARTIFACT_BYTES.with(|bytes| bytes.borrow().clone())
}

#[cfg(test)]
fn record_tokenizer_only_artifact_bytes(role: ArtifactRole, bytes: u64) {
    TOKENIZER_ONLY_ARTIFACT_BYTES.with(|recorded| {
        *recorded.borrow_mut().entry(role).or_default() += bytes;
    });
}

fn validate_tokenizer_artifact_sizes(
    artifacts: impl IntoIterator<Item = (ArtifactRole, u64)>,
) -> Result<(), EmbeddingProviderError> {
    let mut total = 0_u64;
    for (role, byte_size) in artifacts {
        if !TOKENIZER_ARTIFACT_ROLES.contains(&role) {
            continue;
        }
        if byte_size > MAX_TOKENIZER_ARTIFACT_BYTES {
            return Err(EmbeddingProviderError::structured(
                "embedding.tokenizer_artifact_too_large",
                "Tokenizer artifact exceeds the supported size.",
            )
            .with_details(json!({ "role": role })));
        }
        total = total.checked_add(byte_size).ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.tokenizer_artifact_too_large",
                "Tokenizer artifact sizes exceed the supported range.",
            )
        })?;
        if total > MAX_TOKENIZER_SNAPSHOT_BYTES {
            return Err(EmbeddingProviderError::structured(
                "embedding.tokenizer_artifact_too_large",
                "Tokenizer snapshot exceeds the supported cumulative size.",
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelSourceV1 {
    Hf {
        model_id: String,
        resolved_revision: String,
    },
    Local {
        declared_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelArtifactV1 {
    pub role: ArtifactRole,
    pub relative_path: String,
    pub sha256: String,
    pub byte_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_initializer_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelManifestV1 {
    pub schema_version: String,
    pub preset_id: Option<String>,
    pub provider: ModelProviderKind,
    pub model_source: ModelSourceV1,
    pub artifacts: Vec<ModelArtifactV1>,
    pub tokenizer: TokenizerKind,
    pub query_prefix: Option<String>,
    pub document_prefix: Option<String>,
    pub pooling: PoolingKind,
    pub normalization: NormalizationKind,
    pub native_dimension: usize,
    pub output_dimension: usize,
    pub max_length: usize,
    pub quantization: QuantizationKind,
    pub context_template_version: String,
}

impl ModelManifestV1 {
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, EmbeddingProviderError> {
        let value: Value = serde_json::from_slice(bytes).map_err(manifest_parse_error)?;
        let object = value.as_object().ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.manifest_invalid",
                "Prepared model manifest must be a JSON object.",
            )
        })?;
        for field in [
            "schema_version",
            "preset_id",
            "provider",
            "model_source",
            "artifacts",
            "tokenizer",
            "query_prefix",
            "document_prefix",
            "pooling",
            "normalization",
            "native_dimension",
            "output_dimension",
            "max_length",
            "quantization",
            "context_template_version",
        ] {
            if !object.contains_key(field) {
                return Err(EmbeddingProviderError::structured(
                    "embedding.manifest_invalid",
                    "Prepared model manifest is missing a required field.",
                )
                .with_details(json!({ "field": field })));
            }
        }
        let manifest: Self = serde_json::from_value(value).map_err(manifest_parse_error)?;
        manifest.validate_contract()?;
        Ok(manifest)
    }

    pub fn validate_contract(&self) -> Result<(), EmbeddingProviderError> {
        if self.schema_version != MODEL_MANIFEST_SCHEMA_VERSION {
            return Err(EmbeddingProviderError::structured(
                "embedding.manifest_schema_unsupported",
                "Prepared model manifest schema_version is unsupported.",
            ));
        }
        if let ModelSourceV1::Hf {
            model_id,
            resolved_revision,
        } = &self.model_source
        {
            if model_id.is_empty() || !is_commit_sha(resolved_revision) {
                return Err(EmbeddingProviderError::structured(
                    "embedding.manifest_revision_invalid",
                    "Hugging Face prepared models require an immutable 40-character commit SHA.",
                ));
            }
        }
        if self.native_dimension == 0
            || self.output_dimension == 0
            || self.output_dimension > self.native_dimension
            || self.max_length == 0
        {
            return Err(EmbeddingProviderError::structured(
                "embedding.manifest_contract_invalid",
                "Prepared model dimensions and max_length are invalid.",
            ));
        }
        if self.context_template_version != METADATA_CONTEXT_TEMPLATE_VERSION {
            return Err(EmbeddingProviderError::structured(
                "embedding.manifest_context_template_unsupported",
                "Prepared model context_template_version is unsupported.",
            ));
        }
        if self.quantization == QuantizationKind::Dynamic {
            return Err(EmbeddingProviderError::structured(
                "embedding.dynamic_quantization_unsupported",
                "Dynamic quantization is not supported for persistent embedding generations.",
            ));
        }
        let graph_count = self
            .artifacts
            .iter()
            .filter(|artifact| artifact.role == ArtifactRole::OnnxModel)
            .count();
        if graph_count != 1 {
            return Err(EmbeddingProviderError::structured(
                "embedding.manifest_artifacts_invalid",
                "Prepared model manifest must declare exactly one ONNX graph.",
            ));
        }
        for required_role in [
            ArtifactRole::Tokenizer,
            ArtifactRole::Config,
            ArtifactRole::SpecialTokensMap,
            ArtifactRole::TokenizerConfig,
        ] {
            if self
                .artifacts
                .iter()
                .filter(|artifact| artifact.role == required_role)
                .count()
                != 1
            {
                return Err(EmbeddingProviderError::structured(
                    "embedding.manifest_artifacts_invalid",
                    "Prepared model manifest must declare each tokenizer runtime artifact exactly once.",
                ));
            }
        }
        let mut paths = std::collections::BTreeSet::new();
        for artifact in &self.artifacts {
            if !paths.insert(&artifact.relative_path)
                || !is_sha256(&artifact.sha256)
                || artifact.byte_size == 0
                || (artifact.role == ArtifactRole::OnnxExternalData)
                    != artifact.external_initializer_name.is_some()
            {
                return Err(EmbeddingProviderError::structured(
                    "embedding.manifest_artifacts_invalid",
                    "Prepared model artifact declarations are invalid.",
                ));
            }
        }
        Ok(())
    }

    pub fn hash(&self) -> String {
        let encoded = serde_json::to_vec(self).expect("validated model manifest serializes");
        hex_digest(&Sha256::digest(encoded))
    }

    pub fn runtime_contract(&self) -> EmbeddingRuntimeContract {
        EmbeddingRuntimeContract {
            query_prefix: self.query_prefix.clone(),
            document_prefix: self.document_prefix.clone(),
            normalization: self.normalization,
            native_dimension: self.native_dimension,
            output_dimension: self.output_dimension,
        }
    }
}

fn manifest_parse_error(error: serde_json::Error) -> EmbeddingProviderError {
    EmbeddingProviderError::structured(
        "embedding.manifest_invalid",
        "Prepared model manifest is not valid strict JSON.",
    )
    .with_details(json!({ "error": error.to_string() }))
}

fn is_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|character| character.is_ascii_hexdigit())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|character| character.is_ascii_hexdigit())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingRuntimeContract {
    pub query_prefix: Option<String>,
    pub document_prefix: Option<String>,
    pub normalization: NormalizationKind,
    pub native_dimension: usize,
    pub output_dimension: usize,
}

impl EmbeddingRuntimeContract {
    fn validate(&self) -> Result<(), EmbeddingProviderError> {
        if self.native_dimension == 0
            || self.output_dimension == 0
            || self.output_dimension > self.native_dimension
        {
            return Err(EmbeddingProviderError::structured(
                "embedding.manifest_contract_invalid",
                "Embedding runtime dimensions are invalid.",
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct PreparedModelSnapshot {
    pub manifest: ModelManifestV1,
    pub root: PathBuf,
    paths: BTreeMap<ArtifactRole, Vec<PathBuf>>,
    #[cfg(feature = "fastembed-provider")]
    runtime_state: Arc<Mutex<VerifiedRuntimeState>>,
}

impl fmt::Debug for PreparedModelSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedModelSnapshot")
            .field("manifest", &self.manifest)
            .field("root", &self.root)
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct PreparedModelInspection {
    manifest: ModelManifestV1,
    manifest_hash: String,
    root: PathBuf,
    artifacts: Vec<PreparedArtifactInspection>,
    artifact_stamp: String,
}

#[derive(Debug, Clone)]
pub struct PreparedManifestInspection {
    manifest: ModelManifestV1,
    manifest_hash: String,
    root: PathBuf,
}

impl PreparedManifestInspection {
    pub fn manifest(&self) -> &ModelManifestV1 {
        &self.manifest
    }

    pub fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }
}

impl PreparedModelInspection {
    pub fn manifest(&self) -> &ModelManifestV1 {
        &self.manifest
    }

    pub fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }

    pub fn artifact_stamp(&self) -> &str {
        &self.artifact_stamp
    }
}

#[derive(Debug, Clone)]
struct PreparedArtifactInspection {
    role: ArtifactRole,
    relative_path: String,
    canonical_path: PathBuf,
    expected_sha256: String,
    expected_byte_size: u64,
    identity: ArtifactFileIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArtifactFileIdentity {
    byte_size: u64,
    modified_nanos: Option<u128>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    ctime_seconds: i64,
    #[cfg(unix)]
    ctime_nanos: i64,
}

#[cfg(feature = "fastembed-provider")]
struct VerifiedRuntimeArtifact {
    role: ArtifactRole,
    relative_path: String,
    expected_sha256: String,
    expected_byte_size: u64,
    identity: ArtifactFileIdentity,
    file: File,
}

#[cfg(feature = "fastembed-provider")]
struct VerifiedRuntimeState {
    artifacts: Vec<VerifiedRuntimeArtifact>,
    loaded_artifacts: BTreeMap<ArtifactRole, Vec<PreparedRuntimeArtifact>>,
    tokenizer: Option<tokenizers::Tokenizer>,
    consumed: bool,
}

#[cfg(feature = "fastembed-provider")]
struct PreparedRuntimeArtifact {
    relative_path: String,
    bytes: Vec<u8>,
}

#[cfg(feature = "fastembed-provider")]
struct PreparedRuntimePayload {
    artifacts: BTreeMap<ArtifactRole, Vec<PreparedRuntimeArtifact>>,
}

impl PreparedModelSnapshot {
    pub fn path_for_role(&self, role: ArtifactRole) -> Option<&Path> {
        self.paths
            .get(&role)
            .and_then(|paths| paths.first())
            .map(PathBuf::as_path)
    }

    pub fn paths_for_role(&self, role: ArtifactRole) -> impl Iterator<Item = &Path> {
        self.paths
            .get(&role)
            .into_iter()
            .flatten()
            .map(PathBuf::as_path)
    }

    #[cfg(feature = "fastembed-provider")]
    fn runtime_tokenizer(&self) -> Result<tokenizers::Tokenizer, EmbeddingProviderError> {
        let mut state = self
            .runtime_state
            .lock()
            .expect("prepared runtime state mutex poisoned");
        ensure_runtime_tokenizer(&mut state)?;
        state.tokenizer.clone().ok_or_else(runtime_payload_error)
    }

    #[cfg(feature = "fastembed-provider")]
    fn take_runtime_payload(&self) -> Result<PreparedRuntimePayload, EmbeddingProviderError> {
        let mut state = self
            .runtime_state
            .lock()
            .expect("prepared runtime state mutex poisoned");
        if state.consumed {
            return Err(runtime_payload_error());
        }
        ensure_runtime_tokenizer(&mut state)?;
        checked_runtime_payload_size(
            state
                .artifacts
                .iter()
                .map(|artifact| artifact.expected_byte_size),
        )?;
        for index in 0..state.artifacts.len() {
            if !runtime_artifact_is_loaded(&state, index) {
                load_runtime_artifact(&mut state, index)?;
            }
        }
        let payload = PreparedRuntimePayload {
            artifacts: std::mem::take(&mut state.loaded_artifacts),
        };
        state.consumed = true;
        Ok(payload)
    }
}

#[cfg(feature = "fastembed-provider")]
impl PreparedRuntimePayload {
    fn take_one(&mut self, role: ArtifactRole) -> Result<Vec<u8>, EmbeddingProviderError> {
        let mut artifacts = self.artifacts.remove(&role).ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.model_file_missing",
                "Prepared runtime payload is missing a required artifact.",
            )
            .with_details(json!({ "role": role }))
        })?;
        if artifacts.len() != 1 {
            return Err(EmbeddingProviderError::structured(
                "embedding.manifest_artifacts_invalid",
                "Prepared runtime payload has an invalid artifact cardinality.",
            )
            .with_details(json!({ "role": role })));
        }
        Ok(artifacts.pop().expect("one runtime artifact").bytes)
    }

    fn take_relative(
        &mut self,
        role: ArtifactRole,
        relative_path: &str,
    ) -> Result<Vec<u8>, EmbeddingProviderError> {
        let artifacts = self.artifacts.get_mut(&role).ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.model_file_missing",
                "Prepared runtime payload is missing a required artifact.",
            )
            .with_details(json!({ "role": role }))
        })?;
        let index = artifacts
            .iter()
            .position(|artifact| artifact.relative_path == relative_path)
            .ok_or_else(|| {
                EmbeddingProviderError::structured(
                    "embedding.model_file_missing",
                    "Prepared runtime payload is missing a declared artifact.",
                )
                .with_details(json!({ "role": role }))
            })?;
        Ok(artifacts.swap_remove(index).bytes)
    }
}

#[cfg(feature = "fastembed-provider")]
fn ensure_runtime_tokenizer(
    state: &mut VerifiedRuntimeState,
) -> Result<(), EmbeddingProviderError> {
    if state.tokenizer.is_some() {
        return Ok(());
    }
    if state.consumed {
        return Err(runtime_payload_error());
    }
    let tokenizer_index = state
        .artifacts
        .iter()
        .position(|artifact| artifact.role == ArtifactRole::Tokenizer)
        .ok_or_else(runtime_payload_error)?;
    if !runtime_artifact_is_loaded(state, tokenizer_index) {
        load_runtime_artifact(state, tokenizer_index)?;
    }
    let tokenizer_bytes = state
        .loaded_artifacts
        .get(&ArtifactRole::Tokenizer)
        .and_then(|artifacts| artifacts.first())
        .map(|artifact| artifact.bytes.as_slice())
        .ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.model_file_missing",
                "Prepared runtime payload is missing a tokenizer artifact.",
            )
            .with_details(json!({ "role": ArtifactRole::Tokenizer }))
        })?;
    let tokenizer = tokenizers::Tokenizer::from_bytes(tokenizer_bytes).map_err(|_| {
        EmbeddingProviderError::structured(
            "embedding.tokenizer_init_failed",
            "Failed to initialize prepared embedding tokenizer.",
        )
        .with_details(json!({ "role": ArtifactRole::Tokenizer }))
    })?;
    state.tokenizer = Some(tokenizer);
    Ok(())
}

#[cfg(feature = "fastembed-provider")]
fn runtime_artifact_is_loaded(state: &VerifiedRuntimeState, index: usize) -> bool {
    let artifact = &state.artifacts[index];
    state
        .loaded_artifacts
        .get(&artifact.role)
        .is_some_and(|loaded| {
            loaded
                .iter()
                .any(|loaded| loaded.relative_path == artifact.relative_path)
        })
}

#[cfg(feature = "fastembed-provider")]
fn load_runtime_artifact(
    state: &mut VerifiedRuntimeState,
    index: usize,
) -> Result<(), EmbeddingProviderError> {
    let (role, relative_path, bytes) = {
        let artifact = &mut state.artifacts[index];
        (
            artifact.role,
            artifact.relative_path.clone(),
            read_verified_runtime_artifact(artifact)?,
        )
    };
    state
        .loaded_artifacts
        .entry(role)
        .or_default()
        .push(PreparedRuntimeArtifact {
            relative_path,
            bytes,
        });
    Ok(())
}

#[cfg(feature = "fastembed-provider")]
fn checked_runtime_payload_size(
    sizes: impl IntoIterator<Item = u64>,
) -> Result<usize, EmbeddingProviderError> {
    sizes.into_iter().try_fold(0usize, |total, size| {
        let size = usize::try_from(size).map_err(|_| {
            EmbeddingProviderError::structured(
                "embedding.artifact_size_mismatch",
                "Prepared model artifact size exceeds the runtime address space.",
            )
        })?;
        total.checked_add(size).ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.artifact_size_mismatch",
                "Prepared model artifacts exceed the runtime address space.",
            )
        })
    })
}

#[cfg(feature = "fastembed-provider")]
fn read_verified_runtime_artifact(
    artifact: &mut VerifiedRuntimeArtifact,
) -> Result<Vec<u8>, EmbeddingProviderError> {
    let before = artifact_file_identity(&artifact.file.metadata()?);
    if before != artifact.identity {
        return Err(artifact_changed_error());
    }
    artifact.file.seek(SeekFrom::Start(0))?;
    let capacity = usize::try_from(artifact.expected_byte_size).map_err(|_| {
        EmbeddingProviderError::structured(
            "embedding.artifact_size_mismatch",
            "Prepared model artifact size exceeds the runtime address space.",
        )
    })?;
    let read_limit = artifact.expected_byte_size.checked_add(1).ok_or_else(|| {
        EmbeddingProviderError::structured(
            "embedding.artifact_size_mismatch",
            "Prepared model artifact size exceeds the supported range.",
        )
    })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| {
        EmbeddingProviderError::structured(
            "embedding.artifact_size_mismatch",
            "Prepared model artifact cannot fit in runtime memory.",
        )
    })?;
    (&mut artifact.file)
        .take(read_limit)
        .read_to_end(&mut bytes)?;
    let after = artifact_file_identity(&artifact.file.metadata()?);
    if after != before {
        return Err(artifact_changed_error());
    }
    if bytes.len() as u64 != artifact.expected_byte_size {
        return Err(EmbeddingProviderError::structured(
            "embedding.artifact_size_mismatch",
            "Prepared model artifact size does not match the manifest.",
        ));
    }
    if hex_digest(&Sha256::digest(&bytes)) != artifact.expected_sha256 {
        return Err(EmbeddingProviderError::structured(
            "embedding.artifact_checksum_mismatch",
            "Prepared model artifact checksum does not match the manifest.",
        ));
    }
    Ok(bytes)
}

#[cfg(feature = "fastembed-provider")]
fn runtime_payload_error() -> EmbeddingProviderError {
    EmbeddingProviderError::structured(
        "embedding.runtime_unavailable",
        "Prepared model runtime payload is unavailable.",
    )
}

#[derive(Debug, Clone)]
pub struct PreparedModelStore {
    root: PathBuf,
}

impl PreparedModelStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn acquire(
        &self,
        options: &FastembedProviderOptions,
    ) -> Result<PreparedModelSnapshot, EmbeddingProviderError> {
        if let Some(manifest_path) = &options.manifest_path {
            let source = self.load_manifest(manifest_path)?;
            let manifest = source.manifest.clone();
            let artifacts = runtime_artifact_sources_from_prepared(&source);
            return self.materialize(options, manifest, artifacts);
        }
        if let Some(model_path) = &options.model_path {
            let snapshot = resolve_model_path_snapshot(
                model_path,
                ManualModelBehavior {
                    file: options.file.clone(),
                    pooling: options.pooling,
                    query_prefix: options.query_prefix.clone(),
                },
            )?;
            let contract = infer_legacy_runtime_contract(&snapshot)?;
            let manifest = ModelManifestV1 {
                schema_version: MODEL_MANIFEST_SCHEMA_VERSION.to_string(),
                preset_id: None,
                provider: ModelProviderKind::Fastembed,
                model_source: ModelSourceV1::Local {
                    declared_id: model_path.to_string_lossy().into_owned(),
                },
                artifacts: Vec::new(),
                tokenizer: TokenizerKind::HfTokenizerJson,
                query_prefix: Some(snapshot.query_prefix.clone()),
                document_prefix: contract.document_prefix,
                pooling: snapshot.pooling,
                normalization: contract.normalization,
                native_dimension: contract.native_dimension,
                output_dimension: contract.output_dimension,
                max_length: contract.max_length,
                quantization: required_legacy_quantization(options)?,
                context_template_version: METADATA_CONTEXT_TEMPLATE_VERSION.to_string(),
            };
            return self.materialize(options, manifest, runtime_artifact_sources(&snapshot)?);
        }
        #[cfg(feature = "fastembed-provider")]
        {
            self.acquire_hf(options)
        }
        #[cfg(not(feature = "fastembed-provider"))]
        {
            Err(EmbeddingProviderError::structured(
                "embedding.provider_unavailable",
                "This qgh binary was built without Hugging Face model acquisition support.",
            ))
        }
    }

    #[cfg(feature = "fastembed-provider")]
    pub fn acquire_tokenizer(
        &self,
        options: &FastembedProviderOptions,
    ) -> Result<FastembedTokenizer, EmbeddingProviderError> {
        let alias_inspection = match fs::symlink_metadata(self.alias_path(options)) {
            Ok(_) => Some(self.inspect_with_roles(options, Some(&TOKENIZER_ARTIFACT_ROLES))?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => {
                return Err(EmbeddingProviderError::structured(
                    "embedding.prepared_alias_invalid",
                    "Prepared model alias could not be inspected.",
                ));
            }
        };

        if let Some(manifest_path) = &options.manifest_path {
            let source_inspection =
                match fs::symlink_metadata(manifest_path) {
                    Ok(_) => Some(self.inspect_manifest_with_roles(
                        manifest_path,
                        Some(&TOKENIZER_ARTIFACT_ROLES),
                    )?),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(_) => {
                        return Err(EmbeddingProviderError::structured(
                            "embedding.prepared_manifest_invalid",
                            "Prepared model manifest could not be inspected.",
                        ));
                    }
                };
            return match (source_inspection, alias_inspection) {
                (Some(source), Some(alias)) => {
                    if source.manifest_hash != alias.manifest_hash {
                        return Err(EmbeddingProviderError::structured(
                            "embedding.prepared_alias_mismatch",
                            "Prepared model alias does not match the configured manifest.",
                        ));
                    }
                    self.verify_tokenizer(alias)
                }
                (Some(source), None) => self.verify_tokenizer(source),
                (None, Some(alias)) => self.verify_tokenizer(alias),
                (None, None) => {
                    self.inspect_manifest_with_roles(
                        manifest_path,
                        Some(&TOKENIZER_ARTIFACT_ROLES),
                    )?;
                    unreachable!("missing manifest inspection always fails")
                }
            };
        }

        if let Some(alias) = alias_inspection {
            return self.verify_tokenizer(alias);
        }
        if options.model_path.is_some() {
            return acquire_local_tokenizer(options);
        }
        self.acquire_hf_tokenizer(options)
    }

    pub fn load(
        &self,
        options: &FastembedProviderOptions,
    ) -> Result<PreparedModelSnapshot, EmbeddingProviderError> {
        let inspection = self.inspect(options)?;
        self.verify(inspection)
    }

    pub fn inspect(
        &self,
        options: &FastembedProviderOptions,
    ) -> Result<PreparedModelInspection, EmbeddingProviderError> {
        self.inspect_with_roles(options, None)
    }

    pub fn inspect_prepared_alias_contract(
        &self,
        options: &FastembedProviderOptions,
    ) -> Result<PreparedManifestInspection, EmbeddingProviderError> {
        let alias_path = self.alias_path(options);
        let alias_metadata = fs::symlink_metadata(&alias_path).map_err(|error| {
            EmbeddingProviderError::structured(
                "embedding.prepared_snapshot_missing",
                "No prepared local model snapshot is available.",
            )
            .with_details(json!({ "error": error.to_string() }))
        })?;
        if alias_metadata.file_type().is_symlink() || !alias_metadata.is_file() {
            return Err(EmbeddingProviderError::structured(
                "embedding.prepared_alias_invalid",
                "Prepared model alias must be a regular non-symlink file.",
            ));
        }
        if alias_metadata.len() > MAX_PREPARED_ALIAS_BYTES {
            return Err(EmbeddingProviderError::structured(
                "embedding.prepared_alias_invalid",
                "Prepared model alias exceeds the supported size.",
            ));
        }
        let requests_root = canonical_store_subdirectory(
            &self.root,
            "requests",
            "embedding.prepared_alias_invalid",
            "Prepared model request storage is invalid.",
        )?;
        if !fs::canonicalize(&alias_path)?.starts_with(&requests_root) {
            return Err(EmbeddingProviderError::structured(
                "embedding.prepared_alias_invalid",
                "Prepared model alias escapes the prepared model store.",
            ));
        }
        let bytes = read_bounded_file(
            &alias_path,
            &artifact_file_identity(&alias_metadata),
            MAX_PREPARED_ALIAS_BYTES,
            "embedding.prepared_alias_invalid",
            "Prepared model alias changed or exceeds the supported size.",
        )?;
        let alias: PreparedModelAliasV1 = serde_json::from_slice(&bytes).map_err(|error| {
            EmbeddingProviderError::structured(
                "embedding.prepared_alias_invalid",
                "Prepared model alias is invalid.",
            )
            .with_details(json!({ "error": error.to_string() }))
        })?;
        if alias.schema_version != PREPARED_MODEL_ALIAS_SCHEMA_VERSION
            || !is_sha256(&alias.manifest_hash)
        {
            return Err(EmbeddingProviderError::structured(
                "embedding.prepared_alias_invalid",
                "Prepared model alias has an unsupported schema or invalid hash.",
            ));
        }
        let manifest_path = self
            .root
            .join("snapshots")
            .join(&alias.manifest_hash)
            .join("manifest.json");
        let snapshots_root = canonical_store_subdirectory(
            &self.root,
            "snapshots",
            "embedding.prepared_alias_invalid",
            "Prepared model snapshot storage is invalid.",
        )?;
        let snapshot_root = manifest_path
            .parent()
            .expect("snapshot manifest has a parent");
        let snapshot_metadata = fs::symlink_metadata(snapshot_root).map_err(|_| {
            EmbeddingProviderError::structured(
                "embedding.prepared_snapshot_missing",
                "No prepared local model snapshot is available.",
            )
        })?;
        if snapshot_metadata.file_type().is_symlink() || !snapshot_metadata.is_dir() {
            return Err(EmbeddingProviderError::structured(
                "embedding.prepared_alias_invalid",
                "Prepared model alias must reference a regular snapshot directory.",
            ));
        }
        let inspection = read_manifest_contract(&manifest_path)?;
        if inspection.manifest_hash != alias.manifest_hash {
            return Err(EmbeddingProviderError::structured(
                "embedding.prepared_alias_mismatch",
                "Prepared model alias does not match the snapshot manifest.",
            ));
        }
        if !inspection.root.starts_with(&snapshots_root) {
            return Err(EmbeddingProviderError::structured(
                "embedding.prepared_alias_invalid",
                "Prepared model alias escapes the prepared model store.",
            ));
        }
        Ok(inspection)
    }

    fn inspect_with_roles(
        &self,
        options: &FastembedProviderOptions,
        selected_roles: Option<&[ArtifactRole]>,
    ) -> Result<PreparedModelInspection, EmbeddingProviderError> {
        let contract = match self.inspect_prepared_alias_contract(options) {
            Ok(contract) => contract,
            Err(error) if error.code() == "embedding.prepared_snapshot_missing" => {
                if let Some(manifest_path) = &options.manifest_path {
                    self.inspect_manifest_with_roles(manifest_path, selected_roles)?;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        self.inspect_manifest_with_roles(&contract.root.join("manifest.json"), selected_roles)
    }

    pub fn load_manifest(
        &self,
        manifest_path: &Path,
    ) -> Result<PreparedModelSnapshot, EmbeddingProviderError> {
        let inspection = self.inspect_manifest(manifest_path)?;
        self.verify(inspection)
    }

    pub fn validate_manifest_contract(
        &self,
        manifest_path: &Path,
    ) -> Result<(), EmbeddingProviderError> {
        read_manifest_contract(manifest_path).map(|_| ())
    }

    pub fn inspect_manifest_contract(
        &self,
        manifest_path: &Path,
    ) -> Result<PreparedManifestInspection, EmbeddingProviderError> {
        read_manifest_contract(manifest_path)
    }

    pub fn inspect_manifest(
        &self,
        manifest_path: &Path,
    ) -> Result<PreparedModelInspection, EmbeddingProviderError> {
        self.inspect_manifest_with_roles(manifest_path, None)
    }

    fn inspect_manifest_with_roles(
        &self,
        manifest_path: &Path,
        selected_roles: Option<&[ArtifactRole]>,
    ) -> Result<PreparedModelInspection, EmbeddingProviderError> {
        let contract = read_manifest_contract(manifest_path)?;
        let manifest = contract.manifest;
        let manifest_hash = contract.manifest_hash;
        let canonical_root = contract.root;
        validate_tokenizer_artifact_sizes(
            manifest
                .artifacts
                .iter()
                .filter(|artifact| {
                    selected_roles.is_none_or(|roles| roles.contains(&artifact.role))
                })
                .map(|artifact| (artifact.role, artifact.byte_size)),
        )?;
        let mut artifacts = Vec::with_capacity(manifest.artifacts.len());
        for artifact in &manifest.artifacts {
            if selected_roles.is_some_and(|roles| !roles.contains(&artifact.role)) {
                continue;
            }
            let relative = confined_relative_path(&artifact.relative_path)?;
            reject_symlink_components(&canonical_root, relative)?;
            let path = canonical_root.join(relative);
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                EmbeddingProviderError::structured(
                    "embedding.artifact_missing",
                    "Prepared model artifact is missing.",
                )
                .with_details(json!({
                    "path": artifact.relative_path,
                    "error": error.to_string()
                }))
            })?;
            if metadata.file_type().is_symlink() {
                return Err(EmbeddingProviderError::structured(
                    "embedding.artifact_symlink_forbidden",
                    "Prepared model artifacts must not be symbolic links.",
                ));
            }
            if !metadata.is_file() {
                return Err(EmbeddingProviderError::structured(
                    "embedding.artifact_not_regular_file",
                    "Prepared model artifacts must be regular files.",
                ));
            }
            let canonical_path = fs::canonicalize(&path)?;
            if !canonical_path.starts_with(&canonical_root) {
                return Err(EmbeddingProviderError::structured(
                    "embedding.artifact_path_escape",
                    "Prepared model artifact escapes the manifest root.",
                ));
            }
            if metadata.len() != artifact.byte_size {
                return Err(EmbeddingProviderError::structured(
                    "embedding.artifact_size_mismatch",
                    "Prepared model artifact size does not match the manifest.",
                ));
            }
            artifacts.push(PreparedArtifactInspection {
                role: artifact.role,
                relative_path: artifact.relative_path.clone(),
                canonical_path,
                expected_sha256: artifact.sha256.clone(),
                expected_byte_size: artifact.byte_size,
                identity: artifact_file_identity(&metadata),
            });
        }
        let artifact_stamp = prepared_artifact_stamp(&manifest_hash, &canonical_root, &artifacts);
        Ok(PreparedModelInspection {
            manifest,
            manifest_hash,
            root: canonical_root,
            artifacts,
            artifact_stamp,
        })
    }
}

fn read_manifest_contract(
    manifest_path: &Path,
) -> Result<PreparedManifestInspection, EmbeddingProviderError> {
    let metadata = fs::symlink_metadata(manifest_path).map_err(|error| {
        EmbeddingProviderError::structured(
            "embedding.prepared_manifest_missing",
            "Prepared model manifest is unavailable.",
        )
        .with_details(json!({ "error": error.to_string() }))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(EmbeddingProviderError::structured(
            "embedding.prepared_manifest_invalid",
            "Prepared model manifest must be a regular file.",
        ));
    }
    if metadata.len() > MAX_MODEL_MANIFEST_BYTES {
        return Err(EmbeddingProviderError::structured(
            "embedding.prepared_manifest_invalid",
            "Prepared model manifest exceeds the supported size.",
        ));
    }
    let root = manifest_path.parent().ok_or_else(|| {
        EmbeddingProviderError::structured(
            "embedding.prepared_manifest_invalid",
            "Prepared model manifest must have a parent directory.",
        )
    })?;
    let canonical_root = fs::canonicalize(root)?;
    let manifest_bytes = read_bounded_file(
        manifest_path,
        &artifact_file_identity(&metadata),
        MAX_MODEL_MANIFEST_BYTES,
        "embedding.prepared_manifest_invalid",
        "Prepared model manifest changed or exceeds the supported size.",
    )?;
    let manifest = ModelManifestV1::from_json_slice(&manifest_bytes)?;
    let manifest_hash = manifest.hash();
    for artifact in &manifest.artifacts {
        confined_relative_path(&artifact.relative_path)?;
    }
    Ok(PreparedManifestInspection {
        manifest,
        manifest_hash,
        root: canonical_root,
    })
}

fn verify_streamed_prepared_artifact(
    file: &mut File,
    artifact: &PreparedArtifactInspection,
) -> Result<ArtifactFileIdentity, EmbeddingProviderError> {
    let opened_identity = artifact_file_identity(&file.metadata()?);
    if opened_identity != artifact.identity {
        return Err(artifact_changed_error());
    }
    let (sha256, byte_size) = stream_sha256(file)?;
    let final_identity = artifact_file_identity(&file.metadata()?);
    if final_identity != opened_identity {
        return Err(artifact_changed_error());
    }
    if byte_size != artifact.expected_byte_size {
        return Err(EmbeddingProviderError::structured(
            "embedding.artifact_size_mismatch",
            "Prepared model artifact size does not match the manifest.",
        ));
    }
    if sha256 != artifact.expected_sha256 {
        return Err(EmbeddingProviderError::structured(
            "embedding.artifact_checksum_mismatch",
            "Prepared model artifact checksum does not match the manifest.",
        ));
    }
    Ok(opened_identity)
}

impl PreparedModelStore {
    pub fn verify(
        &self,
        inspection: PreparedModelInspection,
    ) -> Result<PreparedModelSnapshot, EmbeddingProviderError> {
        let mut paths = BTreeMap::<ArtifactRole, Vec<PathBuf>>::new();
        #[cfg(feature = "fastembed-provider")]
        let mut runtime_artifacts = Vec::with_capacity(inspection.artifacts.len());
        for artifact in &inspection.artifacts {
            let mut file = File::open(&artifact.canonical_path)?;
            let opened_identity = verify_streamed_prepared_artifact(&mut file, artifact)?;
            paths
                .entry(artifact.role)
                .or_default()
                .push(artifact.canonical_path.clone());
            #[cfg(feature = "fastembed-provider")]
            runtime_artifacts.push(VerifiedRuntimeArtifact {
                role: artifact.role,
                relative_path: artifact.relative_path.clone(),
                expected_sha256: artifact.expected_sha256.clone(),
                expected_byte_size: artifact.expected_byte_size,
                identity: opened_identity,
                file,
            });
        }
        Ok(PreparedModelSnapshot {
            manifest: inspection.manifest,
            root: inspection.root,
            paths,
            #[cfg(feature = "fastembed-provider")]
            runtime_state: Arc::new(Mutex::new(VerifiedRuntimeState {
                artifacts: runtime_artifacts,
                loaded_artifacts: BTreeMap::new(),
                tokenizer: None,
                consumed: false,
            })),
        })
    }

    #[cfg(feature = "fastembed-provider")]
    fn verify_tokenizer(
        &self,
        inspection: PreparedModelInspection,
    ) -> Result<FastembedTokenizer, EmbeddingProviderError> {
        validate_tokenizer_artifact_sizes(
            inspection
                .artifacts
                .iter()
                .map(|artifact| (artifact.role, artifact.expected_byte_size)),
        )?;
        let mut tokenizer_bytes = None;
        for artifact in inspection.artifacts {
            if artifact.role == ArtifactRole::Tokenizer {
                let file = File::open(&artifact.canonical_path)?;
                let opened_identity = artifact_file_identity(&file.metadata()?);
                if opened_identity != artifact.identity {
                    return Err(artifact_changed_error());
                }
                let mut runtime_artifact = VerifiedRuntimeArtifact {
                    role: artifact.role,
                    relative_path: artifact.relative_path,
                    expected_sha256: artifact.expected_sha256,
                    expected_byte_size: artifact.expected_byte_size,
                    identity: opened_identity,
                    file,
                };
                let bytes = read_verified_runtime_artifact(&mut runtime_artifact)?;
                #[cfg(test)]
                record_tokenizer_only_artifact_bytes(artifact.role, bytes.len() as u64);
                tokenizer_bytes = Some(bytes);
            } else {
                let mut file = File::open(&artifact.canonical_path)?;
                verify_streamed_prepared_artifact(&mut file, &artifact)?;
                #[cfg(test)]
                record_tokenizer_only_artifact_bytes(artifact.role, artifact.expected_byte_size);
            }
        }
        FastembedTokenizer::from_verified_bytes(tokenizer_bytes.ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.model_file_missing",
                "Prepared tokenizer snapshot is missing its tokenizer artifact.",
            )
            .with_details(json!({ "role": ArtifactRole::Tokenizer }))
        })?)
    }

    fn alias_path(&self, options: &FastembedProviderOptions) -> PathBuf {
        self.root
            .join("requests")
            .join(format!("{}.json", prepared_request_key(options)))
    }

    #[cfg(feature = "fastembed-provider")]
    fn acquisition_pin_path(&self, options: &FastembedProviderOptions) -> PathBuf {
        self.root
            .join("requests")
            .join(format!("{}.pin.json", prepared_request_key(options)))
    }

    #[cfg(feature = "fastembed-provider")]
    fn acquisition_pin_lock_path(&self, options: &FastembedProviderOptions) -> PathBuf {
        self.root
            .join("requests")
            .join(format!("{}.pin.lock", prepared_request_key(options)))
    }

    #[cfg(feature = "fastembed-provider")]
    fn ensure_requests_root(&self) -> Result<PathBuf, EmbeddingProviderError> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(self.root.join("requests"))?;
        canonical_store_subdirectory(
            &self.root,
            "requests",
            "embedding.acquisition_pin_invalid",
            "Prepared model acquisition pin storage is invalid.",
        )
    }

    #[cfg(feature = "fastembed-provider")]
    fn acquire_pin_mutation_lock(
        &self,
        options: &FastembedProviderOptions,
    ) -> Result<AcquisitionPinMutationLock, EmbeddingProviderError> {
        self.ensure_requests_root()?;
        let path = self.acquisition_pin_lock_path(options);
        for _ in 0..10_000 {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(AcquisitionPinMutationLock { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::yield_now();
                }
                Err(_) => {
                    return Err(EmbeddingProviderError::structured(
                        "embedding.acquisition_pin_lock_failed",
                        "Prepared model acquisition pin lock could not be created.",
                    ));
                }
            }
        }
        Err(EmbeddingProviderError::structured(
            "embedding.acquisition_pin_busy",
            "Prepared model acquisition pin is busy or requires stale-lock cleanup.",
        ))
    }

    #[cfg(feature = "fastembed-provider")]
    fn read_acquisition_pin_record_at(
        &self,
        pin_path: &Path,
    ) -> Result<Option<PreparedModelAcquisitionPinV1>, EmbeddingProviderError> {
        let metadata = match fs::symlink_metadata(pin_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(EmbeddingProviderError::structured(
                    "embedding.acquisition_pin_invalid",
                    "Prepared model acquisition pin could not be inspected.",
                ));
            }
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_PREPARED_ALIAS_BYTES
        {
            return Err(EmbeddingProviderError::structured(
                "embedding.acquisition_pin_invalid",
                "Prepared model acquisition pin must be a bounded regular non-symlink file.",
            ));
        }
        let requests_root = self.ensure_requests_root()?;
        if !fs::canonicalize(pin_path)?.starts_with(&requests_root) {
            return Err(EmbeddingProviderError::structured(
                "embedding.acquisition_pin_invalid",
                "Prepared model acquisition pin escapes its local store.",
            ));
        }
        let bytes = read_bounded_file(
            pin_path,
            &artifact_file_identity(&metadata),
            MAX_PREPARED_ALIAS_BYTES,
            "embedding.acquisition_pin_invalid",
            "Prepared model acquisition pin changed or exceeds the supported size.",
        )?;
        let pin: PreparedModelAcquisitionPinV1 = serde_json::from_slice(&bytes).map_err(|_| {
            EmbeddingProviderError::structured(
                "embedding.acquisition_pin_invalid",
                "Prepared model acquisition pin is invalid.",
            )
        })?;
        if pin.schema_version != PREPARED_MODEL_ACQUISITION_PIN_SCHEMA_VERSION
            || !is_commit_sha(&pin.resolved_revision)
        {
            return Err(EmbeddingProviderError::structured(
                "embedding.acquisition_pin_invalid",
                "Prepared model acquisition pin has an invalid contract.",
            ));
        }
        Ok(Some(pin))
    }

    #[cfg(feature = "fastembed-provider")]
    fn read_acquisition_pin(
        &self,
        options: &FastembedProviderOptions,
        reference: &HfModelReference,
    ) -> Result<Option<HfModelReference>, EmbeddingProviderError> {
        let pin = self.read_acquisition_pin_record_at(&self.acquisition_pin_path(options))?;
        let Some(pin) = pin else {
            return Ok(None);
        };
        if pin.model_id != reference.model_id || pin.requested_revision != reference.revision {
            return Err(EmbeddingProviderError::structured(
                "embedding.acquisition_pin_invalid",
                "Prepared model acquisition pin does not match the configured model reference.",
            ));
        }
        Ok(Some(HfModelReference {
            model_id: pin.model_id,
            revision: pin.resolved_revision,
        }))
    }

    #[cfg(feature = "fastembed-provider")]
    fn acquisition_pin_for_manifest(
        &self,
        options: &FastembedProviderOptions,
        manifest: &ModelManifestV1,
    ) -> Result<Option<PreparedModelAcquisitionPinV1>, EmbeddingProviderError> {
        let ModelSourceV1::Hf {
            model_id,
            resolved_revision,
        } = &manifest.model_source
        else {
            return Ok(None);
        };
        Ok(self
            .read_acquisition_pin_record_at(&self.acquisition_pin_path(options))?
            .filter(|pin| pin.model_id == *model_id && pin.resolved_revision == *resolved_revision))
    }

    #[cfg(feature = "fastembed-provider")]
    fn retire_acquisition_pin(
        &self,
        options: &FastembedProviderOptions,
        expected: &PreparedModelAcquisitionPinV1,
    ) -> Result<(), EmbeddingProviderError> {
        let _lock = self.acquire_pin_mutation_lock(options)?;
        let pin_path = self.acquisition_pin_path(options);
        let current = self.read_acquisition_pin_record_at(&pin_path)?;
        if current.as_ref() == Some(expected) {
            fs::remove_file(pin_path).map_err(|_| {
                EmbeddingProviderError::structured(
                    "embedding.acquisition_pin_retire_failed",
                    "Prepared model acquisition pin could not be retired.",
                )
            })?;
        }
        Ok(())
    }

    #[cfg(feature = "fastembed-provider")]
    fn publish_acquisition_pin(
        &self,
        options: &FastembedProviderOptions,
        reference: &HfModelReference,
        resolved_revision: String,
    ) -> Result<HfModelReference, EmbeddingProviderError> {
        if !is_commit_sha(&resolved_revision) {
            return Err(EmbeddingProviderError::structured(
                "embedding.manifest_revision_invalid",
                "Model acquisition did not resolve an immutable revision.",
            ));
        }
        self.ensure_requests_root()?;
        let pin_path = self.acquisition_pin_path(options);
        let pin = PreparedModelAcquisitionPinV1 {
            schema_version: PREPARED_MODEL_ACQUISITION_PIN_SCHEMA_VERSION.to_string(),
            model_id: reference.model_id.clone(),
            requested_revision: reference.revision.clone(),
            resolved_revision: resolved_revision.clone(),
        };
        let _lock = self.acquire_pin_mutation_lock(options)?;
        if let Some(existing) = self.read_acquisition_pin(options, reference)? {
            return Ok(existing);
        }
        let staging = pin_path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        write_new_bytes(
            &staging,
            &serde_json::to_vec_pretty(&pin).expect("acquisition pin serializes"),
        )?;
        let published = fs::hard_link(&staging, &pin_path);
        let _ = fs::remove_file(&staging);
        match published {
            Ok(()) => Ok(HfModelReference {
                model_id: reference.model_id.clone(),
                revision: resolved_revision,
            }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => self
                .read_acquisition_pin(options, reference)?
                .ok_or_else(|| {
                    EmbeddingProviderError::structured(
                        "embedding.acquisition_pin_invalid",
                        "Prepared model acquisition pin disappeared during publication.",
                    )
                }),
            Err(_) => Err(EmbeddingProviderError::structured(
                "embedding.acquisition_pin_invalid",
                "Prepared model acquisition pin could not be published.",
            )),
        }
    }

    #[cfg(feature = "fastembed-provider")]
    fn resolve_or_pin_hf_reference_with(
        &self,
        options: &FastembedProviderOptions,
        reference: &HfModelReference,
        resolve: impl FnOnce() -> Result<String, EmbeddingProviderError>,
    ) -> Result<HfModelReference, EmbeddingProviderError> {
        if let Some(pinned) = self.read_acquisition_pin(options, reference)? {
            return Ok(pinned);
        }
        self.publish_acquisition_pin(options, reference, resolve()?)
    }

    #[cfg(feature = "fastembed-provider")]
    fn resolve_or_pin_hf_reference(
        &self,
        options: &FastembedProviderOptions,
        reference: &HfModelReference,
        cache_dir: &Path,
    ) -> Result<HfModelReference, EmbeddingProviderError> {
        self.resolve_or_pin_hf_reference_with(options, reference, || {
            let mut repo = HfHubModelRepository::new(
                &reference.model_id,
                &reference.revision,
                cache_dir.to_path_buf(),
                options.token_source_env.clone(),
            )?;
            repo.revision()?.ok_or_else(|| {
                EmbeddingProviderError::structured(
                    "embedding.manifest_revision_invalid",
                    "Model acquisition did not resolve an immutable revision.",
                )
            })
        })
    }

    fn materialize(
        &self,
        options: &FastembedProviderOptions,
        mut manifest: ModelManifestV1,
        sources: Vec<RuntimeArtifactSource>,
    ) -> Result<PreparedModelSnapshot, EmbeddingProviderError> {
        #[cfg(feature = "fastembed-provider")]
        let acquisition_pin = self.acquisition_pin_for_manifest(options, &manifest)?;
        fs::create_dir_all(self.root.join("snapshots"))?;
        fs::create_dir_all(self.root.join("requests"))?;
        canonical_store_subdirectory(
            &self.root,
            "snapshots",
            "embedding.acquisition_artifact_invalid",
            "Prepared model snapshot storage is invalid.",
        )?;
        canonical_store_subdirectory(
            &self.root,
            "requests",
            "embedding.acquisition_artifact_invalid",
            "Prepared model request storage is invalid.",
        )?;
        let staging = self.root.join(format!(
            ".staging-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir(&staging)?;
        let result = (|| {
            let mut artifacts = Vec::with_capacity(sources.len());
            for source in sources {
                let relative_path = confined_relative_path(&source.relative_path)?;
                let metadata = fs::symlink_metadata(&source.source_path)?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(EmbeddingProviderError::structured(
                        "embedding.acquisition_artifact_invalid",
                        "Model acquisition source must be a regular non-symlink file.",
                    ));
                }
                let destination = staging.join(relative_path);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                let (sha256, byte_size) = stream_copy_and_hash(&source.source_path, &destination)?;
                artifacts.push(ModelArtifactV1 {
                    role: source.role,
                    relative_path: source.relative_path,
                    sha256,
                    byte_size,
                    external_initializer_name: source.external_initializer_name,
                });
            }
            if manifest.artifacts.is_empty() {
                manifest.artifacts = artifacts;
            } else if manifest.artifacts != artifacts {
                return Err(EmbeddingProviderError::structured(
                    "embedding.acquisition_artifact_mismatch",
                    "Materialized model artifacts do not match the declared manifest.",
                ));
            }
            manifest.validate_contract()?;
            let manifest_hash = manifest.hash();
            fs::write(
                staging.join("manifest.json"),
                serde_json::to_vec_pretty(&manifest).expect("model manifest serializes"),
            )?;
            let snapshot_root = self.root.join("snapshots").join(&manifest_hash);
            if snapshot_root.exists() {
                fs::remove_dir_all(&staging)?;
            } else {
                fs::rename(&staging, &snapshot_root)?;
            }
            let alias = PreparedModelAliasV1 {
                schema_version: PREPARED_MODEL_ALIAS_SCHEMA_VERSION.to_string(),
                manifest_hash,
            };
            let alias_path = self.alias_path(options);
            let alias_staging = alias_path.with_extension(format!(
                "tmp-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            write_new_bytes(
                &alias_staging,
                &serde_json::to_vec_pretty(&alias).expect("prepared alias serializes"),
            )?;
            fs::rename(alias_staging, alias_path)?;
            self.load(options)
        })();
        if staging.exists() {
            let _ = fs::remove_dir_all(staging);
        }
        #[cfg(feature = "fastembed-provider")]
        if result.is_ok() {
            if let Some(expected) = acquisition_pin.as_ref() {
                let _ = self.retire_acquisition_pin(options, expected);
            }
        }
        result
    }

    #[cfg(feature = "fastembed-provider")]
    fn acquire_hf_tokenizer(
        &self,
        options: &FastembedProviderOptions,
    ) -> Result<FastembedTokenizer, EmbeddingProviderError> {
        let configured_model = options.model.as_deref();
        let explicit_preset = configured_model.and_then(builtin_preset);
        if explicit_preset.is_some()
            && (options.file.is_some()
                || options.pooling.is_some()
                || options.query_prefix.is_some()
                || options.quantization.is_some())
        {
            return Err(EmbeddingProviderError::structured(
                "embedding.preset_override_forbidden",
                "Built-in preset runtime behavior cannot be overridden.",
            ));
        }
        let default_preset = (configured_model.is_none()
            && options.file.is_none()
            && options.pooling.is_none()
            && options.query_prefix.is_none()
            && options.quantization.is_none())
        .then(|| builtin_preset("arctic-l-v2-fp32").expect("default preset is registered"));

        let reference = match explicit_preset.or(default_preset) {
            Some(preset) => HfModelReference {
                model_id: preset.model_id.to_string(),
                revision: preset.revision.to_string(),
            },
            None => hf_model_reference(configured_model)?,
        };
        let preset = explicit_preset
            .or(default_preset)
            .or_else(|| preset_for_compatible_legacy_hf(&reference, options));
        let cache_dir = options
            .cache_dir
            .clone()
            .map(Ok)
            .unwrap_or_else(default_hf_cache_dir)?;
        let pinned = self.resolve_or_pin_hf_reference(options, &reference, &cache_dir)?;
        if let Some(preset) = preset {
            if pinned.revision != preset.revision {
                return Err(EmbeddingProviderError::structured(
                    "embedding.preset_revision_mismatch",
                    "Preset repository did not resolve to its pinned commit.",
                ));
            }
        }
        let mut repo = HfHubModelRepository::new(
            &pinned.model_id,
            &pinned.revision,
            cache_dir.clone(),
            options.token_source_env.clone(),
        )?;
        let artifacts = tokenizer_artifact_paths(|file| repo.get(file))?;
        load_unmanifested_tokenizer(&cache_dir, artifacts)
    }

    #[cfg(feature = "fastembed-provider")]
    fn acquire_hf(
        &self,
        options: &FastembedProviderOptions,
    ) -> Result<PreparedModelSnapshot, EmbeddingProviderError> {
        let configured_model = options.model.as_deref();
        let explicit_preset = configured_model.and_then(builtin_preset);
        if explicit_preset.is_some()
            && (options.file.is_some()
                || options.pooling.is_some()
                || options.query_prefix.is_some()
                || options.quantization.is_some())
        {
            return Err(EmbeddingProviderError::structured(
                "embedding.preset_override_forbidden",
                "Built-in preset runtime behavior cannot be overridden.",
            ));
        }
        let default_preset = (configured_model.is_none()
            && options.file.is_none()
            && options.pooling.is_none()
            && options.query_prefix.is_none()
            && options.quantization.is_none())
        .then(|| builtin_preset("arctic-l-v2-fp32").expect("default preset is registered"));
        if let Some(preset) = explicit_preset.or(default_preset) {
            return self.acquire_builtin_preset(options, preset);
        }

        let reference = hf_model_reference(configured_model)?;
        if let Some(preset) = preset_for_compatible_legacy_hf(&reference, options) {
            return self.acquire_builtin_preset(options, preset);
        }
        let cache_dir = options
            .cache_dir
            .clone()
            .map(Ok)
            .unwrap_or_else(default_hf_cache_dir)?;
        let pinned = self.resolve_or_pin_hf_reference(options, &reference, &cache_dir)?;
        let mut repo = HfHubModelRepository::new(
            &pinned.model_id,
            &pinned.revision,
            cache_dir,
            options.token_source_env.clone(),
        )?;
        let mut snapshot = resolve_hf_model_snapshot(
            &pinned.model_id,
            ManualModelBehavior {
                file: options.file.clone(),
                pooling: options.pooling,
                query_prefix: options.query_prefix.clone(),
            },
            &mut repo,
        )?;
        if snapshot.model_revision != pinned.revision {
            return Err(EmbeddingProviderError::structured(
                "embedding.hf_revision_mismatch",
                "Resolved model artifacts do not match the pinned revision.",
            ));
        }
        if snapshot.path_for(MODULES_FILE).is_none() {
            snapshot
                .paths
                .insert(MODULES_FILE.to_string(), repo.get(MODULES_FILE)?);
        }
        let contract = infer_legacy_runtime_contract(&snapshot)?;
        let manifest = ModelManifestV1 {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION.to_string(),
            preset_id: None,
            provider: ModelProviderKind::Fastembed,
            model_source: ModelSourceV1::Hf {
                model_id: pinned.model_id,
                resolved_revision: snapshot.model_revision.clone(),
            },
            artifacts: Vec::new(),
            tokenizer: TokenizerKind::HfTokenizerJson,
            query_prefix: Some(snapshot.query_prefix.clone()),
            document_prefix: contract.document_prefix,
            pooling: snapshot.pooling,
            normalization: contract.normalization,
            native_dimension: contract.native_dimension,
            output_dimension: contract.output_dimension,
            max_length: contract.max_length,
            quantization: required_legacy_quantization(options)?,
            context_template_version: METADATA_CONTEXT_TEMPLATE_VERSION.to_string(),
        };
        self.materialize(options, manifest, runtime_artifact_sources(&snapshot)?)
    }

    #[cfg(feature = "fastembed-provider")]
    fn acquire_builtin_preset(
        &self,
        options: &FastembedProviderOptions,
        preset: BuiltinPresetSpec,
    ) -> Result<PreparedModelSnapshot, EmbeddingProviderError> {
        let cache_dir = options
            .cache_dir
            .clone()
            .map(Ok)
            .unwrap_or_else(default_hf_cache_dir)?;
        let reference = HfModelReference {
            model_id: preset.model_id.to_string(),
            revision: preset.revision.to_string(),
        };
        let pinned = self.resolve_or_pin_hf_reference(options, &reference, &cache_dir)?;
        if pinned.revision != preset.revision {
            return Err(EmbeddingProviderError::structured(
                "embedding.preset_revision_mismatch",
                "Preset repository did not resolve to its pinned commit.",
            ));
        }
        let mut repo = HfHubModelRepository::new(
            &pinned.model_id,
            &pinned.revision,
            cache_dir,
            options.token_source_env.clone(),
        )?;
        let resolved_revision = pinned.revision;
        let mut paths = BTreeMap::new();
        for file in required_runtime_files(preset.model_file) {
            paths.insert(file.to_string(), repo.get(file)?);
        }
        if let Some((relative_path, _)) = preset.external_initializer {
            paths.insert(relative_path.to_string(), repo.get(relative_path)?);
        }
        let snapshot = ResolvedModelSnapshot {
            model_id: Some(preset.model_id.to_string()),
            model_revision: resolved_revision.clone(),
            model_file: preset.model_file.to_string(),
            query_prefix: preset.query_prefix.unwrap_or_default().to_string(),
            pooling: preset.pooling,
            paths,
        };
        let manifest = ModelManifestV1 {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION.to_string(),
            preset_id: Some(preset.id.to_string()),
            provider: ModelProviderKind::Fastembed,
            model_source: ModelSourceV1::Hf {
                model_id: preset.model_id.to_string(),
                resolved_revision,
            },
            artifacts: Vec::new(),
            tokenizer: TokenizerKind::HfTokenizerJson,
            query_prefix: preset.query_prefix.map(ToString::to_string),
            document_prefix: preset.document_prefix.map(ToString::to_string),
            pooling: preset.pooling,
            normalization: preset.normalization,
            native_dimension: preset.native_dimension,
            output_dimension: preset.output_dimension,
            max_length: preset.max_length,
            quantization: preset.quantization,
            context_template_version: METADATA_CONTEXT_TEMPLATE_VERSION.to_string(),
        };
        let mut sources = runtime_artifact_sources(&snapshot)?;
        if let Some((relative_path, initializer_name)) = preset.external_initializer {
            if let Some(source) = sources
                .iter_mut()
                .find(|source| source.relative_path == relative_path)
            {
                source.external_initializer_name = Some(initializer_name.to_string());
            }
        }
        self.materialize(options, manifest, sources)
    }
}

#[cfg(feature = "fastembed-provider")]
fn acquire_local_tokenizer(
    options: &FastembedProviderOptions,
) -> Result<FastembedTokenizer, EmbeddingProviderError> {
    let model_path = options
        .model_path
        .as_deref()
        .expect("local tokenizer acquisition requires model_path");
    let root = if options.file.is_none()
        && model_path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("onnx"))
    {
        model_root_for_file(model_path)?
    } else {
        model_path.to_path_buf()
    };
    let artifacts = tokenizer_artifact_paths(|file| Ok(root.join(file)))?;
    load_unmanifested_tokenizer(&root, artifacts)
}

#[cfg(feature = "fastembed-provider")]
fn tokenizer_artifact_paths(
    mut resolve: impl FnMut(&str) -> Result<PathBuf, EmbeddingProviderError>,
) -> Result<Vec<(ArtifactRole, PathBuf)>, EmbeddingProviderError> {
    TOKENIZER_ARTIFACT_ROLES
        .into_iter()
        .zip(TOKENIZER_FILES)
        .map(|(role, file)| resolve(file).map(|path| (role, path)))
        .collect()
}

#[cfg(feature = "fastembed-provider")]
fn load_unmanifested_tokenizer(
    root: &Path,
    artifacts: Vec<(ArtifactRole, PathBuf)>,
) -> Result<FastembedTokenizer, EmbeddingProviderError> {
    let canonical_root = fs::canonicalize(root)?;
    let mut inspected = Vec::with_capacity(artifacts.len());
    for (role, path) in artifacts {
        let metadata = fs::symlink_metadata(&path).map_err(|_| {
            EmbeddingProviderError::structured(
                "embedding.artifact_missing",
                "Required tokenizer artifact is missing.",
            )
            .with_details(json!({ "role": role }))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(EmbeddingProviderError::structured(
                "embedding.acquisition_artifact_invalid",
                "Tokenizer acquisition source must be a regular non-symlink file.",
            )
            .with_details(json!({ "role": role })));
        }
        let canonical_path = fs::canonicalize(&path)?;
        if !canonical_path.starts_with(&canonical_root) {
            return Err(EmbeddingProviderError::structured(
                "embedding.artifact_path_escape",
                "Tokenizer artifact escapes its acquisition root.",
            )
            .with_details(json!({ "role": role })));
        }
        inspected.push((
            role,
            canonical_path,
            artifact_file_identity(&metadata),
            metadata.len(),
        ));
    }
    validate_tokenizer_artifact_sizes(
        inspected
            .iter()
            .map(|(role, _, _, byte_size)| (*role, *byte_size)),
    )?;

    let mut tokenizer_bytes = None;
    for (role, canonical_path, identity, byte_size) in inspected {
        if role == ArtifactRole::Tokenizer {
            let bytes = read_bounded_file(
                &canonical_path,
                &identity,
                MAX_TOKENIZER_ARTIFACT_BYTES,
                "embedding.artifact_changed",
                "Tokenizer artifact changed while it was being verified.",
            )?;
            let _sha256 = hex_digest(&Sha256::digest(&bytes));
            #[cfg(test)]
            record_tokenizer_only_artifact_bytes(role, bytes.len() as u64);
            tokenizer_bytes = Some(bytes);
        } else {
            let mut file = File::open(&canonical_path)?;
            let opened_identity = artifact_file_identity(&file.metadata()?);
            if opened_identity != identity {
                return Err(artifact_changed_error());
            }
            let (_sha256, actual_size) = stream_sha256(&mut file)?;
            if actual_size != byte_size
                || artifact_file_identity(&file.metadata()?) != opened_identity
            {
                return Err(artifact_changed_error());
            }
            #[cfg(test)]
            record_tokenizer_only_artifact_bytes(role, actual_size);
        }
    }
    FastembedTokenizer::from_verified_bytes(tokenizer_bytes.ok_or_else(|| {
        EmbeddingProviderError::structured(
            "embedding.model_file_missing",
            "Tokenizer acquisition is missing its tokenizer artifact.",
        )
        .with_details(json!({ "role": ArtifactRole::Tokenizer }))
    })?)
}

const PREPARED_MODEL_ALIAS_SCHEMA_VERSION: &str = "qgh.prepared_model_alias.v1";
#[cfg(feature = "fastembed-provider")]
const PREPARED_MODEL_ACQUISITION_PIN_SCHEMA_VERSION: &str = "qgh.prepared_model_acquisition_pin.v1";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PreparedModelAliasV1 {
    schema_version: String,
    manifest_hash: String,
}

#[cfg(feature = "fastembed-provider")]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PreparedModelAcquisitionPinV1 {
    schema_version: String,
    model_id: String,
    requested_revision: String,
    resolved_revision: String,
}

#[cfg(feature = "fastembed-provider")]
struct AcquisitionPinMutationLock {
    path: PathBuf,
}

#[cfg(feature = "fastembed-provider")]
impl Drop for AcquisitionPinMutationLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

struct RuntimeArtifactSource {
    role: ArtifactRole,
    relative_path: String,
    source_path: PathBuf,
    external_initializer_name: Option<String>,
}

struct LegacyRuntimeContract {
    document_prefix: Option<String>,
    normalization: NormalizationKind,
    native_dimension: usize,
    output_dimension: usize,
    max_length: usize,
}

fn infer_legacy_runtime_contract(
    snapshot: &ResolvedModelSnapshot,
) -> Result<LegacyRuntimeContract, EmbeddingProviderError> {
    let config = read_json_file(
        required_path_from_resolved(snapshot, "config.json")?,
        "config.json",
    )?;
    let tokenizer_config = read_json_file(
        required_path_from_resolved(snapshot, "tokenizer_config.json")?,
        "tokenizer_config.json",
    )?;
    let modules_path = snapshot.path_for(MODULES_FILE).ok_or_else(|| {
        legacy_manifest_ambiguous(
            "Legacy model configuration cannot express document prefix and normalization.",
        )
    })?;
    let modules = read_json_file(modules_path, MODULES_FILE)?;
    let native_dimension = config
        .get("hidden_size")
        .or_else(|| config.get("dim"))
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| legacy_manifest_ambiguous("Legacy model native dimension is ambiguous."))?;
    let max_length = tokenizer_config
        .get("model_max_length")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0 && *value < 1_000_000)
        .or_else(|| {
            config
                .get("max_position_embeddings")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .filter(|value| *value > 0)
        })
        .ok_or_else(|| legacy_manifest_ambiguous("Legacy model max length is ambiguous."))?;
    if !contains_module_type(&modules, "Normalize") {
        return Err(legacy_manifest_ambiguous(
            "Legacy model normalization is ambiguous.",
        ));
    }
    let document_prefix = find_prompt(&modules, "document")
        .map(Some)
        .ok_or_else(|| legacy_manifest_ambiguous("Legacy document prefix is ambiguous."))?;
    Ok(LegacyRuntimeContract {
        document_prefix,
        normalization: NormalizationKind::L2,
        native_dimension,
        output_dimension: native_dimension,
        max_length,
    })
}

fn legacy_manifest_ambiguous(message: &str) -> EmbeddingProviderError {
    EmbeddingProviderError::structured("embedding.legacy_manifest_ambiguous", message)
        .with_hint("Use embedding.manifest_path with an explicit ModelManifestV1.")
}

fn contains_module_type(value: &Value, needle: &str) -> bool {
    match value {
        Value::Object(object) => {
            object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|module_type| module_type.contains(needle))
                || object
                    .values()
                    .any(|value| contains_module_type(value, needle))
        }
        Value::Array(values) => values
            .iter()
            .any(|value| contains_module_type(value, needle)),
        _ => false,
    }
}

fn find_prompt(value: &Value, name: &str) -> Option<String> {
    match value {
        Value::Object(object) => {
            if let Some(prompt) = object
                .get("prompts")
                .and_then(|prompts| prompts.get(name))
                .and_then(Value::as_str)
            {
                return Some(prompt.to_string());
            }
            object.values().find_map(|value| find_prompt(value, name))
        }
        Value::Array(values) => values.iter().find_map(|value| find_prompt(value, name)),
        _ => None,
    }
}

fn required_legacy_quantization(
    options: &FastembedProviderOptions,
) -> Result<QuantizationKind, EmbeddingProviderError> {
    options.quantization.ok_or_else(|| {
        EmbeddingProviderError::structured(
            "embedding.legacy_quantization_required",
            "Legacy model configuration must explicitly declare quantization.",
        )
        .with_hint("Set quantization to `none` or `static`, or use embedding.manifest_path.")
    })
}

fn runtime_artifact_sources(
    snapshot: &ResolvedModelSnapshot,
) -> Result<Vec<RuntimeArtifactSource>, EmbeddingProviderError> {
    let mut sources = Vec::new();
    for (role, relative_path) in [
        (ArtifactRole::OnnxModel, snapshot.model_file.as_str()),
        (ArtifactRole::Tokenizer, "tokenizer.json"),
        (ArtifactRole::Config, "config.json"),
        (ArtifactRole::SpecialTokensMap, "special_tokens_map.json"),
        (ArtifactRole::TokenizerConfig, "tokenizer_config.json"),
    ] {
        sources.push(RuntimeArtifactSource {
            role,
            relative_path: relative_path.to_string(),
            source_path: required_path_from_resolved(snapshot, relative_path)?.to_path_buf(),
            external_initializer_name: None,
        });
    }
    for (relative_path, source_path) in &snapshot.paths {
        if relative_path.ends_with("_data") {
            let initializer_name = Path::new(relative_path)
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    EmbeddingProviderError::structured(
                        "embedding.external_initializer_invalid",
                        "External initializer path is invalid.",
                    )
                })?;
            sources.push(RuntimeArtifactSource {
                role: ArtifactRole::OnnxExternalData,
                relative_path: relative_path.clone(),
                source_path: source_path.clone(),
                external_initializer_name: Some(initializer_name.to_string()),
            });
        }
    }
    Ok(sources)
}

fn runtime_artifact_sources_from_prepared(
    snapshot: &PreparedModelSnapshot,
) -> Vec<RuntimeArtifactSource> {
    snapshot
        .manifest
        .artifacts
        .iter()
        .map(|artifact| RuntimeArtifactSource {
            role: artifact.role,
            relative_path: artifact.relative_path.clone(),
            source_path: snapshot.root.join(&artifact.relative_path),
            external_initializer_name: artifact.external_initializer_name.clone(),
        })
        .collect()
}

fn required_path_from_resolved<'a>(
    snapshot: &'a ResolvedModelSnapshot,
    file: &str,
) -> Result<&'a Path, EmbeddingProviderError> {
    snapshot.path_for(file).ok_or_else(|| {
        EmbeddingProviderError::structured(
            "embedding.model_file_missing",
            "Resolved model snapshot is missing a required runtime file.",
        )
        .with_details(json!({ "file": file }))
    })
}

fn prepared_request_key(options: &FastembedProviderOptions) -> String {
    let identity = format!(
        "manifest={:?}\nmodel={:?}\nmodel_path={:?}\nfile={:?}\npooling={:?}\nquery_prefix={:?}\nquantization={:?}",
        options.manifest_path,
        options.model,
        options.model_path,
        options.file,
        options.pooling,
        options.query_prefix,
        options.quantization
    );
    hex_digest(&Sha256::digest(identity.as_bytes()))
}

fn confined_relative_path(value: &str) -> Result<&Path, EmbeddingProviderError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(EmbeddingProviderError::structured(
            "embedding.artifact_path_invalid",
            "Prepared model artifact path must stay below the manifest root.",
        ));
    }
    Ok(path)
}

fn canonical_store_subdirectory(
    store_root: &Path,
    name: &str,
    code: &'static str,
    message: &'static str,
) -> Result<PathBuf, EmbeddingProviderError> {
    let canonical_store_root = fs::canonicalize(store_root)?;
    let subdirectory = store_root.join(name);
    let metadata = fs::symlink_metadata(&subdirectory)
        .map_err(|_| EmbeddingProviderError::structured(code, message))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(EmbeddingProviderError::structured(code, message));
    }
    let canonical_subdirectory = fs::canonicalize(&subdirectory)?;
    if !canonical_subdirectory.starts_with(&canonical_store_root) {
        return Err(EmbeddingProviderError::structured(code, message));
    }
    Ok(canonical_subdirectory)
}

fn reject_symlink_components(root: &Path, relative: &Path) -> Result<(), EmbeddingProviderError> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(component) => current.push(component),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(EmbeddingProviderError::structured(
                    "embedding.artifact_path_invalid",
                    "Prepared model artifact path must stay below the manifest root.",
                ));
            }
        }
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            EmbeddingProviderError::structured(
                "embedding.artifact_missing",
                "Prepared model artifact is missing.",
            )
            .with_details(json!({ "error": error.to_string() }))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(EmbeddingProviderError::structured(
                "embedding.artifact_symlink_forbidden",
                "Prepared model artifacts must not contain symbolic links.",
            ));
        }
    }
    Ok(())
}

fn artifact_file_identity(metadata: &fs::Metadata) -> ArtifactFileIdentity {
    let modified_nanos = metadata.modified().ok().and_then(|modified| {
        modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_nanos())
    });
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ArtifactFileIdentity {
            byte_size: metadata.len(),
            modified_nanos,
            device: metadata.dev(),
            inode: metadata.ino(),
            ctime_seconds: metadata.ctime(),
            ctime_nanos: metadata.ctime_nsec(),
        }
    }
    #[cfg(not(unix))]
    {
        ArtifactFileIdentity {
            byte_size: metadata.len(),
            modified_nanos,
        }
    }
}

fn prepared_artifact_stamp(
    manifest_hash: &str,
    root: &Path,
    artifacts: &[PreparedArtifactInspection],
) -> String {
    let mut hasher = Sha256::new();
    update_path_digest(&mut hasher, root);
    hasher.update(manifest_hash.as_bytes());
    for artifact in artifacts {
        hasher.update(artifact.relative_path.as_bytes());
        update_path_digest(&mut hasher, &artifact.canonical_path);
        hasher.update(artifact.expected_byte_size.to_le_bytes());
        hasher.update(artifact.identity.byte_size.to_le_bytes());
        hasher.update(
            artifact
                .identity
                .modified_nanos
                .unwrap_or_default()
                .to_le_bytes(),
        );
        #[cfg(unix)]
        {
            hasher.update(artifact.identity.device.to_le_bytes());
            hasher.update(artifact.identity.inode.to_le_bytes());
            hasher.update(artifact.identity.ctime_seconds.to_le_bytes());
            hasher.update(artifact.identity.ctime_nanos.to_le_bytes());
        }
    }
    hex_digest(&hasher.finalize())
}

fn update_path_digest(hasher: &mut Sha256, path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        hasher.update(bytes.len().to_le_bytes());
        hasher.update(bytes);
    }
    #[cfg(not(unix))]
    {
        let path = path.to_string_lossy();
        hasher.update(path.len().to_le_bytes());
        hasher.update(path.as_bytes());
    }
}

fn read_bounded_file(
    path: &Path,
    expected_identity: &ArtifactFileIdentity,
    max_bytes: u64,
    code: &'static str,
    message: &'static str,
) -> Result<Vec<u8>, EmbeddingProviderError> {
    let mut file = File::open(path)?;
    if &artifact_file_identity(&file.metadata()?) != expected_identity {
        return Err(EmbeddingProviderError::structured(code, message));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes
        || &artifact_file_identity(&file.metadata()?) != expected_identity
    {
        return Err(EmbeddingProviderError::structured(code, message));
    }
    Ok(bytes)
}

fn stream_sha256(reader: &mut impl Read) -> Result<(String, u64), EmbeddingProviderError> {
    const BUFFER_BYTES: usize = 1024 * 1024;
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let mut hasher = Sha256::new();
    let mut byte_size = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        byte_size = byte_size.checked_add(read as u64).ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.artifact_size_mismatch",
                "Prepared model artifact size exceeds the supported range.",
            )
        })?;
    }
    Ok((hex_digest(&hasher.finalize()), byte_size))
}

fn stream_copy_and_hash(
    source: &Path,
    destination: &Path,
) -> Result<(String, u64), EmbeddingProviderError> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(EmbeddingProviderError::structured(
            "embedding.acquisition_artifact_invalid",
            "Model acquisition source must be a regular non-symlink file.",
        ));
    }
    let expected_identity = artifact_file_identity(&metadata);
    let mut source_file = File::open(source)?;
    if artifact_file_identity(&source_file.metadata()?) != expected_identity {
        return Err(artifact_changed_error());
    }
    let result = stream_reader_to_new_file(&mut source_file, destination);
    if artifact_file_identity(&source_file.metadata()?) != expected_identity {
        let _ = fs::remove_file(destination);
        return Err(artifact_changed_error());
    }
    result
}

fn stream_reader_to_new_file(
    reader: &mut impl Read,
    destination: &Path,
) -> Result<(String, u64), EmbeddingProviderError> {
    const BUFFER_BYTES: usize = 1024 * 1024;
    let mut destination_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let result = (|| {
        let mut buffer = vec![0_u8; BUFFER_BYTES];
        let mut hasher = Sha256::new();
        let mut byte_size = 0_u64;
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            destination_file.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
            byte_size = byte_size.checked_add(read as u64).ok_or_else(|| {
                EmbeddingProviderError::structured(
                    "embedding.artifact_size_mismatch",
                    "Prepared model artifact size exceeds the supported range.",
                )
            })?;
        }
        destination_file.sync_all()?;
        Ok((hex_digest(&hasher.finalize()), byte_size))
    })();
    if result.is_err() {
        drop(destination_file);
        let _ = fs::remove_file(destination);
    }
    result
}

fn write_new_bytes(path: &Path, bytes: &[u8]) -> Result<(), EmbeddingProviderError> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
    }
    result
}

fn artifact_changed_error() -> EmbeddingProviderError {
    EmbeddingProviderError::structured(
        "embedding.artifact_changed",
        "Prepared model artifact changed during verification.",
    )
}

impl PoolingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PoolingKind::Cls => "cls",
            PoolingKind::Mean => "mean",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastembedProviderOptions {
    pub manifest_path: Option<PathBuf>,
    pub model: Option<String>,
    pub model_path: Option<PathBuf>,
    pub file: Option<String>,
    pub pooling: Option<PoolingKind>,
    pub query_prefix: Option<String>,
    pub quantization: Option<QuantizationKind>,
    pub token_source_env: Option<String>,
    pub cache_dir: Option<PathBuf>,
}

impl Default for FastembedProviderOptions {
    fn default() -> Self {
        Self {
            manifest_path: None,
            model: Some(format!("hf:{DEFAULT_HF_MODEL_ID}")),
            model_path: None,
            file: None,
            pooling: None,
            query_prefix: None,
            quantization: None,
            token_source_env: None,
            cache_dir: None,
        }
    }
}

pub fn is_builtin_preset_id(value: &str) -> bool {
    BUILTIN_PRESET_IDS.contains(&value)
}

pub fn builtin_preset_hf_reference(value: &str) -> Option<HfModelReference> {
    builtin_preset(value).map(|preset| HfModelReference {
        model_id: preset.model_id.to_string(),
        revision: preset.revision.to_string(),
    })
}

#[derive(Debug, Clone, Copy)]
struct BuiltinPresetSpec {
    id: &'static str,
    model_id: &'static str,
    revision: &'static str,
    model_file: &'static str,
    external_initializer: Option<(&'static str, &'static str)>,
    pooling: PoolingKind,
    query_prefix: Option<&'static str>,
    document_prefix: Option<&'static str>,
    normalization: NormalizationKind,
    native_dimension: usize,
    output_dimension: usize,
    max_length: usize,
    quantization: QuantizationKind,
}

fn builtin_preset(id: &str) -> Option<BuiltinPresetSpec> {
    match id {
        "arctic-m-v2-fp32" => Some(BuiltinPresetSpec {
            id: "arctic-m-v2-fp32",
            model_id: "Snowflake/snowflake-arctic-embed-m-v2.0",
            revision: ARCTIC_M_V2_REVISION,
            model_file: "onnx/model.onnx",
            external_initializer: None,
            pooling: PoolingKind::Cls,
            query_prefix: Some(DEFAULT_QUERY_PREFIX),
            document_prefix: Some(""),
            normalization: NormalizationKind::L2,
            native_dimension: 768,
            output_dimension: 768,
            max_length: 8192,
            quantization: QuantizationKind::None,
        }),
        "granite-97m-multilingual-r2-int8-static" => Some(BuiltinPresetSpec {
            id: "granite-97m-multilingual-r2-int8-static",
            model_id: "ibm-granite/granite-embedding-97m-multilingual-r2",
            revision: GRANITE_97M_R2_REVISION,
            model_file: "onnx/model_quint8_avx2.onnx",
            external_initializer: None,
            pooling: PoolingKind::Cls,
            query_prefix: Some(""),
            document_prefix: Some(""),
            normalization: NormalizationKind::L2,
            native_dimension: 384,
            output_dimension: 384,
            max_length: 32_768,
            quantization: QuantizationKind::Static,
        }),
        "granite-311m-multilingual-r2-int8-static" => Some(BuiltinPresetSpec {
            id: "granite-311m-multilingual-r2-int8-static",
            model_id: "ibm-granite/granite-embedding-311m-multilingual-r2",
            revision: GRANITE_311M_R2_REVISION,
            model_file: "onnx/model_quint8_avx2.onnx",
            external_initializer: None,
            pooling: PoolingKind::Cls,
            query_prefix: Some(""),
            document_prefix: Some(""),
            normalization: NormalizationKind::L2,
            native_dimension: 768,
            output_dimension: 768,
            max_length: 32_768,
            quantization: QuantizationKind::Static,
        }),
        "arctic-l-v2-fp32" => Some(BuiltinPresetSpec {
            id: "arctic-l-v2-fp32",
            model_id: DEFAULT_HF_MODEL_ID,
            revision: DEFAULT_HF_MODEL_REVISION,
            model_file: DEFAULT_HF_MODEL_FILE,
            external_initializer: Some(("onnx/model.onnx_data", "model.onnx_data")),
            pooling: PoolingKind::Cls,
            query_prefix: Some(DEFAULT_QUERY_PREFIX),
            document_prefix: Some(""),
            normalization: NormalizationKind::L2,
            native_dimension: 1024,
            output_dimension: 1024,
            max_length: 8192,
            quantization: QuantizationKind::None,
        }),
        _ => None,
    }
}

fn preset_for_compatible_legacy_hf(
    reference: &HfModelReference,
    options: &FastembedProviderOptions,
) -> Option<BuiltinPresetSpec> {
    BUILTIN_PRESET_IDS
        .iter()
        .filter_map(|id| builtin_preset(id))
        .find(|preset| {
            preset.model_id == reference.model_id
                && preset.revision == reference.revision
                && options
                    .file
                    .as_deref()
                    .is_none_or(|file| file == preset.model_file)
                && options
                    .pooling
                    .is_none_or(|pooling| pooling == preset.pooling)
                && options
                    .quantization
                    .is_none_or(|quantization| quantization == preset.quantization)
                && options.query_prefix.as_deref().is_none_or(|prefix| {
                    preset
                        .query_prefix
                        .is_some_and(|expected| prefix == expected)
                })
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfModelReference {
    pub model_id: String,
    pub revision: String,
}

pub fn default_hf_model_reference() -> HfModelReference {
    HfModelReference {
        model_id: DEFAULT_HF_MODEL_ID.to_string(),
        revision: DEFAULT_HF_MODEL_REVISION.to_string(),
    }
}

pub fn parse_hf_model_reference(model: &str) -> Option<HfModelReference> {
    let reference = model.strip_prefix("hf:")?;
    let (model_id, explicit_revision) = match reference.rsplit_once('@') {
        Some((model_id, revision)) => (model_id, Some(revision)),
        None => (reference, None),
    };
    let revision = explicit_revision.unwrap_or_else(|| {
        if model_id == DEFAULT_HF_MODEL_ID {
            DEFAULT_HF_MODEL_REVISION
        } else {
            "main"
        }
    });
    if model_id.is_empty() || revision.is_empty() {
        return None;
    }
    Some(HfModelReference {
        model_id: model_id.to_string(),
        revision: revision.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelSnapshot {
    pub model_id: Option<String>,
    pub model_revision: String,
    pub model_file: String,
    pub query_prefix: String,
    pub pooling: PoolingKind,
    paths: BTreeMap<String, PathBuf>,
}

impl ResolvedModelSnapshot {
    pub fn path_for(&self, file: &str) -> Option<&Path> {
        self.paths.get(file).map(PathBuf::as_path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingFingerprintSeed {
    pub provider: String,
    pub model_id: String,
    pub model_revision: String,
    pub pooling: PoolingKind,
    pub query_prefix: String,
}

impl EmbeddingFingerprintSeed {
    pub fn with_dimension(self, dimension: usize) -> EmbeddingFingerprint {
        EmbeddingFingerprint {
            schema_version: EMBEDDING_FINGERPRINT_SCHEMA_VERSION.to_string(),
            provider: self.provider,
            model_id: self.model_id,
            model_revision: self.model_revision,
            dimension,
            pooling: self.pooling,
            query_prefix: self.query_prefix,
            chunker_version: CHUNKER_VERSION.to_string(),
            source_schema_version: SOURCE_SCHEMA_VERSION.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct EmbeddingFingerprint {
    pub schema_version: String,
    pub provider: String,
    pub model_id: String,
    pub model_revision: String,
    pub dimension: usize,
    pub pooling: PoolingKind,
    pub query_prefix: String,
    pub chunker_version: String,
    pub source_schema_version: String,
}

impl EmbeddingFingerprint {
    pub fn hash(&self) -> String {
        let encoded = serde_json::to_vec(self).expect("embedding fingerprint serializes");
        hex_digest(&Sha256::digest(encoded))
    }

    /// `None` expectation fields defer to the model's own defaults rather
    /// than acting as unchecked wildcards: inferred pooling, query prefix,
    /// and the embedding dimension are deterministic functions of the model
    /// files plus the explicit config, and the model files are fixed by
    /// (`model_id`, immutable `model_revision`) — both always compared.
    /// `model_path` snapshots rely on the user keeping the local directory
    /// contents stable.
    pub fn matches_expectation(&self, expectation: &EmbeddingFingerprintExpectation) -> bool {
        self.schema_version == EMBEDDING_FINGERPRINT_SCHEMA_VERSION
            && self.provider == expectation.provider
            && expectation
                .model_id
                .as_ref()
                .is_none_or(|model_id| &self.model_id == model_id)
            && expectation
                .model_revision
                .as_ref()
                .is_none_or(|revision| &self.model_revision == revision)
            && expectation
                .pooling
                .is_none_or(|pooling| self.pooling == pooling)
            && expectation
                .query_prefix
                .as_ref()
                .is_none_or(|prefix| &self.query_prefix == prefix)
            && self.chunker_version == CHUNKER_VERSION
            && self.source_schema_version == SOURCE_SCHEMA_VERSION
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingFingerprintExpectation {
    pub provider: String,
    pub model_id: Option<String>,
    pub model_revision: Option<String>,
    pub pooling: Option<PoolingKind>,
    pub query_prefix: Option<String>,
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("hex write cannot fail");
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManualModelBehavior {
    pub file: Option<String>,
    pub pooling: Option<PoolingKind>,
    pub query_prefix: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingProviderError {
    code: String,
    message: String,
    details: Value,
    hint: Option<String>,
}

impl EmbeddingProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self::structured("embedding.failure", message)
    }

    pub fn structured(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: json!({}),
            hint: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn details(&self) -> &Value {
        &self.details
    }

    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }
}

impl fmt::Display for EmbeddingProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for EmbeddingProviderError {}

pub fn default_hf_cache_dir() -> Result<PathBuf, EmbeddingProviderError> {
    let cache_home = if let Some(value) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(value)
    } else {
        let Some(home) = std::env::var_os("HOME") else {
            return Err(EmbeddingProviderError::structured(
                "embedding.cache_unavailable",
                "XDG_CACHE_HOME is not set and HOME is unavailable.",
            ));
        };
        PathBuf::from(home).join(".cache")
    };
    Ok(cache_home.join("qgh").join("hf"))
}

pub fn default_prepared_model_store() -> Result<PreparedModelStore, EmbeddingProviderError> {
    let hf_cache = default_hf_cache_dir()?;
    let qgh_cache = hf_cache.parent().ok_or_else(|| {
        EmbeddingProviderError::structured(
            "embedding.cache_unavailable",
            "Could not resolve the qgh model cache directory.",
        )
    })?;
    Ok(PreparedModelStore::new(qgh_cache.join("prepared-models")))
}

pub trait ModelRepository {
    fn get(&mut self, file: &str) -> Result<PathBuf, EmbeddingProviderError>;
    fn list_files(&mut self) -> Result<Vec<String>, EmbeddingProviderError>;
    fn revision(&mut self) -> Result<Option<String>, EmbeddingProviderError> {
        Ok(None)
    }
}

pub fn resolve_hf_model_snapshot(
    model_id: &str,
    manual: ManualModelBehavior,
    repo: &mut dyn ModelRepository,
) -> Result<ResolvedModelSnapshot, EmbeddingProviderError> {
    let mut paths = BTreeMap::new();
    let mut attempted_files = Vec::new();
    let mut modules_json = None;
    let mut pooling_json = None;
    let mut pooling_config_file = DEFAULT_POOLING_CONFIG_FILE.to_string();

    if manual.pooling.is_none() || manual.query_prefix.is_none() {
        attempted_files.push(MODULES_FILE.to_string());
        if let Some((path, json)) = read_optional_metadata(repo, MODULES_FILE)? {
            pooling_config_file = pooling_config_file_from_modules(&json)
                .unwrap_or_else(|| DEFAULT_POOLING_CONFIG_FILE.to_string());
            paths.insert(MODULES_FILE.to_string(), path);
            modules_json = Some(json);
        }
    }

    if manual.pooling.is_none() {
        attempted_files.push(pooling_config_file.clone());
        if let Some((path, json)) = read_optional_metadata(repo, &pooling_config_file)? {
            paths.insert(pooling_config_file.clone(), path);
            pooling_json = Some(json);
        }
    }

    let model_file = if let Some(file) = manual.file {
        Some(file)
    } else if let Some(file) = known_default_model_file(model_id) {
        Some(file)
    } else {
        detect_onnx_file(repo)?
    };
    let pooling = manual
        .pooling
        .or_else(|| pooling_json.as_ref().and_then(infer_pooling));
    let query_prefix = manual
        .query_prefix
        .or_else(|| modules_json.as_ref().and_then(infer_query_prefix));

    let behavior = require_model_behavior(
        Some(model_id),
        model_file,
        pooling,
        query_prefix,
        &attempted_files,
    )?;

    for file in required_runtime_files(&behavior.model_file) {
        if paths.contains_key(file) {
            continue;
        }
        let path = repo.get(file)?;
        paths.insert(file.to_string(), path);
    }

    // ONNX graphs above 2GB ship their weights as an external `<model>_data`
    // companion that ort resolves relative to the graph file, so it must be
    // downloaded into the same snapshot directory or session init fails.
    let companion = format!("{}_data", behavior.model_file);
    if !paths.contains_key(&companion) && repo.list_files()?.iter().any(|file| file == &companion) {
        let path = repo.get(&companion)?;
        paths.insert(companion, path);
    }

    Ok(ResolvedModelSnapshot {
        model_id: Some(model_id.to_string()),
        model_revision: repo.revision()?.unwrap_or_else(|| "unknown".to_string()),
        model_file: behavior.model_file,
        query_prefix: behavior.query_prefix,
        pooling: behavior.pooling,
        paths,
    })
}

pub fn resolve_model_path_snapshot(
    model_path: &Path,
    manual: ManualModelBehavior,
) -> Result<ResolvedModelSnapshot, EmbeddingProviderError> {
    let (root, model_file) = if model_path.is_file() {
        let root = model_root_for_file(model_path)?;
        let model_file = relative_file_name(&root, model_path)?;
        (root, Some(model_file))
    } else if model_path.is_dir() {
        let model_file = manual
            .file
            .clone()
            .or_else(|| detect_local_onnx_file(model_path));
        (model_path.to_path_buf(), model_file)
    } else {
        return Err(EmbeddingProviderError::structured(
            "embedding.model_path_not_found",
            "Embedding model_path does not exist.",
        )
        .with_details(json!({ "model_path": model_path.display().to_string() })));
    };

    let modules_path = root.join(MODULES_FILE);
    let pooling_config_file = if modules_path.exists() {
        let modules_json = read_json_file(&modules_path, MODULES_FILE)?;
        pooling_config_file_from_modules(&modules_json)
            .unwrap_or_else(|| DEFAULT_POOLING_CONFIG_FILE.to_string())
    } else {
        DEFAULT_POOLING_CONFIG_FILE.to_string()
    };
    let pooling_path = root.join(&pooling_config_file);

    let modules_json = if modules_path.exists() {
        Some(read_json_file(&modules_path, MODULES_FILE)?)
    } else {
        None
    };
    let pooling_json = if pooling_path.exists() {
        Some(read_json_file(&pooling_path, &pooling_config_file)?)
    } else {
        None
    };
    let pooling = manual
        .pooling
        .or_else(|| pooling_json.as_ref().and_then(infer_pooling));
    let query_prefix = manual
        .query_prefix
        .or_else(|| modules_json.as_ref().and_then(infer_query_prefix));
    let behavior = require_model_behavior(
        None,
        model_file,
        pooling,
        query_prefix,
        &[MODULES_FILE.to_string(), pooling_config_file.clone()],
    )?;

    let mut paths = BTreeMap::new();
    paths.insert(behavior.model_file.clone(), root.join(&behavior.model_file));
    for file in TOKENIZER_FILES {
        paths.insert(file.to_string(), root.join(file));
    }
    if modules_path.exists() {
        paths.insert(MODULES_FILE.to_string(), modules_path);
    }
    if pooling_path.exists() {
        paths.insert(pooling_config_file, pooling_path);
    }

    Ok(ResolvedModelSnapshot {
        model_id: None,
        model_revision: LOCAL_MODEL_REVISION.to_string(),
        model_file: behavior.model_file,
        query_prefix: behavior.query_prefix,
        pooling: behavior.pooling,
        paths,
    })
}

pub fn required_runtime_files(model_file: &str) -> Vec<&str> {
    let mut files = vec![model_file];
    files.extend(TOKENIZER_FILES);
    files.sort_unstable();
    files.dedup();
    files
}

#[cfg(feature = "fastembed-provider")]
pub struct FastembedEngine {
    model: Mutex<fastembed::TextEmbedding>,
}

#[cfg(feature = "fastembed-provider")]
pub struct FastembedTokenizer {
    tokenizer: tokenizers::Tokenizer,
}

#[cfg(feature = "fastembed-provider")]
fn fastembed_user_defined_model(
    snapshot: &PreparedModelSnapshot,
) -> Result<fastembed::UserDefinedEmbeddingModel, EmbeddingProviderError> {
    use fastembed::{QuantizationMode, TokenizerFiles, UserDefinedEmbeddingModel};

    let mut payload = snapshot.take_runtime_payload()?;
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: payload.take_one(ArtifactRole::Tokenizer)?,
        config_file: payload.take_one(ArtifactRole::Config)?,
        special_tokens_map_file: payload.take_one(ArtifactRole::SpecialTokensMap)?,
        tokenizer_config_file: payload.take_one(ArtifactRole::TokenizerConfig)?,
    };
    let mut model =
        UserDefinedEmbeddingModel::new(payload.take_one(ArtifactRole::OnnxModel)?, tokenizer_files)
            .with_pooling(match snapshot.manifest.pooling {
                PoolingKind::Cls => fastembed::Pooling::Cls,
                PoolingKind::Mean => fastembed::Pooling::Mean,
            })
            .with_quantization(match snapshot.manifest.quantization {
                QuantizationKind::None => QuantizationMode::None,
                QuantizationKind::Static => QuantizationMode::Static,
                QuantizationKind::Dynamic => {
                    return Err(EmbeddingProviderError::structured(
                "embedding.dynamic_quantization_unsupported",
                "Dynamic quantization is not supported for persistent embedding generations.",
            ));
                }
            });
    for artifact in snapshot
        .manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.role == ArtifactRole::OnnxExternalData)
    {
        let initializer_name = artifact.external_initializer_name.clone().ok_or_else(|| {
            EmbeddingProviderError::structured(
                "embedding.external_initializer_invalid",
                "External initializer is missing its graph file name.",
            )
        })?;
        model = model.with_external_initializer(
            initializer_name,
            payload.take_relative(ArtifactRole::OnnxExternalData, &artifact.relative_path)?,
        );
    }
    Ok(model)
}

#[cfg(feature = "fastembed-provider")]
impl FastembedEngine {
    pub fn from_prepared_snapshot(
        snapshot: &PreparedModelSnapshot,
    ) -> Result<Self, EmbeddingProviderError> {
        use fastembed::{InitOptionsUserDefined, TextEmbedding};

        let model = TextEmbedding::try_new_from_user_defined(
            fastembed_user_defined_model(snapshot)?,
            InitOptionsUserDefined::new().with_max_length(snapshot.manifest.max_length),
        )
        .map_err(|_| {
            EmbeddingProviderError::structured(
                "embedding.fastembed_init_failed",
                "Failed to initialize fastembed prepared model.",
            )
            .with_details(json!({ "role": "runtime" }))
        })?;
        Ok(Self {
            model: Mutex::new(model),
        })
    }

    pub fn from_snapshot(snapshot: &ResolvedModelSnapshot) -> Result<Self, EmbeddingProviderError> {
        use fastembed::{
            InitOptionsUserDefined, QuantizationMode, TextEmbedding, TokenizerFiles,
            UserDefinedEmbeddingModel,
        };

        let model_path = required_path(snapshot, &snapshot.model_file)?;
        let tokenizer_files = TokenizerFiles {
            tokenizer_file: fs::read(required_path(snapshot, "tokenizer.json")?)?,
            config_file: fs::read(required_path(snapshot, "config.json")?)?,
            special_tokens_map_file: fs::read(required_path(snapshot, "special_tokens_map.json")?)?,
            tokenizer_config_file: fs::read(required_path(snapshot, "tokenizer_config.json")?)?,
        };
        let mut model = UserDefinedEmbeddingModel::new(fs::read(model_path)?, tokenizer_files)
            .with_pooling(match snapshot.pooling {
                PoolingKind::Cls => fastembed::Pooling::Cls,
                PoolingKind::Mean => fastembed::Pooling::Mean,
            });
        if snapshot.model_file.contains("quant") {
            model = model.with_quantization(QuantizationMode::Dynamic);
        }
        let model = TextEmbedding::try_new_from_user_defined(
            model,
            InitOptionsUserDefined::new().with_max_length(8192),
        )
        .map_err(|_| {
            EmbeddingProviderError::structured(
                "embedding.fastembed_init_failed",
                "Failed to initialize fastembed local model.",
            )
            .with_details(json!({ "role": "runtime" }))
        })?;
        Ok(Self {
            model: Mutex::new(model),
        })
    }
}

#[cfg(feature = "fastembed-provider")]
const FASTEMBED_BATCH_SIZE: usize = 16;

#[cfg(feature = "fastembed-provider")]
impl EmbeddingEngine for FastembedEngine {
    fn embed_texts(
        &self,
        texts: &[String],
    ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
        // fastembed's default batch size (256) with ~900-token chunks blows
        // past tens of GB of activation memory on the large model; keep CPU
        // inference batches small so a full-corpus embed stays bounded.
        self.model
            .lock()
            .expect("fastembed mutex poisoned")
            .embed(texts, Some(FASTEMBED_BATCH_SIZE))
            .map_err(|error| {
                EmbeddingProviderError::structured(
                    "embedding.fastembed_failed",
                    "fastembed failed to produce embeddings.",
                )
                .with_details(json!({ "error": error.to_string() }))
            })
    }
}

#[cfg(feature = "fastembed-provider")]
impl FastembedTokenizer {
    fn from_verified_bytes(bytes: Vec<u8>) -> Result<Self, EmbeddingProviderError> {
        let tokenizer = tokenizers::Tokenizer::from_bytes(bytes).map_err(|_| {
            EmbeddingProviderError::structured(
                "embedding.tokenizer_init_failed",
                "Failed to initialize prepared embedding tokenizer.",
            )
            .with_details(json!({ "role": ArtifactRole::Tokenizer }))
        })?;
        Ok(Self { tokenizer })
    }

    pub fn from_prepared_snapshot(
        snapshot: &PreparedModelSnapshot,
    ) -> Result<Self, EmbeddingProviderError> {
        let tokenizer = snapshot.runtime_tokenizer()?;
        Ok(Self { tokenizer })
    }

    pub fn from_snapshot(snapshot: &ResolvedModelSnapshot) -> Result<Self, EmbeddingProviderError> {
        let tokenizer_path = required_path(snapshot, "tokenizer.json")?;
        let tokenizer = tokenizer_from_local_bytes(
            tokenizer_path,
            "Failed to initialize embedding tokenizer.",
        )?;
        Ok(Self { tokenizer })
    }

    pub fn from_options(options: FastembedProviderOptions) -> Result<Self, EmbeddingProviderError> {
        let snapshot = resolve_fastembed_snapshot(options)?;
        Self::from_snapshot(&snapshot)
    }
}

#[cfg(feature = "fastembed-provider")]
fn tokenizer_from_local_bytes(
    path: &Path,
    message: &'static str,
) -> Result<tokenizers::Tokenizer, EmbeddingProviderError> {
    let bytes = fs::read(path).map_err(|_| {
        EmbeddingProviderError::structured("embedding.tokenizer_init_failed", message)
            .with_details(json!({ "role": ArtifactRole::Tokenizer }))
    })?;
    tokenizers::Tokenizer::from_bytes(bytes).map_err(|_| {
        EmbeddingProviderError::structured("embedding.tokenizer_init_failed", message)
            .with_details(json!({ "role": ArtifactRole::Tokenizer }))
    })
}

#[cfg(feature = "fastembed-provider")]
impl EmbeddingTokenizer for FastembedTokenizer {
    fn tokenize(&self, text: &str) -> Result<Vec<TokenSpan>, EmbeddingProviderError> {
        Ok(self.tokenize_canonical(text)?.spans)
    }

    // The HF tokenizer reports offsets against its normalizer's output, and
    // the XLM-R Precompiled normalizer drops bytes (e.g. collapses newlines)
    // without adjusting offset alignment, so offsets drift against the
    // original text. Normalizing first and encoding the normalized text makes
    // the offsets valid byte ranges into that text; only the leading
    // Metaspace meta-token can still overlap its successor, which the
    // monotonic clamp below removes.
    fn tokenize_canonical(&self, text: &str) -> Result<TokenizedText, EmbeddingProviderError> {
        use tokenizers::Normalizer;

        let tokenizer_failed = |error: tokenizers::Error| {
            EmbeddingProviderError::structured(
                "embedding.tokenizer_failed",
                "Embedding tokenizer failed to tokenize source text.",
            )
            .with_details(json!({ "error": error.to_string() }))
        };
        let normalized = match self.tokenizer.get_normalizer() {
            Some(normalizer) => {
                let mut normalized = tokenizers::NormalizedString::from(text);
                normalizer
                    .normalize(&mut normalized)
                    .map_err(tokenizer_failed)?;
                normalized
            }
            None => tokenizers::NormalizedString::from(text),
        };
        let canonical = normalized.get().to_string();
        let encoding = self
            .tokenizer
            .encode(canonical.as_str(), false)
            .map_err(tokenizer_failed)?;
        let mut spans = Vec::new();
        let mut original_spans = Vec::new();
        let mut previous_end = 0usize;
        for (start, end) in encoding.get_offsets() {
            let start = (*start).max(previous_end);
            if start >= *end {
                continue;
            }
            let original = normalized
                .convert_offsets(tokenizers::tokenizer::normalizer::Range::Normalized(
                    start..*end,
                ))
                .ok_or_else(|| {
                    EmbeddingProviderError::structured(
                        "embedding.tokenizer_unmappable_offset",
                        "Embedding tokenizer produced a span that cannot map to the original source.",
                    )
                })?;
            if original.start >= original.end
                || original.end > text.len()
                || !text.is_char_boundary(original.start)
                || !text.is_char_boundary(original.end)
            {
                return Err(EmbeddingProviderError::structured(
                    "embedding.tokenizer_unmappable_offset",
                    "Embedding tokenizer produced an invalid original-source span.",
                ));
            }
            spans.push(TokenSpan { start, end: *end });
            original_spans.push(TokenSpan {
                start: original.start,
                end: original.end,
            });
            previous_end = *end;
        }
        Ok(TokenizedText {
            text: canonical,
            spans,
            original_text: text.to_string(),
            original_spans,
        })
    }
}

#[cfg(feature = "fastembed-provider")]
impl LocalEmbeddingProvider<FastembedEngine> {
    pub fn from_options(options: FastembedProviderOptions) -> Result<Self, EmbeddingProviderError> {
        let snapshot = resolve_fastembed_snapshot(options)?;
        let engine = FastembedEngine::from_snapshot(&snapshot)?;
        Ok(Self::new(engine, snapshot.query_prefix))
    }
}

#[cfg(feature = "fastembed-provider")]
pub fn resolve_fastembed_snapshot(
    options: FastembedProviderOptions,
) -> Result<ResolvedModelSnapshot, EmbeddingProviderError> {
    if let Some(model_path) = options.model_path {
        return resolve_model_path_snapshot(
            &model_path,
            ManualModelBehavior {
                file: options.file,
                pooling: options.pooling,
                query_prefix: options.query_prefix,
            },
        );
    }
    let reference = hf_model_reference(options.model.as_deref())?;
    let cache_dir = options
        .cache_dir
        .map(Ok)
        .unwrap_or_else(default_hf_cache_dir)?;
    let mut discovery = HfHubModelRepository::new(
        &reference.model_id,
        &reference.revision,
        cache_dir.clone(),
        options.token_source_env.clone(),
    )?;
    let resolved_revision = discovery.revision()?.ok_or_else(|| {
        EmbeddingProviderError::structured(
            "embedding.manifest_revision_invalid",
            "Model acquisition did not resolve an immutable revision.",
        )
    })?;
    if !is_commit_sha(&resolved_revision) {
        return Err(EmbeddingProviderError::structured(
            "embedding.manifest_revision_invalid",
            "Model acquisition did not resolve an immutable revision.",
        ));
    }
    let mut repo = HfHubModelRepository::new(
        &reference.model_id,
        &resolved_revision,
        cache_dir,
        options.token_source_env,
    )?;
    let snapshot = resolve_hf_model_snapshot(
        &reference.model_id,
        ManualModelBehavior {
            file: options.file,
            pooling: options.pooling,
            query_prefix: options.query_prefix,
        },
        &mut repo,
    )?;
    if snapshot.model_revision != resolved_revision {
        return Err(EmbeddingProviderError::structured(
            "embedding.hf_revision_mismatch",
            "Resolved model artifacts do not match the pinned revision.",
        ));
    }
    Ok(snapshot)
}

#[cfg(feature = "fastembed-provider")]
struct HfHubModelRepository {
    repo: hf_hub::api::sync::ApiRepo,
    info: Option<hf_hub::api::RepoInfo>,
    cache_root: PathBuf,
}

#[cfg(feature = "fastembed-provider")]
impl HfHubModelRepository {
    fn new(
        model_id: &str,
        model_revision: &str,
        cache_dir: PathBuf,
        token_source_env: Option<String>,
    ) -> Result<Self, EmbeddingProviderError> {
        fs::create_dir_all(&cache_dir)?;
        let cache_root = fs::canonicalize(&cache_dir)?;
        let token = token_source_env
            .map(|env| {
                std::env::var(&env).map_err(|_| {
                    EmbeddingProviderError::structured(
                        "embedding.token_unavailable",
                        "Configured embedding token environment variable is not set.",
                    )
                    .with_details(json!({ "token_source": { "type": "env", "env": env } }))
                })
            })
            .transpose()?;
        let api = hf_hub::api::sync::ApiBuilder::new()
            .with_cache_dir(cache_dir)
            .with_endpoint(HUGGINGFACE_ENDPOINT.to_string())
            .with_progress(false)
            .with_token(token)
            .build()
            .map_err(|error| {
                EmbeddingProviderError::structured(
                    "embedding.hf_client_failed",
                    "Failed to initialize Hugging Face model client.",
                )
                .with_details(json!({
                    "host": HUGGINGFACE_ENDPOINT,
                    "error": error.to_string()
                }))
            })?;
        Ok(Self {
            repo: api.repo(hf_hub::Repo::with_revision(
                model_id.to_string(),
                hf_hub::RepoType::Model,
                model_revision.to_string(),
            )),
            info: None,
            cache_root,
        })
    }

    fn info(&mut self) -> Result<&hf_hub::api::RepoInfo, EmbeddingProviderError> {
        if self.info.is_none() {
            self.info = Some(self.repo.info().map_err(|error| {
                EmbeddingProviderError::structured(
                    "embedding.hf_model_info_failed",
                    "Failed to inspect Hugging Face model revision.",
                )
                .with_details(json!({
                    "host": HUGGINGFACE_ENDPOINT,
                    "error": error.to_string()
                }))
            })?);
        }
        Ok(self.info.as_ref().expect("info initialized"))
    }
}

#[cfg(feature = "fastembed-provider")]
impl ModelRepository for HfHubModelRepository {
    fn get(&mut self, file: &str) -> Result<PathBuf, EmbeddingProviderError> {
        let path = self.repo.get(file).map_err(|error| {
            EmbeddingProviderError::structured(
                "embedding.hf_download_failed",
                "Failed to download required Hugging Face model file.",
            )
            .with_details(json!({
                "host": HUGGINGFACE_ENDPOINT,
                "file": file,
                "error": error.to_string()
            }))
        })?;
        confined_hf_cache_artifact(&self.cache_root, &path, file)
    }

    fn list_files(&mut self) -> Result<Vec<String>, EmbeddingProviderError> {
        Ok(self
            .info()?
            .siblings
            .clone()
            .into_iter()
            .map(|sibling| sibling.rfilename)
            .collect())
    }

    fn revision(&mut self) -> Result<Option<String>, EmbeddingProviderError> {
        // Record the resolved commit sha, not the configured revision:
        // a mutable revision name ("main", tags) can point at different
        // model files over time, which would let inferred pooling, query
        // prefix, and dimension drift behind an unchanged fingerprint.
        Ok(Some(self.info()?.sha.clone()))
    }
}

#[cfg(feature = "fastembed-provider")]
fn confined_hf_cache_artifact(
    cache_root: &Path,
    path: &Path,
    file: &str,
) -> Result<PathBuf, EmbeddingProviderError> {
    let canonical_path = fs::canonicalize(path).map_err(|_| {
        EmbeddingProviderError::structured(
            "embedding.hf_cache_invalid",
            "Downloaded Hugging Face artifact could not be resolved in the local cache.",
        )
        .with_details(json!({ "file": file }))
    })?;
    if !canonical_path.starts_with(cache_root) {
        return Err(EmbeddingProviderError::structured(
            "embedding.hf_cache_invalid",
            "Downloaded Hugging Face artifact escapes the configured local cache.",
        )
        .with_details(json!({ "file": file })));
    }
    Ok(canonical_path)
}

#[cfg(feature = "fastembed-provider")]
fn hf_model_reference(model: Option<&str>) -> Result<HfModelReference, EmbeddingProviderError> {
    let Some(model) = model else {
        return Ok(default_hf_model_reference());
    };
    parse_hf_model_reference(model).ok_or_else(|| {
        EmbeddingProviderError::structured(
            "embedding.invalid_model",
            "Embedding model must use `hf:<org>/<repo>[@revision]`.",
        )
        .with_details(json!({ "model": model }))
    })
}

#[cfg(feature = "fastembed-provider")]
fn required_path<'a>(
    snapshot: &'a ResolvedModelSnapshot,
    file: &str,
) -> Result<&'a Path, EmbeddingProviderError> {
    snapshot.path_for(file).ok_or_else(|| {
        EmbeddingProviderError::structured(
            "embedding.model_file_missing",
            "Resolved model snapshot is missing a required file.",
        )
        .with_details(json!({ "file": file }))
    })
}

fn read_json_file(path: &Path, label: &str) -> Result<Value, EmbeddingProviderError> {
    let bytes = fs::read(path).map_err(|error| {
        EmbeddingProviderError::structured(
            "embedding.model_metadata_unreadable",
            "Failed to read model metadata file.",
        )
        .with_details(json!({ "file": label, "error": error.to_string() }))
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        EmbeddingProviderError::structured(
            "embedding.model_metadata_invalid",
            "Model metadata file is not valid JSON.",
        )
        .with_details(json!({ "file": label, "error": error.to_string() }))
    })
}

fn read_optional_metadata(
    repo: &mut dyn ModelRepository,
    file: &str,
) -> Result<Option<(PathBuf, Value)>, EmbeddingProviderError> {
    let path = match repo.get(file) {
        Ok(path) => path,
        Err(error) if metadata_fetch_can_be_recovered(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    match read_json_file(&path, file) {
        Ok(json) => Ok(Some((path, json))),
        Err(error) if metadata_fetch_can_be_recovered(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn metadata_fetch_can_be_recovered(error: &EmbeddingProviderError) -> bool {
    matches!(
        error.code(),
        "embedding.hf_download_failed"
            | "embedding.model_metadata_invalid"
            | "embedding.model_metadata_unreadable"
    )
}

fn pooling_config_file_from_modules(modules: &Value) -> Option<String> {
    modules.as_array()?.iter().find_map(|module| {
        let object = module.as_object()?;
        let module_type = object.get("type")?.as_str()?;
        if !module_type.contains("Pooling") {
            return None;
        }
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .or_else(|| object.get("name").and_then(Value::as_str))?;
        Some(format!("{}/config.json", path.trim_matches('/')))
    })
}

fn infer_pooling(pooling_config: &Value) -> Option<PoolingKind> {
    if let Some(mode) = pooling_config
        .get("pooling_mode")
        .and_then(Value::as_str)
        .or_else(|| pooling_config.get("pooling").and_then(Value::as_str))
    {
        return parse_pooling_name(mode);
    }
    let cls = pooling_config
        .get("pooling_mode_cls_token")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mean = pooling_config
        .get("pooling_mode_mean_tokens")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match (cls, mean) {
        (true, false) => Some(PoolingKind::Cls),
        (false, true) => Some(PoolingKind::Mean),
        _ => None,
    }
}

fn parse_pooling_name(value: &str) -> Option<PoolingKind> {
    match value.to_ascii_lowercase().as_str() {
        "cls" | "cls_token" => Some(PoolingKind::Cls),
        "mean" | "mean_tokens" => Some(PoolingKind::Mean),
        _ => None,
    }
}

fn infer_query_prefix(modules: &Value) -> Option<String> {
    find_query_prefix(modules).filter(|prefix| prefix == DEFAULT_QUERY_PREFIX)
}

fn find_query_prefix(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            if let Some(prefix) = object.get("query_prefix").and_then(Value::as_str) {
                return Some(prefix.to_string());
            }
            if let Some(prefix) = object.get("query_prompt").and_then(Value::as_str) {
                return Some(prefix.to_string());
            }
            if let Some(prefix) = object
                .get("prompts")
                .and_then(|prompts| prompts.get("query"))
                .and_then(Value::as_str)
            {
                return Some(prefix.to_string());
            }
            object.values().find_map(find_query_prefix)
        }
        Value::Array(values) => values.iter().find_map(find_query_prefix),
        _ => None,
    }
}

fn known_default_model_file(model_id: &str) -> Option<String> {
    (model_id == DEFAULT_HF_MODEL_ID).then(|| DEFAULT_HF_MODEL_FILE.to_string())
}

fn detect_onnx_file(
    repo: &mut dyn ModelRepository,
) -> Result<Option<String>, EmbeddingProviderError> {
    let files = repo.list_files()?;
    Ok(detect_onnx_candidate(files.iter().map(String::as_str)))
}

fn detect_local_onnx_file(root: &Path) -> Option<String> {
    let candidates = [
        DEFAULT_HF_MODEL_FILE,
        "onnx/model_quantized.onnx",
        "onnx/model.onnx_data",
        "model.onnx",
        "model_quantized.onnx",
    ];
    candidates
        .into_iter()
        .find(|candidate| root.join(candidate).is_file() && candidate.ends_with(".onnx"))
        .map(ToString::to_string)
}

fn detect_onnx_candidate<'a>(files: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut candidates = files
        .filter(|file| file.ends_with(".onnx"))
        .filter(|file| !file.contains("q4"))
        .collect::<Vec<_>>();
    candidates.sort_unstable();
    for preferred in [DEFAULT_HF_MODEL_FILE, "onnx/model_quantized.onnx"] {
        if candidates.contains(&preferred) {
            return Some(preferred.to_string());
        }
    }
    (candidates.len() == 1).then(|| candidates[0].to_string())
}

struct RequiredBehavior {
    model_file: String,
    pooling: PoolingKind,
    query_prefix: String,
}

fn require_model_behavior(
    model_id: Option<&str>,
    file: Option<String>,
    pooling: Option<PoolingKind>,
    query_prefix: Option<String>,
    attempted_files: &[String],
) -> Result<RequiredBehavior, EmbeddingProviderError> {
    let mut missing = Vec::new();
    if file.is_none() {
        missing.push("file");
    }
    if pooling.is_none() {
        missing.push("pooling");
    }
    if query_prefix.is_none() {
        missing.push("query_prefix");
    }
    if !missing.is_empty() {
        return Err(EmbeddingProviderError::structured(
            "embedding.model_metadata_required",
            "Could not auto-detect model file, pooling, and query prefix from sentence-transformers metadata.",
        )
        .with_details(json!({
            "model": model_id,
            "missing_manual_keys": missing,
            "required_manual_keys": ["pooling", "query_prefix", "file"],
            "attempted_files": attempted_files
        }))
        .with_hint("Set embedding.pooling, embedding.query_prefix, and embedding.file explicitly."));
    }
    Ok(RequiredBehavior {
        model_file: file.expect("checked"),
        pooling: pooling.expect("checked"),
        query_prefix: query_prefix.expect("checked"),
    })
}

fn model_root_for_file(model_file: &Path) -> Result<PathBuf, EmbeddingProviderError> {
    let Some(parent) = model_file.parent() else {
        return Err(EmbeddingProviderError::structured(
            "embedding.model_path_invalid",
            "Embedding model_path must have a parent directory.",
        ));
    };
    if parent.file_name().is_some_and(|name| name == "onnx") {
        Ok(parent.parent().unwrap_or(parent).to_path_buf())
    } else {
        Ok(parent.to_path_buf())
    }
}

fn relative_file_name(root: &Path, file: &Path) -> Result<String, EmbeddingProviderError> {
    file.strip_prefix(root)
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .map_err(|_| {
            EmbeddingProviderError::structured(
                "embedding.model_path_invalid",
                "Embedding model_path must be inside its model root.",
            )
        })
}

impl From<std::io::Error> for EmbeddingProviderError {
    fn from(_: std::io::Error) -> Self {
        EmbeddingProviderError::structured("embedding.io", "Embedding provider I/O failed.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn issue_embedding_input_uses_exact_metadata_context_template() {
        let prepared = prepare_embedding_input(
            EmbeddingSourceContext::Issue {
                repository: "github.com/owner/repo",
                issue_number: 47,
                title: "Harden retrieval publication",
            },
            "Authoritative issue chunk.",
        );

        assert_eq!(
            prepared.context_template_version(),
            METADATA_CONTEXT_TEMPLATE_VERSION
        );
        assert_eq!(
            prepared.as_str(),
            "Repository: github.com/owner/repo\nIssue #47: Harden retrieval publication\n\nAuthoritative issue chunk."
        );
    }

    #[test]
    fn comment_embedding_input_uses_exact_parent_issue_context_template() {
        let prepared = prepare_embedding_input(
            EmbeddingSourceContext::Comment {
                repository: "github.com/owner/repo",
                parent_issue_number: 47,
                parent_issue_title: "Harden retrieval publication",
            },
            "Authoritative comment chunk.",
        );

        assert_eq!(
            prepared.as_str(),
            "Repository: github.com/owner/repo\nComment on issue #47: Harden retrieval publication\n\nAuthoritative comment chunk."
        );
    }

    #[test]
    fn metadata_context_output_and_hash_are_deterministic() {
        let context = EmbeddingSourceContext::Issue {
            repository: "github.com/owner/repo",
            issue_number: 47,
            title: "Harden retrieval publication",
        };
        let first = prepare_embedding_input(context, "Authoritative issue chunk.");
        let second = prepare_embedding_input(context, "Authoritative issue chunk.");

        assert_eq!(first, second);
        assert_eq!(
            first.context_hash("manifest-hash", "chunker-fingerprint"),
            crate::context::embedding_context_hash(
                "manifest-hash",
                "chunker-fingerprint",
                METADATA_CONTEXT_TEMPLATE_VERSION,
                first.as_str(),
            )
        );
        assert_eq!(
            first.context_hash("manifest-hash", "chunker-fingerprint"),
            second.context_hash("manifest-hash", "chunker-fingerprint")
        );
    }

    #[test]
    fn metadata_context_does_not_mutate_authoritative_text_or_include_mutable_fields() {
        let authoritative_chunk = String::from("State: preserve these source bytes exactly.\r\n");
        let original = authoritative_chunk.clone();
        let prepared = prepare_embedding_input(
            EmbeddingSourceContext::Issue {
                repository: "github.com/owner/repo",
                issue_number: 47,
                title: "Harden retrieval publication",
            },
            &authoritative_chunk,
        );

        assert_eq!(authoritative_chunk.as_bytes(), original.as_bytes());
        assert!(prepared.as_str().ends_with(&authoritative_chunk));
        for mutable_metadata in ["Labels: release-blocker", "Author: alice", "State: closed"] {
            assert!(!prepared.as_str().contains(mutable_metadata));
        }
    }

    #[test]
    fn parent_issue_title_change_invalidates_comment_context_hash() {
        let before = prepare_embedding_input(
            EmbeddingSourceContext::Comment {
                repository: "github.com/owner/repo",
                parent_issue_number: 47,
                parent_issue_title: "Old title",
            },
            "Unchanged authoritative comment chunk.",
        );
        let after = prepare_embedding_input(
            EmbeddingSourceContext::Comment {
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

    #[derive(Default)]
    struct RecordingEngine {
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl EmbeddingEngine for RecordingEngine {
        fn embed_texts(
            &self,
            texts: &[String],
        ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
            self.calls.lock().unwrap().push(texts.to_vec());
            Ok(texts.iter().map(|_| vec![1.0, 2.0, 3.0]).collect())
        }
    }

    #[test]
    fn default_hf_model_revision_is_pinned_commit() {
        assert_eq!(DEFAULT_HF_MODEL_REVISION.len(), 40);
        assert!(DEFAULT_HF_MODEL_REVISION
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn expectation_defaults_pin_model_id_and_revision() {
        let fingerprint = EmbeddingFingerprintSeed {
            provider: "local".to_string(),
            model_id: DEFAULT_HF_MODEL_ID.to_string(),
            model_revision: "superseded-commit-sha".to_string(),
            pooling: PoolingKind::Mean,
            query_prefix: DEFAULT_QUERY_PREFIX.to_string(),
        }
        .with_dimension(1024);
        let expectation = EmbeddingFingerprintExpectation {
            provider: "local".to_string(),
            model_id: Some(DEFAULT_HF_MODEL_ID.to_string()),
            model_revision: Some(DEFAULT_HF_MODEL_REVISION.to_string()),
            pooling: None,
            query_prefix: None,
        };
        assert!(!fingerprint.matches_expectation(&expectation));

        let current = EmbeddingFingerprintSeed {
            provider: "local".to_string(),
            model_id: DEFAULT_HF_MODEL_ID.to_string(),
            model_revision: DEFAULT_HF_MODEL_REVISION.to_string(),
            pooling: PoolingKind::Mean,
            query_prefix: DEFAULT_QUERY_PREFIX.to_string(),
        }
        .with_dimension(1024);
        assert!(current.matches_expectation(&expectation));
    }

    #[test]
    fn local_provider_prefixes_queries_only_and_records_dimension_dynamically() {
        let engine = RecordingEngine::default();
        let provider = LocalEmbeddingProvider::new(engine, DEFAULT_QUERY_PREFIX);

        let query = provider.embed_query("rate limit").unwrap();
        assert_eq!(query, vec![1.0, 2.0, 3.0]);
        let documents = provider
            .embed_documents(&["query: already document", "plain"])
            .unwrap();
        assert_eq!(documents.len(), 2);
        assert_eq!(provider.dimension(), Some(3));

        let calls = provider.engine.calls.lock().unwrap();
        assert_eq!(calls[0], vec!["query: rate limit"]);
        assert_eq!(calls[1], vec!["query: already document", "plain"]);
    }

    #[test]
    fn strict_manifest_rejects_unknown_fields_and_dynamic_quantization() {
        let mut manifest = fixture_manifest(Vec::new());
        let mut json = serde_json::to_value(&manifest).unwrap();
        json.as_object_mut()
            .unwrap()
            .insert("guessed_behavior".to_string(), json!(true));

        let error =
            ModelManifestV1::from_json_slice(&serde_json::to_vec(&json).unwrap()).unwrap_err();
        assert_eq!(error.code(), "embedding.manifest_invalid");

        manifest.quantization = QuantizationKind::Dynamic;
        let error = manifest.validate_contract().unwrap_err();
        assert_eq!(error.code(), "embedding.dynamic_quantization_unsupported");
    }

    #[test]
    fn manifest_rejects_non_production_context_template() {
        let mut manifest = fixture_manifest(Vec::new());
        manifest.context_template_version = "qgh.context.none.v1".to_string();

        let error = manifest.validate_contract().unwrap_err();

        assert_eq!(
            error.code(),
            "embedding.manifest_context_template_unsupported"
        );
    }

    #[test]
    fn prepared_store_rejects_escape_symlink_and_artifact_integrity_mismatch() {
        let root = temp_dir("qgh-prepared-store");
        let snapshot_root = root.join("snapshot");
        fs::create_dir_all(&snapshot_root).unwrap();
        let graph = snapshot_root.join("model.onnx");
        fs::write(&graph, b"onnx-bytes").unwrap();
        let artifact = fixture_artifact(ArtifactRole::OnnxModel, "model.onnx", b"onnx-bytes", None);
        let mut artifacts = vec![artifact.clone()];
        for (role, file) in [
            (ArtifactRole::Tokenizer, "tokenizer.json"),
            (ArtifactRole::Config, "config.json"),
            (ArtifactRole::SpecialTokensMap, "special_tokens_map.json"),
            (ArtifactRole::TokenizerConfig, "tokenizer_config.json"),
        ] {
            fs::write(snapshot_root.join(file), b"{}").unwrap();
            artifacts.push(fixture_artifact(role, file, b"{}", None));
        }
        let manifest_path = snapshot_root.join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&fixture_manifest(artifacts.clone())).unwrap(),
        )
        .unwrap();

        let store = PreparedModelStore::new(root.join("store"));
        let snapshot = store.load_manifest(&manifest_path).unwrap();
        let canonical_graph = fs::canonicalize(&graph).unwrap();
        assert_eq!(
            snapshot.path_for_role(ArtifactRole::OnnxModel),
            Some(canonical_graph.as_path())
        );

        let mut escaped = fixture_manifest(artifacts.clone());
        escaped.artifacts[0] = ModelArtifactV1 {
            relative_path: "../model.onnx".to_string(),
            ..artifact.clone()
        };
        fs::write(&manifest_path, serde_json::to_vec_pretty(&escaped).unwrap()).unwrap();
        let error = store.load_manifest(&manifest_path).unwrap_err();
        assert_eq!(error.code(), "embedding.artifact_path_invalid");

        escaped.artifacts[0].relative_path = "model.onnx".to_string();
        escaped.artifacts[0].sha256 = "0".repeat(64);
        fs::write(&manifest_path, serde_json::to_vec_pretty(&escaped).unwrap()).unwrap();
        let error = store.load_manifest(&manifest_path).unwrap_err();
        assert_eq!(error.code(), "embedding.artifact_checksum_mismatch");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let outside = root.join("outside.onnx");
            fs::write(&outside, b"onnx-bytes").unwrap();
            fs::remove_file(&graph).unwrap();
            symlink(&outside, &graph).unwrap();
            let manifest = fixture_manifest(artifacts);
            fs::write(
                &manifest_path,
                serde_json::to_vec_pretty(&manifest).unwrap(),
            )
            .unwrap();
            let error = store.load_manifest(&manifest_path).unwrap_err();
            assert_eq!(error.code(), "embedding.artifact_symlink_forbidden");
        }
    }

    #[test]
    fn inspection_is_body_free_but_verification_rejects_same_size_corruption() {
        let root = temp_dir("qgh-prepared-inspect-verify");
        let snapshot_root = root.join("snapshot");
        let manifest_path = write_prepared_manifest_fixture(&snapshot_root, b"graph-one");
        fs::write(snapshot_root.join("model.onnx"), b"graph-two").unwrap();

        let store = PreparedModelStore::new(root.join("store"));
        let inspection = store.inspect_manifest(&manifest_path).unwrap();
        let error = store.verify(inspection).unwrap_err();

        assert_eq!(error.code(), "embedding.artifact_checksum_mismatch");
    }

    #[test]
    fn materialization_rejects_bytes_that_drift_from_explicit_manifest() {
        let root = temp_dir("qgh-explicit-manifest-copy-drift");
        let source_root = root.join("source");
        let manifest_path = write_prepared_manifest_fixture(&source_root, b"graph-one");
        let verifier = PreparedModelStore::new(root.join("verifier"));
        let verified = verifier.load_manifest(&manifest_path).unwrap();
        fs::write(source_root.join("model.onnx"), b"graph-two").unwrap();
        let sources = runtime_artifact_sources_from_prepared(&verified);
        let options = FastembedProviderOptions {
            manifest_path: Some(manifest_path),
            ..FastembedProviderOptions::default()
        };
        let destination = PreparedModelStore::new(root.join("destination"));

        let error = destination
            .materialize(&options, verified.manifest.clone(), sources)
            .unwrap_err();

        assert_eq!(error.code(), "embedding.acquisition_artifact_mismatch");
    }

    #[test]
    fn verification_rejects_artifact_replaced_after_inspection() {
        let root = temp_dir("qgh-prepared-replaced-after-inspection");
        let snapshot_root = root.join("snapshot");
        let manifest_path = write_prepared_manifest_fixture(&snapshot_root, b"graph-one");
        let store = PreparedModelStore::new(root.join("store"));
        let inspection = store.inspect_manifest(&manifest_path).unwrap();
        fs::write(snapshot_root.join("model.onnx"), b"graph-two").unwrap();

        let error = store.verify(inspection).unwrap_err();

        assert_eq!(error.code(), "embedding.artifact_changed");
    }

    #[cfg(all(feature = "fastembed-provider", unix))]
    #[test]
    fn verified_runtime_payload_fails_closed_after_path_swaps() {
        use std::os::unix::fs::symlink;

        for (index, relative_path) in ["tokenizer.json", "model.onnx", "weights.bin"]
            .into_iter()
            .enumerate()
        {
            let root = temp_dir(&format!("qgh-verified-runtime-swap-{index}"));
            let snapshot_root = root.join("snapshot");
            let manifest_path = write_runtime_payload_fixture(&snapshot_root);
            let store = PreparedModelStore::new(root.join("unused"));
            let snapshot = store
                .verify(store.inspect_manifest(&manifest_path).unwrap())
                .unwrap();

            let artifact_path = snapshot_root.join(relative_path);
            fs::rename(
                &artifact_path,
                snapshot_root.join(format!("{index}.original")),
            )
            .unwrap();
            let outside = root.join(format!("outside-{index}"));
            fs::write(&outside, b"replacement-outside-bytes").unwrap();
            symlink(&outside, &artifact_path).unwrap();

            let error = match relative_path {
                "tokenizer.json" => match FastembedTokenizer::from_prepared_snapshot(&snapshot) {
                    Ok(_) => panic!("swapped tokenizer must fail closed before parsing"),
                    Err(error) => error,
                },
                _ => {
                    FastembedTokenizer::from_prepared_snapshot(&snapshot)
                        .expect("tokenizer-only use must not read model artifacts");
                    fastembed_user_defined_model(&snapshot)
                        .expect_err("swapped model artifact must fail closed before ORT")
                }
            };
            assert!(matches!(
                error.code(),
                "embedding.artifact_changed" | "embedding.artifact_checksum_mismatch"
            ));
            assert!(!error
                .details()
                .to_string()
                .contains("replacement-outside-bytes"));
        }
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn tokenizer_only_verification_rejects_tokenizer_checksum_mismatch_without_reading_model() {
        let root = temp_dir("qgh-tokenizer-only-checksum");
        let source_root = root.join("source");
        let manifest_path = write_runtime_payload_fixture(&source_root);
        let tokenizer_path = source_root.join("tokenizer.json");
        let tokenizer_size = fs::metadata(&tokenizer_path).unwrap().len() as usize;
        fs::write(&tokenizer_path, vec![b'x'; tokenizer_size]).unwrap();
        fs::remove_file(source_root.join("model.onnx")).unwrap();
        fs::remove_file(source_root.join("weights.bin")).unwrap();
        let options = FastembedProviderOptions {
            manifest_path: Some(manifest_path),
            ..FastembedProviderOptions::default()
        };
        let store = PreparedModelStore::new(root.join("store"));

        reset_tokenizer_only_artifact_bytes();
        let error = match store.acquire_tokenizer(&options) {
            Ok(_) => panic!("corrupt tokenizer must fail closed"),
            Err(error) => error,
        };
        let bytes = tokenizer_only_artifact_bytes();

        assert_eq!(error.code(), "embedding.artifact_checksum_mismatch");
        assert_eq!(bytes.get(&ArtifactRole::OnnxModel).copied().unwrap_or(0), 0);
        assert_eq!(
            bytes
                .get(&ArtifactRole::OnnxExternalData)
                .copied()
                .unwrap_or(0),
            0
        );
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn tokenizer_only_acquisition_reuses_prepared_alias_without_model_artifacts() {
        let root = temp_dir("qgh-tokenizer-only-prepared-alias");
        let source_root = root.join("source");
        let manifest_path = write_runtime_payload_fixture(&source_root);
        let options = FastembedProviderOptions {
            manifest_path: Some(manifest_path),
            ..FastembedProviderOptions::default()
        };
        let store = PreparedModelStore::new(root.join("store"));
        let snapshot = store.acquire(&options).unwrap();
        fs::remove_dir_all(&source_root).unwrap();
        fs::remove_file(snapshot.root.join("model.onnx")).unwrap();
        fs::remove_file(snapshot.root.join("weights.bin")).unwrap();

        reset_tokenizer_only_artifact_bytes();
        store.acquire_tokenizer(&options).unwrap();
        let bytes = tokenizer_only_artifact_bytes();

        assert!(bytes
            .get(&ArtifactRole::Tokenizer)
            .is_some_and(|bytes| *bytes > 0));
        assert_eq!(bytes.get(&ArtifactRole::OnnxModel).copied().unwrap_or(0), 0);
        assert_eq!(
            bytes
                .get(&ArtifactRole::OnnxExternalData)
                .copied()
                .unwrap_or(0),
            0
        );
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn tokenizer_only_direct_model_file_does_not_require_graph_metadata() {
        let root = temp_dir("qgh-tokenizer-only-direct-model-file");
        let manifest_path = write_runtime_payload_fixture(&root);
        fs::remove_file(&manifest_path).unwrap();
        fs::remove_file(root.join("model.onnx")).unwrap();
        fs::remove_file(root.join("weights.bin")).unwrap();
        fs::create_dir_all(root.join("onnx")).unwrap();
        let options = FastembedProviderOptions {
            model_path: Some(root.join("onnx/missing-model.onnx")),
            quantization: Some(QuantizationKind::None),
            ..FastembedProviderOptions::default()
        };
        let store = PreparedModelStore::new(root.join("store"));

        reset_tokenizer_only_artifact_bytes();
        store.acquire_tokenizer(&options).unwrap();
        let bytes = tokenizer_only_artifact_bytes();

        assert!(bytes
            .get(&ArtifactRole::Tokenizer)
            .is_some_and(|bytes| *bytes > 0));
        assert_eq!(bytes.get(&ArtifactRole::OnnxModel).copied().unwrap_or(0), 0);
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn tokenizer_only_rejects_oversized_declared_artifact_before_opening_it() {
        let root = temp_dir("qgh-tokenizer-only-size-limit");
        let manifest_path = write_runtime_payload_fixture(&root);
        let mut manifest = ModelManifestV1::from_json_slice(&fs::read(&manifest_path).unwrap())
            .expect("fixture manifest");
        let tokenizer = manifest
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.role == ArtifactRole::Tokenizer)
            .expect("tokenizer declaration");
        tokenizer.byte_size = MAX_TOKENIZER_ARTIFACT_BYTES + 1;
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        fs::remove_file(root.join("tokenizer.json")).unwrap();
        let options = FastembedProviderOptions {
            manifest_path: Some(manifest_path),
            ..FastembedProviderOptions::default()
        };
        let store = PreparedModelStore::new(root.join("store"));

        let error = match store.acquire_tokenizer(&options) {
            Ok(_) => panic!("oversized tokenizer artifact must fail before opening"),
            Err(error) => error,
        };

        assert_eq!(error.code(), "embedding.tokenizer_artifact_too_large");
    }

    #[test]
    fn tokenizer_size_validation_rejects_cumulative_overflow() {
        let error = validate_tokenizer_artifact_sizes([
            (ArtifactRole::Tokenizer, MAX_TOKENIZER_ARTIFACT_BYTES),
            (ArtifactRole::Config, MAX_TOKENIZER_ARTIFACT_BYTES),
            (ArtifactRole::TokenizerConfig, 1),
        ])
        .unwrap_err();

        assert_eq!(error.code(), "embedding.tokenizer_artifact_too_large");
    }

    #[cfg(all(feature = "fastembed-provider", unix))]
    #[test]
    fn hf_cache_artifact_canonicalization_rejects_escape_symlinks() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("qgh-hf-cache-confinement");
        let cache = root.join("cache");
        fs::create_dir_all(&cache).unwrap();
        let cache = fs::canonicalize(cache).unwrap();
        let inside = cache.join("inside.bin");
        fs::write(&inside, b"inside").unwrap();
        assert_eq!(
            confined_hf_cache_artifact(&cache, &inside, "inside.bin").unwrap(),
            inside
        );

        let outside = root.join("outside.bin");
        fs::write(&outside, b"outside").unwrap();
        let pointer = cache.join("pointer.bin");
        symlink(&outside, &pointer).unwrap();
        let error = confined_hf_cache_artifact(&cache, &pointer, "pointer.bin").unwrap_err();

        assert_eq!(error.code(), "embedding.hf_cache_invalid");
        assert!(!error
            .details()
            .to_string()
            .contains(&root.to_string_lossy()[..]));
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn tokenizer_and_full_acquisition_reuse_one_resolved_hf_pin() {
        let root = temp_dir("qgh-hf-acquisition-pin");
        let store = PreparedModelStore::new(root.join("prepared"));
        let options = FastembedProviderOptions {
            model: Some("hf:example/model@main".to_string()),
            ..FastembedProviderOptions::default()
        };
        let reference = HfModelReference {
            model_id: "example/model".to_string(),
            revision: "main".to_string(),
        };
        let resolves = std::cell::Cell::new(0_u32);
        let first = store
            .resolve_or_pin_hf_reference_with(&options, &reference, || {
                resolves.set(resolves.get() + 1);
                Ok("a".repeat(40))
            })
            .unwrap();
        let second = store
            .resolve_or_pin_hf_reference_with(&options, &reference, || {
                resolves.set(resolves.get() + 1);
                Ok("b".repeat(40))
            })
            .unwrap();

        assert_eq!(first.revision, "a".repeat(40));
        assert_eq!(second, first);
        assert_eq!(resolves.get(), 1);
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn late_materialization_cannot_retire_a_newer_hf_pin() {
        let root = temp_dir("qgh-hf-acquisition-pin-interleaving");
        let store = PreparedModelStore::new(root.join("prepared"));
        let options = FastembedProviderOptions {
            model: Some("hf:example/model@main".to_string()),
            ..FastembedProviderOptions::default()
        };
        let reference = HfModelReference {
            model_id: "example/model".to_string(),
            revision: "main".to_string(),
        };
        store
            .publish_acquisition_pin(&options, &reference, "a".repeat(40))
            .unwrap();
        let older = store
            .read_acquisition_pin_record_at(&store.acquisition_pin_path(&options))
            .unwrap()
            .unwrap();

        store.retire_acquisition_pin(&options, &older).unwrap();
        store
            .publish_acquisition_pin(&options, &reference, "b".repeat(40))
            .unwrap();
        store.retire_acquisition_pin(&options, &older).unwrap();

        let resolves = std::cell::Cell::new(0_u32);
        let full = store
            .resolve_or_pin_hf_reference_with(&options, &reference, || {
                resolves.set(resolves.get() + 1);
                Ok("c".repeat(40))
            })
            .unwrap();
        assert_eq!(full.revision, "b".repeat(40));
        assert_eq!(resolves.get(), 0);
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn verified_runtime_payload_rejects_cumulative_address_space_overflow() {
        assert_eq!(checked_runtime_payload_size([1, 2, 3]).unwrap(), 6);
        let error = checked_runtime_payload_size([u64::MAX, 1]).unwrap_err();
        assert_eq!(error.code(), "embedding.artifact_size_mismatch");
    }

    #[cfg(unix)]
    #[test]
    fn artifact_stamp_changes_after_same_size_file_replacement() {
        let root = temp_dir("qgh-prepared-stamp-replacement");
        let snapshot_root = root.join("snapshot");
        let manifest_path = write_prepared_manifest_fixture(&snapshot_root, b"graph-one");
        let store = PreparedModelStore::new(root.join("store"));
        let before = store.inspect_manifest(&manifest_path).unwrap();
        let replacement = snapshot_root.join("replacement.onnx");
        fs::write(&replacement, b"graph-two").unwrap();
        fs::rename(&replacement, snapshot_root.join("model.onnx")).unwrap();

        let after = store.inspect_manifest(&manifest_path).unwrap();

        assert_ne!(before.artifact_stamp(), after.artifact_stamp());
    }

    #[test]
    fn stream_copy_removes_partial_destination_after_read_failure() {
        struct FailingReader {
            emitted: bool,
        }

        impl Read for FailingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.emitted {
                    return Err(std::io::Error::other("fixture read failure"));
                }
                self.emitted = true;
                buffer[..4].copy_from_slice(b"part");
                Ok(4)
            }
        }

        let root = temp_dir("qgh-stream-copy-cleanup");
        let destination = root.join("artifact.bin");
        let mut reader = FailingReader { emitted: false };

        let error = stream_reader_to_new_file(&mut reader, &destination).unwrap_err();

        assert_eq!(error.code(), "embedding.io");
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn inspection_rejects_nested_symlink_and_alias_snapshot_escape() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("qgh-prepared-nested-symlink");
        let source_root = root.join("source");
        let manifest_path = write_prepared_manifest_fixture(&source_root, b"graph-one");
        let real = source_root.join("real");
        fs::create_dir_all(&real).unwrap();
        fs::rename(source_root.join("model.onnx"), real.join("model.onnx")).unwrap();
        symlink(&real, source_root.join("linked")).unwrap();
        let mut manifest =
            ModelManifestV1::from_json_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.role == ArtifactRole::OnnxModel)
            .unwrap()
            .relative_path = "linked/model.onnx".to_string();
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let store_root = root.join("store");
        let store = PreparedModelStore::new(store_root.clone());

        let error = store.inspect_manifest(&manifest_path).unwrap_err();
        assert_eq!(error.code(), "embedding.artifact_symlink_forbidden");

        fs::remove_file(source_root.join("linked")).unwrap();
        fs::rename(real.join("model.onnx"), source_root.join("model.onnx")).unwrap();
        fs::remove_dir(real).unwrap();
        manifest
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.role == ArtifactRole::OnnxModel)
            .unwrap()
            .relative_path = "model.onnx".to_string();
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let manifest_hash = manifest.hash();
        fs::create_dir_all(store_root.join("snapshots")).unwrap();
        fs::create_dir_all(store_root.join("requests")).unwrap();
        symlink(
            &source_root,
            store_root.join("snapshots").join(&manifest_hash),
        )
        .unwrap();
        let options = FastembedProviderOptions::default();
        fs::write(
            store.alias_path(&options),
            serde_json::to_vec_pretty(&PreparedModelAliasV1 {
                schema_version: PREPARED_MODEL_ALIAS_SCHEMA_VERSION.to_string(),
                manifest_hash: manifest_hash.clone(),
            })
            .unwrap(),
        )
        .unwrap();

        let error = store.inspect(&options).unwrap_err();
        assert_eq!(error.code(), "embedding.prepared_alias_invalid");

        fs::remove_file(store_root.join("snapshots").join(&manifest_hash)).unwrap();
        fs::remove_dir(store_root.join("snapshots")).unwrap();
        let external_snapshots = root.join("external-snapshots");
        fs::create_dir_all(&external_snapshots).unwrap();
        fs::rename(&source_root, external_snapshots.join(&manifest_hash)).unwrap();
        symlink(&external_snapshots, store_root.join("snapshots")).unwrap();

        let error = store.inspect(&options).unwrap_err();
        assert_eq!(error.code(), "embedding.prepared_alias_invalid");
    }

    #[test]
    fn manifest_runtime_applies_document_query_prefix_mrl_and_l2_normalization() {
        struct ContractEngine {
            calls: Mutex<Vec<Vec<String>>>,
        }

        impl EmbeddingEngine for ContractEngine {
            fn embed_texts(
                &self,
                texts: &[String],
            ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
                self.calls.lock().unwrap().push(texts.to_vec());
                Ok(texts.iter().map(|_| vec![3.0, 4.0, 12.0, 0.0]).collect())
            }
        }

        let provider = LocalEmbeddingProvider::with_contract(
            ContractEngine {
                calls: Mutex::new(Vec::new()),
            },
            EmbeddingRuntimeContract {
                query_prefix: Some("q: ".to_string()),
                document_prefix: Some("d: ".to_string()),
                normalization: NormalizationKind::L2,
                native_dimension: 4,
                output_dimension: 2,
            },
        )
        .unwrap();

        assert_eq!(provider.embed_query("needle").unwrap(), vec![0.6, 0.8]);
        assert_eq!(
            provider.embed_documents(&["haystack"]).unwrap(),
            vec![vec![0.6, 0.8]]
        );
        let calls = provider.engine.calls.lock().unwrap();
        assert_eq!(calls[0], vec!["q: needle"]);
        assert_eq!(calls[1], vec!["d: haystack"]);
    }

    #[test]
    fn custom_hf_without_revision_uses_default_branch_not_snowflake_revision() {
        let reference = parse_hf_model_reference("hf:Example/custom-model").unwrap();

        assert_eq!(reference.model_id, "Example/custom-model");
        assert_eq!(reference.revision, "main");
        assert_ne!(reference.revision, DEFAULT_HF_MODEL_REVISION);
    }

    #[test]
    fn builtin_preset_registry_is_closed_and_commit_pinned() {
        assert_eq!(BUILTIN_PRESET_IDS.len(), 4);
        for preset_id in BUILTIN_PRESET_IDS {
            let preset = builtin_preset(preset_id).unwrap();
            assert_eq!(preset.id, preset_id);
            assert!(is_commit_sha(preset.revision));
            assert_ne!(preset.quantization, QuantizationKind::Dynamic);
        }
        assert!(builtin_preset("jina-v5").is_none());
        assert_eq!(
            builtin_preset("granite-97m-multilingual-r2-int8-static")
                .unwrap()
                .quantization,
            QuantizationKind::Static
        );
    }

    #[test]
    fn acquire_local_legacy_creates_snapshot_that_loads_after_source_is_removed() {
        let source = temp_dir("qgh-legacy-local-source");
        fs::write(source.join("model.onnx"), b"onnx").unwrap();
        fs::write(source.join("tokenizer.json"), b"{}").unwrap();
        fs::write(
            source.join("config.json"),
            br#"{"hidden_size":4,"max_position_embeddings":32}"#,
        )
        .unwrap();
        fs::write(source.join("special_tokens_map.json"), b"{}").unwrap();
        fs::write(
            source.join("tokenizer_config.json"),
            br#"{"model_max_length":32}"#,
        )
        .unwrap();
        fs::write(
            source.join("modules.json"),
            br#"[
                {"type":"sentence_transformers.models.Normalize"},
                {"prompts":{"query":"","document":""}}
            ]"#,
        )
        .unwrap();

        let store = PreparedModelStore::new(temp_dir("qgh-prepared-model-store"));
        let options = FastembedProviderOptions {
            manifest_path: None,
            model: None,
            model_path: Some(source.clone()),
            file: Some("model.onnx".to_string()),
            pooling: Some(PoolingKind::Cls),
            query_prefix: Some(String::new()),
            quantization: Some(QuantizationKind::None),
            token_source_env: None,
            cache_dir: None,
        };

        let acquired = store.acquire(&options).unwrap();
        assert_ne!(acquired.root, fs::canonicalize(&source).unwrap());
        assert_eq!(acquired.manifest.native_dimension, 4);
        fs::remove_dir_all(source).unwrap();

        let loaded = store.load(&options).unwrap();
        assert_eq!(loaded.manifest.hash(), acquired.manifest.hash());
        assert!(loaded
            .path_for_role(ArtifactRole::OnnxModel)
            .unwrap()
            .is_file());
    }

    #[test]
    fn acquire_explicit_manifest_copies_snapshot_and_load_ignores_source_mutation() {
        let source = temp_dir("qgh-explicit-manifest-source");
        let declarations = [
            (
                ArtifactRole::OnnxModel,
                "model.onnx",
                b"original-graph".as_slice(),
            ),
            (ArtifactRole::Tokenizer, "tokenizer.json", b"{}".as_slice()),
            (ArtifactRole::Config, "config.json", b"{}".as_slice()),
            (
                ArtifactRole::SpecialTokensMap,
                "special_tokens_map.json",
                b"{}".as_slice(),
            ),
            (
                ArtifactRole::TokenizerConfig,
                "tokenizer_config.json",
                b"{}".as_slice(),
            ),
        ];
        let artifacts = declarations
            .into_iter()
            .map(|(role, relative_path, bytes)| {
                fs::write(source.join(relative_path), bytes).unwrap();
                fixture_artifact(role, relative_path, bytes, None)
            })
            .collect();
        let manifest_path = source.join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&fixture_manifest(artifacts)).unwrap(),
        )
        .unwrap();
        let options = FastembedProviderOptions {
            manifest_path: Some(manifest_path.clone()),
            model: None,
            model_path: None,
            file: None,
            pooling: None,
            query_prefix: None,
            quantization: None,
            token_source_env: None,
            cache_dir: None,
        };
        let store = PreparedModelStore::new(temp_dir("qgh-explicit-manifest-store"));

        let acquired = store.acquire(&options).unwrap();
        assert_ne!(acquired.root, fs::canonicalize(&source).unwrap());
        fs::write(source.join("model.onnx"), b"mutated-graph").unwrap();
        fs::remove_file(manifest_path).unwrap();

        let loaded = store.load(&options).unwrap();
        assert_eq!(loaded.manifest.hash(), acquired.manifest.hash());
        assert_eq!(
            fs::read(loaded.path_for_role(ArtifactRole::OnnxModel).unwrap()).unwrap(),
            b"original-graph"
        );
    }

    #[test]
    fn legacy_model_path_rejects_declared_dynamic_with_plain_model_filename() {
        let source = temp_dir("qgh-legacy-dynamic-source");
        fs::write(source.join("model.onnx"), b"dynamic-graph").unwrap();
        fs::write(source.join("tokenizer.json"), b"{}").unwrap();
        fs::write(
            source.join("config.json"),
            br#"{"hidden_size":4,"max_position_embeddings":32}"#,
        )
        .unwrap();
        fs::write(source.join("special_tokens_map.json"), b"{}").unwrap();
        fs::write(
            source.join("tokenizer_config.json"),
            br#"{"model_max_length":32}"#,
        )
        .unwrap();
        fs::write(
            source.join("modules.json"),
            br#"[
                {"type":"sentence_transformers.models.Normalize"},
                {"prompts":{"query":"","document":""}}
            ]"#,
        )
        .unwrap();
        let options = FastembedProviderOptions {
            manifest_path: None,
            model: None,
            model_path: Some(source),
            file: Some("model.onnx".to_string()),
            pooling: Some(PoolingKind::Cls),
            query_prefix: Some(String::new()),
            quantization: Some(QuantizationKind::Dynamic),
            token_source_env: None,
            cache_dir: None,
        };

        let store = PreparedModelStore::new(temp_dir("qgh-legacy-dynamic-store"));
        let error = store.acquire(&options).unwrap_err();

        assert_eq!(error.code(), "embedding.dynamic_quantization_unsupported");

        let mut undeclared = options;
        undeclared.quantization = None;
        let error = store.acquire(&undeclared).unwrap_err();
        assert_eq!(error.code(), "embedding.legacy_quantization_required");
    }

    #[test]
    fn batch_comparability_rejects_position_dependent_vectors() {
        struct PositionDependentEngine;

        impl EmbeddingEngine for PositionDependentEngine {
            fn embed_texts(
                &self,
                texts: &[String],
            ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
                Ok(texts
                    .iter()
                    .enumerate()
                    .map(|(index, _)| vec![1.0, index as f32 + 1.0])
                    .collect())
            }
        }

        let provider = LocalEmbeddingProvider::new(PositionDependentEngine, "");
        let error = validate_batch_comparability(&provider, "same text").unwrap_err();

        assert_eq!(error.code(), "embedding.batch_incomparable");
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn prepared_fastembed_model_includes_declared_external_initializer() {
        let root = temp_dir("qgh-external-initializer");
        let manifest_path = write_runtime_payload_fixture(&root);
        let snapshot = PreparedModelStore::new(root.join("unused"))
            .load_manifest(&manifest_path)
            .unwrap();

        let model = fastembed_user_defined_model(&snapshot).unwrap();

        assert_eq!(model.onnx_file, b"original-model");
        assert!(tokenizers::Tokenizer::from_bytes(&model.tokenizer_files.tokenizer_file).is_ok());
        assert_eq!(model.tokenizer_files.config_file, b"{}");
        assert_eq!(model.tokenizer_files.special_tokens_map_file, b"{}");
        assert_eq!(model.tokenizer_files.tokenizer_config_file, b"{}");
        assert_eq!(model.external_initializers.len(), 1);
        assert_eq!(model.external_initializers[0].file_name, "weights.bin");
        assert_eq!(model.external_initializers[0].buffer, b"original-weights");
    }

    fn fixture_manifest(artifacts: Vec<ModelArtifactV1>) -> ModelManifestV1 {
        ModelManifestV1 {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION.to_string(),
            preset_id: None,
            provider: ModelProviderKind::Fastembed,
            model_source: ModelSourceV1::Local {
                declared_id: "fixture".to_string(),
            },
            artifacts,
            tokenizer: TokenizerKind::HfTokenizerJson,
            query_prefix: Some(String::new()),
            document_prefix: Some(String::new()),
            pooling: PoolingKind::Cls,
            normalization: NormalizationKind::L2,
            native_dimension: 4,
            output_dimension: 4,
            max_length: 32,
            quantization: QuantizationKind::None,
            context_template_version: METADATA_CONTEXT_TEMPLATE_VERSION.to_string(),
        }
    }

    fn write_prepared_manifest_fixture(root: &Path, graph: &[u8]) -> PathBuf {
        fs::create_dir_all(root).unwrap();
        let declarations = [
            (ArtifactRole::OnnxModel, "model.onnx", graph),
            (ArtifactRole::Tokenizer, "tokenizer.json", b"{}".as_slice()),
            (ArtifactRole::Config, "config.json", b"{}".as_slice()),
            (
                ArtifactRole::SpecialTokensMap,
                "special_tokens_map.json",
                b"{}".as_slice(),
            ),
            (
                ArtifactRole::TokenizerConfig,
                "tokenizer_config.json",
                b"{}".as_slice(),
            ),
        ];
        let artifacts = declarations
            .iter()
            .map(|(role, path, bytes)| {
                fs::write(root.join(path), bytes).unwrap();
                fixture_artifact(*role, path, bytes, None)
            })
            .collect();
        let manifest_path = root.join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&fixture_manifest(artifacts)).unwrap(),
        )
        .unwrap();
        manifest_path
    }

    #[cfg(feature = "fastembed-provider")]
    fn write_runtime_payload_fixture(root: &Path) -> PathBuf {
        use tokenizers::models::wordlevel::WordLevel;

        fs::create_dir_all(root).unwrap();
        let tokenizer_bytes = tokenizers::Tokenizer::new(WordLevel::default())
            .to_string(false)
            .unwrap()
            .into_bytes();
        let declarations = [
            (
                ArtifactRole::OnnxModel,
                "model.onnx",
                b"original-model".as_slice(),
                None,
            ),
            (
                ArtifactRole::OnnxExternalData,
                "weights.bin",
                b"original-weights".as_slice(),
                Some("weights.bin"),
            ),
            (
                ArtifactRole::Tokenizer,
                "tokenizer.json",
                tokenizer_bytes.as_slice(),
                None,
            ),
            (ArtifactRole::Config, "config.json", b"{}".as_slice(), None),
            (
                ArtifactRole::SpecialTokensMap,
                "special_tokens_map.json",
                b"{}".as_slice(),
                None,
            ),
            (
                ArtifactRole::TokenizerConfig,
                "tokenizer_config.json",
                b"{}".as_slice(),
                None,
            ),
        ];
        let artifacts = declarations
            .iter()
            .map(|(role, relative_path, bytes, initializer_name)| {
                fs::write(root.join(relative_path), bytes).unwrap();
                fixture_artifact(*role, relative_path, bytes, *initializer_name)
            })
            .collect();
        let manifest_path = root.join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&fixture_manifest(artifacts)).unwrap(),
        )
        .unwrap();
        manifest_path
    }

    fn fixture_artifact(
        role: ArtifactRole,
        relative_path: &str,
        bytes: &[u8],
        external_initializer_name: Option<&str>,
    ) -> ModelArtifactV1 {
        ModelArtifactV1 {
            role,
            relative_path: relative_path.to_string(),
            sha256: hex_digest(&Sha256::digest(bytes)),
            byte_size: bytes.len() as u64,
            external_initializer_name: external_initializer_name.map(ToString::to_string),
        }
    }

    #[test]
    fn sentence_transformers_metadata_detects_pooling_prefix_and_file() {
        let mut repo = FixtureRepo::new()
            .file(
                MODULES_FILE,
                r#"[{
                    "idx": 1,
                    "name": "1",
                    "path": "1_Pooling",
                    "type": "sentence_transformers.models.Pooling",
                    "prompts": { "query": "query: " }
                }]"#,
            )
            .file(
                DEFAULT_POOLING_CONFIG_FILE,
                r#"{"pooling_mode_cls_token": false, "pooling_mode_mean_tokens": true}"#,
            )
            .file(DEFAULT_HF_MODEL_FILE, "onnx")
            .tokenizer_files();

        let snapshot = resolve_hf_model_snapshot(
            "Example/model",
            ManualModelBehavior {
                file: Some(DEFAULT_HF_MODEL_FILE.to_string()),
                ..ManualModelBehavior::default()
            },
            &mut repo,
        )
        .unwrap();

        assert_eq!(snapshot.model_file, DEFAULT_HF_MODEL_FILE);
        assert_eq!(snapshot.pooling, PoolingKind::Mean);
        assert_eq!(snapshot.query_prefix, DEFAULT_QUERY_PREFIX);
    }

    #[test]
    fn metadata_detection_failure_is_structured_and_requires_manual_keys() {
        let mut repo = FixtureRepo::new()
            .file(
                MODULES_FILE,
                r#"[{"path":"1_Pooling","type":"sentence_transformers.models.Pooling"}]"#,
            )
            .file(DEFAULT_POOLING_CONFIG_FILE, r#"{}"#)
            .tokenizer_files();

        let error =
            resolve_hf_model_snapshot("Example/model", ManualModelBehavior::default(), &mut repo)
                .unwrap_err();

        assert_eq!(error.code(), "embedding.model_metadata_required");
        assert_eq!(
            error.details()["required_manual_keys"],
            json!(["pooling", "query_prefix", "file"])
        );
        assert_eq!(
            error.details()["missing_manual_keys"],
            json!(["file", "pooling", "query_prefix"])
        );
    }

    #[test]
    fn manual_hf_behavior_does_not_require_sentence_transformers_metadata() {
        let mut repo = FixtureRepo::new()
            .file(DEFAULT_HF_MODEL_FILE, "onnx")
            .tokenizer_files();

        let snapshot = resolve_hf_model_snapshot(
            "Example/model",
            ManualModelBehavior {
                file: Some(DEFAULT_HF_MODEL_FILE.to_string()),
                pooling: Some(PoolingKind::Cls),
                query_prefix: Some(DEFAULT_QUERY_PREFIX.to_string()),
            },
            &mut repo,
        )
        .unwrap();

        assert_eq!(snapshot.model_file, DEFAULT_HF_MODEL_FILE);
        assert_eq!(snapshot.pooling, PoolingKind::Cls);
        assert_eq!(snapshot.query_prefix, DEFAULT_QUERY_PREFIX);

        let fetched = repo.fetched.borrow().clone();
        assert!(!fetched.contains(&MODULES_FILE.to_string()));
        assert!(!fetched.contains(&DEFAULT_POOLING_CONFIG_FILE.to_string()));
    }

    #[test]
    fn default_model_does_not_silently_fill_missing_pooling_or_query_prefix() {
        let mut repo = FixtureRepo::new()
            .file(DEFAULT_HF_MODEL_FILE, "onnx")
            .tokenizer_files();

        let error = resolve_hf_model_snapshot(
            DEFAULT_HF_MODEL_ID,
            ManualModelBehavior::default(),
            &mut repo,
        )
        .unwrap_err();

        assert_eq!(error.code(), "embedding.model_metadata_required");
        assert_eq!(
            error.details()["missing_manual_keys"],
            json!(["pooling", "query_prefix"])
        );
        assert_eq!(
            error.details()["required_manual_keys"],
            json!(["pooling", "query_prefix", "file"])
        );
    }

    #[test]
    fn fresh_hf_cache_downloads_all_files_needed_by_fastembed_engine() {
        let mut repo = FixtureRepo::new()
            .file(
                MODULES_FILE,
                r#"[{
                    "path":"1_Pooling",
                    "type":"sentence_transformers.models.Pooling",
                    "query_prefix":"query: "
                }]"#,
            )
            .file(
                DEFAULT_POOLING_CONFIG_FILE,
                r#"{"pooling_mode_cls_token": true, "pooling_mode_mean_tokens": false}"#,
            )
            .file(DEFAULT_HF_MODEL_FILE, "onnx")
            .tokenizer_files();

        let snapshot = resolve_hf_model_snapshot(
            DEFAULT_HF_MODEL_ID,
            ManualModelBehavior::default(),
            &mut repo,
        )
        .unwrap();

        let fetched = repo.fetched.borrow().clone();
        for required in required_runtime_files(DEFAULT_HF_MODEL_FILE) {
            assert!(
                fetched.contains(&required.to_string()),
                "missing required fetch {required}; fetched {fetched:#?}"
            );
            assert!(
                snapshot.path_for(required).is_some(),
                "missing snapshot path for {required}"
            );
        }
    }

    #[test]
    fn model_path_snapshot_uses_only_local_files() {
        let root = temp_dir("qgh-model-path");
        fs::create_dir_all(root.join("onnx")).unwrap();
        fs::write(root.join(DEFAULT_HF_MODEL_FILE), b"onnx").unwrap();
        fs::write(
            root.join(MODULES_FILE),
            r#"[{"path":"1_Pooling","type":"sentence_transformers.models.Pooling","query_prefix":"query: "}]"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("1_Pooling")).unwrap();
        fs::write(
            root.join(DEFAULT_POOLING_CONFIG_FILE),
            r#"{"pooling_mode_cls_token": true, "pooling_mode_mean_tokens": false}"#,
        )
        .unwrap();
        for file in TOKENIZER_FILES {
            fs::write(root.join(file), b"{}").unwrap();
        }

        let snapshot = resolve_model_path_snapshot(&root, ManualModelBehavior::default()).unwrap();

        assert_eq!(snapshot.model_id, None);
        assert_eq!(snapshot.model_revision, LOCAL_MODEL_REVISION);
        assert_eq!(snapshot.model_file, DEFAULT_HF_MODEL_FILE);
        assert_eq!(snapshot.pooling, PoolingKind::Cls);
        assert_eq!(snapshot.query_prefix, DEFAULT_QUERY_PREFIX);
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    fn fastembed_engine_reports_structured_error_for_invalid_local_snapshot() {
        let root = temp_dir("qgh-fastembed-invalid-snapshot");
        fs::create_dir_all(root.join("onnx")).unwrap();
        fs::write(root.join(DEFAULT_HF_MODEL_FILE), b"not an onnx model").unwrap();
        for file in TOKENIZER_FILES {
            fs::write(root.join(file), b"{}").unwrap();
        }
        let snapshot = ResolvedModelSnapshot {
            model_id: Some(DEFAULT_HF_MODEL_ID.to_string()),
            model_revision: "fixture-sha".to_string(),
            model_file: DEFAULT_HF_MODEL_FILE.to_string(),
            query_prefix: DEFAULT_QUERY_PREFIX.to_string(),
            pooling: PoolingKind::Cls,
            paths: required_runtime_files(DEFAULT_HF_MODEL_FILE)
                .into_iter()
                .map(|file| (file.to_string(), root.join(file)))
                .collect(),
        };

        let error = match FastembedEngine::from_snapshot(&snapshot) {
            Ok(_) => panic!("invalid snapshot unexpectedly initialized"),
            Err(error) => error,
        };

        assert_eq!(error.code(), "embedding.fastembed_init_failed");
    }

    #[cfg(feature = "fastembed-provider")]
    #[test]
    #[ignore = "downloads the default Hugging Face model and runs local ONNX inference"]
    fn ignored_default_fastembed_model_embeds_query_and_documents() {
        let provider =
            LocalEmbeddingProvider::<FastembedEngine>::from_options(FastembedProviderOptions {
                model: Some(format!("hf:{DEFAULT_HF_MODEL_ID}")),
                ..FastembedProviderOptions::default()
            })
            .unwrap();

        let query = provider.embed_query("rate limit").unwrap();
        let documents = provider
            .embed_documents(&["secondary rate limit backoff"])
            .unwrap();

        assert!(!query.is_empty());
        assert_eq!(documents.len(), 1);
        assert_eq!(provider.dimension(), Some(query.len()));
    }

    struct FixtureRepo {
        root: PathBuf,
        files: BTreeMap<String, String>,
        fetched: RefCell<Vec<String>>,
    }

    impl FixtureRepo {
        fn new() -> Self {
            Self {
                root: temp_dir("qgh-hf-fixture"),
                files: BTreeMap::new(),
                fetched: RefCell::new(Vec::new()),
            }
        }

        fn file(mut self, path: &str, contents: &str) -> Self {
            self.files.insert(path.to_string(), contents.to_string());
            self
        }

        fn tokenizer_files(mut self) -> Self {
            for file in TOKENIZER_FILES {
                self.files
                    .entry(file.to_string())
                    .or_insert_with(|| match file {
                        "tokenizer_config.json" => {
                            r#"{"model_max_length":8192,"pad_token":"[PAD]"}"#.to_string()
                        }
                        "config.json" => r#"{"pad_token_id":0}"#.to_string(),
                        _ => "{}".to_string(),
                    });
            }
            self
        }
    }

    impl ModelRepository for FixtureRepo {
        fn get(&mut self, file: &str) -> Result<PathBuf, EmbeddingProviderError> {
            self.fetched.borrow_mut().push(file.to_string());
            let Some(contents) = self.files.get(file) else {
                return Err(EmbeddingProviderError::structured(
                    "embedding.hf_download_failed",
                    "fixture file missing",
                )
                .with_details(json!({ "file": file })));
            };
            let path = self.root.join(file);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, contents).unwrap();
            Ok(path)
        }

        fn list_files(&mut self) -> Result<Vec<String>, EmbeddingProviderError> {
            Ok(self.files.keys().cloned().collect())
        }

        fn revision(&mut self) -> Result<Option<String>, EmbeddingProviderError> {
            Ok(Some("fixture-sha".to_string()))
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let count = TEMP_DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("{prefix}-{}-{nanos}-{count}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
