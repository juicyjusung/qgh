use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub type EmbeddingVector = Vec<f32>;

pub const DEFAULT_HF_MODEL_ID: &str = "Snowflake/snowflake-arctic-embed-l-v2.0";
pub const DEFAULT_HF_MODEL_REVISION: &str = "main";
pub const DEFAULT_HF_MODEL_FILE: &str = "onnx/model_quantized.onnx";
pub const DEFAULT_QUERY_PREFIX: &str = "query: ";
pub const HUGGINGFACE_ENDPOINT: &str = "https://huggingface.co";
pub const EMBEDDING_FINGERPRINT_SCHEMA_VERSION: &str = "qgh.embedding_fingerprint.v1";
pub const CHUNKER_VERSION: &str = "qgh.chunker.v1";
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenSpan {
    pub start: usize,
    pub end: usize,
}

pub trait EmbeddingProvider {
    fn embed_documents(
        &self,
        texts: &[&str],
    ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError>;
    fn embed_query(&self, text: &str) -> Result<EmbeddingVector, EmbeddingProviderError>;
}

pub trait EmbeddingEngine {
    fn embed_texts(&self, texts: &[String])
        -> Result<Vec<EmbeddingVector>, EmbeddingProviderError>;
}

pub trait EmbeddingTokenizer {
    fn tokenize(&self, text: &str) -> Result<Vec<TokenSpan>, EmbeddingProviderError>;

    fn count_tokens(&self, text: &str) -> Result<usize, EmbeddingProviderError> {
        Ok(self.tokenize(text)?.len())
    }
}

pub struct LocalEmbeddingProvider<E> {
    engine: E,
    query_prefix: String,
    dimension: Mutex<Option<usize>>,
}

impl<E> LocalEmbeddingProvider<E> {
    pub fn new(engine: E, query_prefix: impl Into<String>) -> Self {
        Self {
            engine,
            query_prefix: query_prefix.into(),
            dimension: Mutex::new(None),
        }
    }

    pub fn dimension(&self) -> Option<usize> {
        *self.dimension.lock().expect("dimension mutex poisoned")
    }

    pub fn query_prefix(&self) -> &str {
        &self.query_prefix
    }
}

impl<E: EmbeddingEngine> EmbeddingProvider for LocalEmbeddingProvider<E> {
    fn embed_documents(
        &self,
        texts: &[&str],
    ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
        let prepared = texts
            .iter()
            .map(|text| (*text).to_string())
            .collect::<Vec<_>>();
        let vectors = self.engine.embed_texts(&prepared)?;
        self.record_dimension(&vectors)?;
        Ok(vectors)
    }

    fn embed_query(&self, text: &str) -> Result<EmbeddingVector, EmbeddingProviderError> {
        let prepared = vec![format!("{}{}", self.query_prefix, text)];
        let mut vectors = self.engine.embed_texts(&prepared)?;
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
    pub model: Option<String>,
    pub model_path: Option<PathBuf>,
    pub file: Option<String>,
    pub pooling: Option<PoolingKind>,
    pub query_prefix: Option<String>,
    pub token_source_env: Option<String>,
    pub cache_dir: Option<PathBuf>,
}

impl Default for FastembedProviderOptions {
    fn default() -> Self {
        Self {
            model: Some(format!("hf:{DEFAULT_HF_MODEL_ID}")),
            model_path: None,
            file: None,
            pooling: None,
            query_prefix: None,
            token_source_env: None,
            cache_dir: None,
        }
    }
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
    let (model_id, revision) = reference
        .rsplit_once('@')
        .unwrap_or((reference, DEFAULT_HF_MODEL_REVISION));
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
impl FastembedEngine {
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
        .map_err(|error| {
            EmbeddingProviderError::structured(
                "embedding.fastembed_init_failed",
                "Failed to initialize fastembed local model.",
            )
            .with_details(json!({ "error": error.to_string() }))
        })?;
        Ok(Self {
            model: Mutex::new(model),
        })
    }
}

#[cfg(feature = "fastembed-provider")]
impl EmbeddingEngine for FastembedEngine {
    fn embed_texts(
        &self,
        texts: &[String],
    ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
        self.model
            .lock()
            .expect("fastembed mutex poisoned")
            .embed(texts, None)
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
    pub fn from_snapshot(snapshot: &ResolvedModelSnapshot) -> Result<Self, EmbeddingProviderError> {
        let tokenizer_path = required_path(snapshot, "tokenizer.json")?;
        let tokenizer = tokenizers::Tokenizer::from_file(tokenizer_path).map_err(|error| {
            EmbeddingProviderError::structured(
                "embedding.tokenizer_init_failed",
                "Failed to initialize embedding tokenizer.",
            )
            .with_details(json!({
                "file": tokenizer_path.display().to_string(),
                "error": error.to_string()
            }))
        })?;
        Ok(Self { tokenizer })
    }

    pub fn from_options(options: FastembedProviderOptions) -> Result<Self, EmbeddingProviderError> {
        let snapshot = resolve_fastembed_snapshot(options)?;
        Self::from_snapshot(&snapshot)
    }
}

#[cfg(feature = "fastembed-provider")]
impl EmbeddingTokenizer for FastembedTokenizer {
    fn tokenize(&self, text: &str) -> Result<Vec<TokenSpan>, EmbeddingProviderError> {
        let encoding = self.tokenizer.encode(text, false).map_err(|error| {
            EmbeddingProviderError::structured(
                "embedding.tokenizer_failed",
                "Embedding tokenizer failed to tokenize source text.",
            )
            .with_details(json!({ "error": error.to_string() }))
        })?;
        Ok(encoding
            .get_offsets()
            .iter()
            .filter_map(|(start, end)| {
                (start < end).then_some(TokenSpan {
                    start: *start,
                    end: *end,
                })
            })
            .collect())
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
    let mut repo = HfHubModelRepository::new(
        &reference.model_id,
        &reference.revision,
        cache_dir,
        options.token_source_env,
    )?;
    resolve_hf_model_snapshot(
        &reference.model_id,
        ManualModelBehavior {
            file: options.file,
            pooling: options.pooling,
            query_prefix: options.query_prefix,
        },
        &mut repo,
    )
}

#[cfg(feature = "fastembed-provider")]
struct HfHubModelRepository {
    repo: hf_hub::api::sync::ApiRepo,
    model_revision: String,
    info: Option<hf_hub::api::RepoInfo>,
}

#[cfg(feature = "fastembed-provider")]
impl HfHubModelRepository {
    fn new(
        model_id: &str,
        model_revision: &str,
        cache_dir: PathBuf,
        token_source_env: Option<String>,
    ) -> Result<Self, EmbeddingProviderError> {
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
            model_revision: model_revision.to_string(),
            info: None,
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
        self.repo.get(file).map_err(|error| {
            EmbeddingProviderError::structured(
                "embedding.hf_download_failed",
                "Failed to download required Hugging Face model file.",
            )
            .with_details(json!({
                "host": HUGGINGFACE_ENDPOINT,
                "file": file,
                "error": error.to_string()
            }))
        })
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
        Ok(Some(self.model_revision.clone()))
    }
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
        "onnx/model.onnx",
        "onnx/model.onnx_data",
        "model_quantized.onnx",
        "model.onnx",
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
    for preferred in [
        DEFAULT_HF_MODEL_FILE,
        "onnx/model_quantized.onnx",
        "onnx/model.onnx",
    ] {
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
    fn from(value: std::io::Error) -> Self {
        EmbeddingProviderError::structured("embedding.io", "Embedding provider I/O failed.")
            .with_details(json!({ "error": value.to_string() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[derive(Default)]
    struct RecordingEngine {
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl EmbeddingEngine for RecordingEngine {
        fn embed_texts(
            &self,
            texts: &[String],
        ) -> Result<Vec<EmbeddingVector>, EmbeddingProviderError> {
            self.calls.borrow_mut().push(texts.to_vec());
            Ok(texts.iter().map(|_| vec![1.0, 2.0, 3.0]).collect())
        }
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

        let calls = provider.engine.calls.borrow();
        assert_eq!(calls[0], vec!["query: rate limit"]);
        assert_eq!(calls[1], vec!["query: already document", "plain"]);
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
