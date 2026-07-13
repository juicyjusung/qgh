use crate::error::QghError;
use crate::paths::{ensure_private_dir, qgh_cache_dir, set_private_dir, set_private_file};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const QWEN_EMBEDDING_PRESET_ID: &str = "qwen3-embedding-0.6b";
pub const QWEN_RERANKER_PRESET_ID: &str = "qwen3-reranker-0.6b";
pub const QWEN_EMBEDDING_MODEL_ID: &str = "Qwen/Qwen3-Embedding-0.6B";
pub const QWEN_EMBEDDING_REVISION: &str = "97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3";
pub const QWEN_RERANKER_MODEL_ID: &str = "Qwen/Qwen3-Reranker-0.6B";
pub const QWEN_RERANKER_REVISION: &str = "e61197ed45024b0ed8a2d74b80b4d909f1255473";
pub const QWEN_EMBEDDING_QUERY_PREFIX: &str = "Instruct: Given a GitHub issue search query, retrieve relevant GitHub issue or comment passages that satisfy the information need\nQuery:";

const MODEL_MANIFEST_SCHEMA_VERSION: &str = "qgh.local_model_manifest.v1";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const COPY_BUFFER_BYTES: usize = 1024 * 1024;
static VERIFIED_ARTIFACTS: OnceLock<Mutex<HashMap<PathBuf, (String, FileIdentity)>>> =
    OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPurpose {
    Embedding,
    Reranker,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelArtifactSpec {
    pub relative_path: String,
    pub sha256: String,
    pub byte_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QwenModelSpec {
    pub preset_id: String,
    pub purpose: ModelPurpose,
    pub model_id: String,
    pub resolved_revision: String,
    pub artifacts: Vec<ModelArtifactSpec>,
}

pub fn qwen_model_spec(preset_id: &str) -> Option<QwenModelSpec> {
    match preset_id {
        QWEN_EMBEDDING_PRESET_ID => Some(QwenModelSpec {
            preset_id: QWEN_EMBEDDING_PRESET_ID.to_string(),
            purpose: ModelPurpose::Embedding,
            model_id: QWEN_EMBEDDING_MODEL_ID.to_string(),
            resolved_revision: QWEN_EMBEDDING_REVISION.to_string(),
            artifacts: vec![
                artifact(
                    "config.json",
                    "b5bf1f51fc45be473a54718cef92448d90a1be001bf9b9a44b8c7f10a19feaa9",
                    727,
                ),
                artifact(
                    "model.safetensors",
                    "0437e45c94563b09e13cb7a64478fc406947a93cb34a7e05870fc8dcd48e23fd",
                    1_191_586_416,
                ),
                artifact(
                    "tokenizer.json",
                    "def76fb086971c7867b829c23a26261e38d9d74e02139253b38aeb9df8b4b50a",
                    11_423_705,
                ),
            ],
        }),
        QWEN_RERANKER_PRESET_ID => Some(QwenModelSpec {
            preset_id: QWEN_RERANKER_PRESET_ID.to_string(),
            purpose: ModelPurpose::Reranker,
            model_id: QWEN_RERANKER_MODEL_ID.to_string(),
            resolved_revision: QWEN_RERANKER_REVISION.to_string(),
            artifacts: vec![
                artifact(
                    "config.json",
                    "d479c427a9ca5295218063d4f9aca4f297ab4ac27487cca7af42c84643d51ef0",
                    727,
                ),
                artifact(
                    "model.safetensors",
                    "27cd75a405b9c1b46b59abfd88aaa209e6fed2a1972cde9b70e7659537c5e65b",
                    1_191_588_280,
                ),
                artifact(
                    "tokenizer.json",
                    "aeb13307a71acd8fe81861d94ad54ab689df773318809eed3cbe794b4492dae4",
                    11_422_654,
                ),
                artifact(
                    "1_LogitScore/config.json",
                    "73e3156450564d8a98b7e47bcf5aace0f29600828b51937da545571e84db3ff3",
                    57,
                ),
            ],
        }),
        _ => None,
    }
}

/// Returns the pinned manifest identity without touching the installed model
/// store. Status and other read-only contract views use this to compare stored
/// generations without hashing model payloads.
pub fn qwen_model_manifest_hash(spec: &QwenModelSpec) -> String {
    debug_assert!(validate_spec(spec).is_ok());
    LocalModelManifestV1::for_spec(spec).hash()
}

fn artifact(relative_path: &str, sha256: &str, byte_size: u64) -> ModelArtifactSpec {
    ModelArtifactSpec {
        relative_path: relative_path.to_string(),
        sha256: sha256.to_string(),
        byte_size,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LocalModelManifestV1 {
    schema_version: String,
    preset_id: String,
    purpose: ModelPurpose,
    model_id: String,
    resolved_revision: String,
    artifacts: Vec<ModelArtifactSpec>,
}

impl LocalModelManifestV1 {
    fn for_spec(spec: &QwenModelSpec) -> Self {
        Self {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION.to_string(),
            preset_id: spec.preset_id.clone(),
            purpose: spec.purpose,
            model_id: spec.model_id.clone(),
            resolved_revision: spec.resolved_revision.clone(),
            artifacts: spec.artifacts.clone(),
        }
    }

    fn hash(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("local model manifest serializes");
        hex_digest(&Sha256::digest(bytes))
    }
}

#[derive(Debug, Clone)]
pub struct PreparedQwenModelSnapshot {
    #[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
    pub root: PathBuf,
    pub manifest_hash: String,
    #[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
    paths: BTreeMap<String, PathBuf>,
    #[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
    identities: BTreeMap<String, FileIdentity>,
}

impl PreparedQwenModelSnapshot {
    #[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
    pub fn artifact_path(&self, relative_path: &str) -> Result<&Path, QghError> {
        self.paths
            .get(relative_path)
            .map(PathBuf::as_path)
            .ok_or_else(|| {
                QghError::validation(
                    "model.artifact_missing",
                    "Prepared local model artifact is unavailable.",
                )
            })
    }

    #[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
    pub fn revalidate_artifact_identities(&self) -> Result<(), QghError> {
        for (relative_path, expected) in &self.identities {
            let path = self
                .paths
                .get(relative_path)
                .ok_or_else(model_artifact_invalid)?;
            let metadata = fs::symlink_metadata(path).map_err(|_| model_artifact_invalid())?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || FileIdentity::from_metadata(&metadata) != *expected
            {
                return Err(model_artifact_invalid());
            }
        }
        Ok(())
    }
}

#[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelInstallAction {
    Installed,
    AlreadyInstalled,
}

pub struct ModelInstallOutcome {
    pub action: ModelInstallAction,
    pub snapshot: PreparedQwenModelSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSnapshotState {
    Missing,
    Present,
    Invalid,
}

#[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
pub trait ModelArtifactFetcher {
    fn fetch(&mut self, spec: &QwenModelSpec, relative_path: &str) -> Result<PathBuf, QghError>;
}

#[derive(Debug, Clone)]
pub struct PreparedQwenModelStore {
    root: PathBuf,
}

impl PreparedQwenModelStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Performs a metadata-only readiness check suitable for `status`.
    ///
    /// This validates the pinned manifest, exact tree, file kinds, and byte
    /// sizes without hashing the multi-gigabyte payload on every CLI process.
    /// Runtime loading still calls `inspect`, which verifies every checksum.
    pub fn snapshot_state(&self, spec: &QwenModelSpec) -> ModelSnapshotState {
        match self.inspect_layout(spec) {
            Ok(_) => ModelSnapshotState::Present,
            Err(error) if error.code == "model.not_installed" => ModelSnapshotState::Missing,
            Err(_) => ModelSnapshotState::Invalid,
        }
    }

    #[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
    pub fn install_with_fetcher(
        &self,
        spec: &QwenModelSpec,
        fetcher: &mut dyn ModelArtifactFetcher,
    ) -> Result<ModelInstallOutcome, QghError> {
        validate_spec(spec)?;
        ensure_private_dir(&self.root).map_err(model_qgh_storage_error)?;
        let destination = self.model_root(spec);
        let mut quarantine = None;
        if fs::symlink_metadata(&destination).is_ok() {
            match self.inspect(spec) {
                Ok(snapshot) => {
                    return Ok(ModelInstallOutcome {
                        action: ModelInstallAction::AlreadyInstalled,
                        snapshot,
                    });
                }
                Err(error)
                    if matches!(
                        error.code.as_str(),
                        "model.snapshot_invalid" | "model.artifact_invalid"
                    ) =>
                {
                    let path = self.quarantine_root(spec)?;
                    fs::rename(&destination, &path).map_err(|_| {
                        model_install_error("Could not isolate the invalid local model snapshot.")
                    })?;
                    sync_directory(&self.root)?;
                    quarantine = Some(path);
                }
                Err(error) => return Err(error),
            }
        }

        let staging = self.staging_root(spec)?;
        fs::create_dir(&staging).map_err(model_storage_error)?;
        set_private_dir(&staging).map_err(model_qgh_storage_error)?;
        let install_result = self.populate_staging(spec, fetcher, &staging);
        if let Err(error) = install_result {
            let _ = fs::remove_dir_all(&staging);
            restore_quarantine(&destination, quarantine.as_deref());
            return Err(error);
        }

        match fs::rename(&staging, &destination) {
            Ok(()) => {}
            Err(_) => {
                if let Ok(snapshot) = self.inspect(spec) {
                    let _ = fs::remove_dir_all(&staging);
                    remove_quarantine(quarantine.as_deref());
                    return Ok(ModelInstallOutcome {
                        action: ModelInstallAction::AlreadyInstalled,
                        snapshot,
                    });
                }
                let _ = fs::remove_dir_all(&staging);
                restore_quarantine(&destination, quarantine.as_deref());
                return Err(model_install_error(
                    "Could not atomically publish the prepared local model.",
                ));
            }
        }
        set_private_dir(&destination).map_err(model_qgh_storage_error)?;
        sync_directory(&self.root)?;
        remove_quarantine(quarantine.as_deref());
        sync_directory(&self.root)?;
        Ok(ModelInstallOutcome {
            action: ModelInstallAction::Installed,
            snapshot: self.inspect(spec)?,
        })
    }

    pub fn inspect(&self, spec: &QwenModelSpec) -> Result<PreparedQwenModelSnapshot, QghError> {
        let snapshot = self.inspect_layout(spec)?;
        for artifact in &spec.artifacts {
            let canonical_path = snapshot
                .paths
                .get(&artifact.relative_path)
                .ok_or_else(model_artifact_invalid)?;
            let identity = snapshot
                .identities
                .get(&artifact.relative_path)
                .ok_or_else(model_artifact_invalid)?;
            let verification_cached = VERIFIED_ARTIFACTS
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .map_err(|_| model_artifact_invalid())?
                .get(canonical_path)
                .is_some_and(|(sha256, verified_identity)| {
                    sha256 == &artifact.sha256 && verified_identity == identity
                });
            if !verification_cached {
                let (sha256, byte_size, hashed_identity) = hash_file_stable(canonical_path)?;
                if sha256 != artifact.sha256 || byte_size != artifact.byte_size {
                    return Err(model_artifact_invalid());
                }
                if &hashed_identity != identity {
                    return Err(model_artifact_invalid());
                }
                VERIFIED_ARTIFACTS
                    .get_or_init(|| Mutex::new(HashMap::new()))
                    .lock()
                    .map_err(|_| model_artifact_invalid())?
                    .insert(
                        canonical_path.clone(),
                        (artifact.sha256.clone(), identity.clone()),
                    );
            }
        }
        Ok(snapshot)
    }

    fn inspect_layout(&self, spec: &QwenModelSpec) -> Result<PreparedQwenModelSnapshot, QghError> {
        validate_spec(spec)?;
        let root = self.model_root(spec);
        let root_metadata = fs::symlink_metadata(&root).map_err(|_| model_not_installed())?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            return Err(model_snapshot_invalid());
        }
        let manifest_path = root.join("manifest.json");
        let manifest_metadata =
            fs::symlink_metadata(&manifest_path).map_err(|_| model_snapshot_invalid())?;
        if manifest_metadata.file_type().is_symlink()
            || !manifest_metadata.is_file()
            || manifest_metadata.len() > MAX_MANIFEST_BYTES
        {
            return Err(model_snapshot_invalid());
        }
        let manifest_bytes = read_bounded(&manifest_path, MAX_MANIFEST_BYTES)?;
        let manifest: LocalModelManifestV1 =
            serde_json::from_slice(&manifest_bytes).map_err(|_| model_snapshot_invalid())?;
        let expected = LocalModelManifestV1::for_spec(spec);
        if manifest != expected {
            return Err(model_snapshot_invalid());
        }
        validate_snapshot_tree(&root, &manifest)?;

        let canonical_root = fs::canonicalize(&root).map_err(|_| model_snapshot_invalid())?;
        let mut paths = BTreeMap::new();
        let mut identities = BTreeMap::new();
        for artifact in &manifest.artifacts {
            validate_relative_path(&artifact.relative_path)?;
            let path = root.join(&artifact.relative_path);
            reject_symlink_components(&root, Path::new(&artifact.relative_path))?;
            let metadata = fs::symlink_metadata(&path).map_err(|_| model_artifact_invalid())?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() != artifact.byte_size
            {
                return Err(model_artifact_invalid());
            }
            let canonical_path = fs::canonicalize(&path).map_err(|_| model_artifact_invalid())?;
            if !canonical_path.starts_with(&canonical_root) {
                return Err(model_artifact_invalid());
            }
            let identity = FileIdentity::from_metadata(&metadata);
            paths.insert(artifact.relative_path.clone(), canonical_path);
            identities.insert(artifact.relative_path.clone(), identity);
        }
        Ok(PreparedQwenModelSnapshot {
            root: canonical_root,
            manifest_hash: manifest.hash(),
            paths,
            identities,
        })
    }

    fn model_root(&self, spec: &QwenModelSpec) -> PathBuf {
        self.root.join(&spec.preset_id)
    }

    #[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
    fn staging_root(&self, spec: &QwenModelSpec) -> Result<PathBuf, QghError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| model_install_error("Could not create a model staging directory."))?
            .as_nanos();
        Ok(self.root.join(format!(
            ".staging-{}-{}-{nonce}",
            spec.preset_id,
            std::process::id()
        )))
    }

    #[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
    fn quarantine_root(&self, spec: &QwenModelSpec) -> Result<PathBuf, QghError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| model_install_error("Could not isolate an invalid model snapshot."))?
            .as_nanos();
        Ok(self.root.join(format!(
            ".invalid-{}-{}-{nonce}",
            spec.preset_id,
            std::process::id()
        )))
    }

    #[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
    fn populate_staging(
        &self,
        spec: &QwenModelSpec,
        fetcher: &mut dyn ModelArtifactFetcher,
        staging: &Path,
    ) -> Result<(), QghError> {
        for artifact in &spec.artifacts {
            let source = fetcher.fetch(spec, &artifact.relative_path)?;
            let destination = staging.join(&artifact.relative_path);
            if let Some(parent) = destination.parent() {
                ensure_private_dir(parent).map_err(model_qgh_storage_error)?;
            }
            copy_verified_artifact(&source, &destination, artifact)?;
        }
        let manifest = LocalModelManifestV1::for_spec(spec);
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|_| model_install_error("Could not encode the local model manifest."))?;
        let manifest_path = staging.join("manifest.json");
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&manifest_path)
            .map_err(model_storage_error)?;
        file.write_all(&manifest_bytes)
            .map_err(model_storage_error)?;
        file.sync_all().map_err(model_storage_error)?;
        set_private_file(&manifest_path).map_err(model_qgh_storage_error)?;
        sync_directory_tree(staging)
    }
}

pub fn default_prepared_qwen_model_store() -> Result<PreparedQwenModelStore, QghError> {
    Ok(PreparedQwenModelStore::new(
        qgh_cache_dir()?.join("prepared-qwen-models"),
    ))
}

#[cfg(feature = "fastembed-provider")]
struct HfQwenModelFetcher {
    repo: hf_hub::api::sync::ApiRepo,
}

#[cfg(feature = "fastembed-provider")]
impl HfQwenModelFetcher {
    fn new(spec: &QwenModelSpec, show_progress: bool) -> Result<Self, QghError> {
        let cache_dir = qgh_cache_dir()?.join("hf");
        ensure_private_dir(&cache_dir).map_err(model_qgh_storage_error)?;
        let api = hf_hub::api::sync::ApiBuilder::new()
            .with_cache_dir(cache_dir)
            .with_endpoint("https://huggingface.co".to_string())
            .with_progress(show_progress)
            .with_retries(2)
            .with_token(None)
            .build()
            .map_err(|_| model_download_error())?;
        Ok(Self {
            repo: api.repo(hf_hub::Repo::with_revision(
                spec.model_id.clone(),
                hf_hub::RepoType::Model,
                spec.resolved_revision.clone(),
            )),
        })
    }
}

#[cfg(feature = "fastembed-provider")]
impl ModelArtifactFetcher for HfQwenModelFetcher {
    fn fetch(&mut self, spec: &QwenModelSpec, relative_path: &str) -> Result<PathBuf, QghError> {
        if !spec
            .artifacts
            .iter()
            .any(|artifact| artifact.relative_path == relative_path)
        {
            return Err(model_snapshot_invalid());
        }
        self.repo
            .get(relative_path)
            .map_err(|_| model_download_error())
    }
}

#[cfg(feature = "fastembed-provider")]
pub fn install_qwen_model(
    preset_id: &str,
    show_progress: bool,
) -> Result<ModelInstallOutcome, QghError> {
    let spec = qwen_model_spec(preset_id).ok_or_else(|| {
        QghError::validation(
            "model.unknown",
            "The requested local model is not supported.",
        )
    })?;
    let store = default_prepared_qwen_model_store()?;
    match store.inspect(&spec) {
        Ok(snapshot) => {
            return Ok(ModelInstallOutcome {
                action: ModelInstallAction::AlreadyInstalled,
                snapshot,
            });
        }
        Err(error)
            if matches!(
                error.code.as_str(),
                "model.not_installed" | "model.snapshot_invalid" | "model.artifact_invalid"
            ) => {}
        Err(error) => return Err(error),
    }
    let mut fetcher = HfQwenModelFetcher::new(&spec, show_progress)?;
    store.install_with_fetcher(&spec, &mut fetcher)
}

#[cfg(not(feature = "fastembed-provider"))]
pub fn install_qwen_model(
    _preset_id: &str,
    _show_progress: bool,
) -> Result<ModelInstallOutcome, QghError> {
    Err(QghError::validation(
        "model.provider_unavailable",
        "This qgh binary was built without local Qwen model installation support.",
    )
    .with_hint("Install a qgh release binary with local model support."))
}

fn validate_spec(spec: &QwenModelSpec) -> Result<(), QghError> {
    if spec.preset_id.is_empty()
        || !spec.preset_id.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == '-'
                || character == '_'
                || character == '.'
        })
        || spec.model_id.is_empty()
        || spec.resolved_revision.len() != 40
        || !spec
            .resolved_revision
            .chars()
            .all(|character| character.is_ascii_hexdigit())
        || spec.artifacts.is_empty()
    {
        return Err(model_snapshot_invalid());
    }
    let mut paths = std::collections::BTreeSet::new();
    for artifact in &spec.artifacts {
        validate_relative_path(&artifact.relative_path)?;
        if !paths.insert(&artifact.relative_path)
            || artifact.byte_size == 0
            || artifact.sha256.len() != 64
            || !artifact
                .sha256
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            return Err(model_snapshot_invalid());
        }
    }
    Ok(())
}

fn validate_relative_path(relative_path: &str) -> Result<(), QghError> {
    let path = Path::new(relative_path);
    if relative_path.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(model_snapshot_invalid());
    }
    Ok(())
}

fn reject_symlink_components(root: &Path, relative_path: &Path) -> Result<(), QghError> {
    let mut current = root.to_path_buf();
    for component in relative_path.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(model_snapshot_invalid());
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current).map_err(|_| model_artifact_invalid())?;
        if metadata.file_type().is_symlink() {
            return Err(model_artifact_invalid());
        }
    }
    Ok(())
}

#[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
fn copy_verified_artifact(
    source: &Path,
    destination: &Path,
    expected: &ModelArtifactSpec,
) -> Result<(), QghError> {
    let mut input = File::open(source).map_err(|_| model_download_error())?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(model_storage_error)?;
    let mut hasher = Sha256::new();
    let mut byte_size = 0u64;
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|_| model_download_error())?;
        if read == 0 {
            break;
        }
        byte_size = byte_size
            .checked_add(read as u64)
            .ok_or_else(model_artifact_invalid)?;
        if byte_size > expected.byte_size {
            return Err(model_artifact_invalid());
        }
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .map_err(model_storage_error)?;
    }
    if byte_size != expected.byte_size || hex_digest(&hasher.finalize()) != expected.sha256 {
        return Err(model_artifact_invalid());
    }
    output.sync_all().map_err(model_storage_error)?;
    set_private_file(destination).map_err(model_qgh_storage_error)
}

fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, QghError> {
    let file = File::open(path).map_err(|_| model_snapshot_invalid())?;
    let mut bytes = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| model_snapshot_invalid())?;
    if bytes.len() as u64 > max_bytes {
        return Err(model_snapshot_invalid());
    }
    Ok(bytes)
}

fn hash_file_stable(path: &Path) -> Result<(String, u64, FileIdentity), QghError> {
    let mut file = File::open(path).map_err(|_| model_artifact_invalid())?;
    let before =
        FileIdentity::from_metadata(&file.metadata().map_err(|_| model_artifact_invalid())?);
    let mut hasher = Sha256::new();
    let mut byte_size = 0u64;
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| model_artifact_invalid())?;
        if read == 0 {
            break;
        }
        byte_size = byte_size
            .checked_add(read as u64)
            .ok_or_else(model_artifact_invalid)?;
        hasher.update(&buffer[..read]);
    }
    let after =
        FileIdentity::from_metadata(&file.metadata().map_err(|_| model_artifact_invalid())?);
    if before != after {
        return Err(model_artifact_invalid());
    }
    Ok((hex_digest(&hasher.finalize()), byte_size, after))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        }
    }
}

fn validate_snapshot_tree(root: &Path, manifest: &LocalModelManifestV1) -> Result<(), QghError> {
    let mut expected_files = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.relative_path.clone())
        .collect::<BTreeSet<_>>();
    expected_files.insert("manifest.json".to_string());
    let mut expected_dirs = BTreeSet::new();
    for relative_path in &expected_files {
        let mut parent = Path::new(relative_path).parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() {
                break;
            }
            expected_dirs.insert(path.to_string_lossy().into_owned());
            parent = path.parent();
        }
    }

    let mut actual_files = BTreeSet::new();
    let mut actual_dirs = BTreeSet::new();
    collect_snapshot_tree(root, root, &mut actual_files, &mut actual_dirs)?;
    if actual_files != expected_files || actual_dirs != expected_dirs {
        return Err(model_snapshot_invalid());
    }
    Ok(())
}

fn collect_snapshot_tree(
    root: &Path,
    current: &Path,
    files: &mut BTreeSet<String>,
    directories: &mut BTreeSet<String>,
) -> Result<(), QghError> {
    for entry in fs::read_dir(current).map_err(|_| model_snapshot_invalid())? {
        let entry = entry.map_err(|_| model_snapshot_invalid())?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|_| model_snapshot_invalid())?;
        if metadata.file_type().is_symlink() {
            return Err(model_artifact_invalid());
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| model_snapshot_invalid())?;
        let relative = relative
            .to_str()
            .ok_or_else(model_snapshot_invalid)?
            .to_string();
        validate_relative_path(&relative)?;
        if metadata.is_dir() {
            directories.insert(relative);
            collect_snapshot_tree(root, &path, files, directories)?;
        } else if metadata.is_file() {
            files.insert(relative);
        } else {
            return Err(model_snapshot_invalid());
        }
    }
    Ok(())
}

#[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
fn restore_quarantine(destination: &Path, quarantine: Option<&Path>) {
    let Some(quarantine) = quarantine else {
        return;
    };
    if fs::symlink_metadata(destination).is_ok() {
        remove_quarantine(Some(quarantine));
    } else {
        let _ = fs::rename(quarantine, destination);
    }
}

#[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
fn remove_quarantine(quarantine: Option<&Path>) {
    let Some(path) = quarantine else {
        return;
    };
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        let _ = fs::remove_dir_all(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

#[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
fn sync_directory_tree(root: &Path) -> Result<(), QghError> {
    fn collect(path: &Path, directories: &mut Vec<PathBuf>) -> Result<(), QghError> {
        directories.push(path.to_path_buf());
        for entry in fs::read_dir(path).map_err(model_storage_error)? {
            let entry = entry.map_err(model_storage_error)?;
            let metadata = entry.metadata().map_err(model_storage_error)?;
            if metadata.is_dir() {
                collect(&entry.path(), directories)?;
            }
        }
        Ok(())
    }

    let mut directories = Vec::new();
    collect(root, &mut directories)?;
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        sync_directory(&directory)?;
    }
    Ok(())
}

#[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
fn sync_directory(path: &Path) -> Result<(), QghError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(model_storage_error)
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn model_not_installed() -> QghError {
    QghError::validation(
        "model.not_installed",
        "The requested local model is not installed.",
    )
}

fn model_snapshot_invalid() -> QghError {
    QghError::validation(
        "model.snapshot_invalid",
        "The prepared local model snapshot is invalid.",
    )
}

fn model_artifact_invalid() -> QghError {
    QghError::validation(
        "model.artifact_invalid",
        "A prepared local model artifact failed integrity validation.",
    )
}

#[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
fn model_download_error() -> QghError {
    let mut error = QghError::new(
        "model.download_failed",
        "Could not download a pinned local model artifact.",
        3,
    );
    error.retryable = true;
    error
}

#[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
fn model_install_error(message: &'static str) -> QghError {
    QghError::new("model.install_failed", message, 6)
}

#[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
fn model_storage_error(_error: std::io::Error) -> QghError {
    model_install_error("Could not write the prepared local model snapshot.")
}

#[cfg_attr(not(feature = "fastembed-provider"), allow(dead_code))]
fn model_qgh_storage_error(_error: QghError) -> QghError {
    model_install_error("Could not secure the prepared local model snapshot.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct FixtureFetcher {
        root: PathBuf,
    }

    struct PanicFetcher;

    impl ModelArtifactFetcher for FixtureFetcher {
        fn fetch(
            &mut self,
            _spec: &QwenModelSpec,
            relative_path: &str,
        ) -> Result<PathBuf, QghError> {
            Ok(self.root.join(relative_path))
        }
    }

    impl ModelArtifactFetcher for PanicFetcher {
        fn fetch(
            &mut self,
            _spec: &QwenModelSpec,
            _relative_path: &str,
        ) -> Result<PathBuf, QghError> {
            panic!("an already-installed model must not be fetched")
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "qgh-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn fixture_spec() -> QwenModelSpec {
        let artifacts = [
            ("config.json", b"config".as_slice()),
            ("weights.bin", b"weights"),
        ]
        .into_iter()
        .map(|(relative_path, bytes)| ModelArtifactSpec {
            relative_path: relative_path.to_string(),
            sha256: sha256(bytes),
            byte_size: bytes.len() as u64,
        })
        .collect();
        QwenModelSpec {
            preset_id: "fixture-model".to_string(),
            purpose: ModelPurpose::Embedding,
            model_id: "Fixture/model".to_string(),
            resolved_revision: "0123456789012345678901234567890123456789".to_string(),
            artifacts,
        }
    }

    #[test]
    fn pinned_qwen_manifest_identity_is_pure_and_stable() {
        let spec = qwen_model_spec(QWEN_EMBEDDING_PRESET_ID).unwrap();

        assert_eq!(
            qwen_model_manifest_hash(&spec),
            "e0915f9f5946dc0b6309e9923e5d319b81de1e54985b7c00f9f23957e2c46af4"
        );
    }

    #[test]
    fn install_verifies_artifacts_and_atomically_publishes_a_snapshot() {
        let root = temp_dir("model-store-red");
        let source = temp_dir("model-source-red");
        fs::write(source.join("config.json"), b"config").unwrap();
        fs::write(source.join("weights.bin"), b"weights").unwrap();
        let store = PreparedQwenModelStore::new(root.clone());
        let mut fetcher = FixtureFetcher { root: source };

        let installed = store
            .install_with_fetcher(&fixture_spec(), &mut fetcher)
            .unwrap();

        assert_eq!(installed.action, ModelInstallAction::Installed);
        assert_eq!(
            fs::read(installed.snapshot.artifact_path("weights.bin").unwrap()).unwrap(),
            b"weights"
        );
        assert!(installed.snapshot.root.join("manifest.json").is_file());
        assert_eq!(
            store.inspect(&fixture_spec()).unwrap().manifest_hash,
            installed.snapshot.manifest_hash
        );
        assert!(fs::read_dir(root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".staging-")));
    }

    #[test]
    fn reinstall_reuses_only_a_fully_verified_snapshot() {
        let root = temp_dir("model-reinstall");
        let source = temp_dir("model-reinstall-source");
        fs::write(source.join("config.json"), b"config").unwrap();
        fs::write(source.join("weights.bin"), b"weights").unwrap();
        let store = PreparedQwenModelStore::new(root);
        store
            .install_with_fetcher(
                &fixture_spec(),
                &mut FixtureFetcher {
                    root: source.clone(),
                },
            )
            .unwrap();

        let reused = store
            .install_with_fetcher(&fixture_spec(), &mut PanicFetcher)
            .unwrap();

        assert_eq!(reused.action, ModelInstallAction::AlreadyInstalled);
    }

    #[test]
    fn reinstall_atomically_repairs_a_corrupt_snapshot() {
        let root = temp_dir("model-repair");
        let source = temp_dir("model-repair-source");
        fs::write(source.join("config.json"), b"config").unwrap();
        fs::write(source.join("weights.bin"), b"weights").unwrap();
        let store = PreparedQwenModelStore::new(root.clone());
        store
            .install_with_fetcher(
                &fixture_spec(),
                &mut FixtureFetcher {
                    root: source.clone(),
                },
            )
            .unwrap();
        fs::write(root.join("fixture-model/weights.bin"), b"weightr").unwrap();

        let repaired = store
            .install_with_fetcher(&fixture_spec(), &mut FixtureFetcher { root: source })
            .unwrap();

        assert_eq!(repaired.action, ModelInstallAction::Installed);
        assert_eq!(
            fs::read(repaired.snapshot.artifact_path("weights.bin").unwrap()).unwrap(),
            b"weights"
        );
        assert!(fs::read_dir(root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".invalid-")));
    }

    #[test]
    fn failed_integrity_check_leaves_no_published_or_staging_snapshot() {
        let root = temp_dir("model-integrity-failure");
        let source = temp_dir("model-integrity-failure-source");
        fs::write(source.join("config.json"), b"config").unwrap();
        fs::write(source.join("weights.bin"), b"private-corrupt-payload").unwrap();
        let store = PreparedQwenModelStore::new(root.clone());

        let error = store
            .install_with_fetcher(&fixture_spec(), &mut FixtureFetcher { root: source })
            .err()
            .unwrap();

        assert_eq!(error.code, "model.artifact_invalid");
        assert!(!root.join("fixture-model").exists());
        assert!(fs::read_dir(root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".staging-")));
        assert!(!error.message.contains("private-corrupt-payload"));
    }

    #[test]
    fn strict_manifest_and_artifact_hashes_fail_closed_after_install() {
        let root = temp_dir("model-strict-manifest");
        let source = temp_dir("model-strict-manifest-source");
        fs::write(source.join("config.json"), b"config").unwrap();
        fs::write(source.join("weights.bin"), b"weights").unwrap();
        let store = PreparedQwenModelStore::new(root.clone());
        store
            .install_with_fetcher(&fixture_spec(), &mut FixtureFetcher { root: source })
            .unwrap();

        fs::write(root.join("fixture-model/weights.bin"), b"weightr").unwrap();
        let corrupt = store.inspect(&fixture_spec()).err().unwrap();
        assert_eq!(corrupt.code, "model.artifact_invalid");

        fs::write(root.join("fixture-model/weights.bin"), b"weights").unwrap();
        let manifest_path = root.join("fixture-model/manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["unknown_runtime_knob"] = serde_json::json!(true);
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let unknown = store.inspect(&fixture_spec()).err().unwrap();
        assert_eq!(unknown.code, "model.snapshot_invalid");
    }

    #[test]
    fn prepared_snapshot_rejects_unexpected_files_and_directories() {
        let root = temp_dir("model-unexpected-tree");
        let source = temp_dir("model-unexpected-tree-source");
        fs::write(source.join("config.json"), b"config").unwrap();
        fs::write(source.join("weights.bin"), b"weights").unwrap();
        let store = PreparedQwenModelStore::new(root.clone());
        store
            .install_with_fetcher(&fixture_spec(), &mut FixtureFetcher { root: source })
            .unwrap();
        fs::create_dir(root.join("fixture-model/extra")).unwrap();
        fs::write(root.join("fixture-model/extra/file"), b"unexpected").unwrap();

        let error = store.inspect(&fixture_spec()).err().unwrap();

        assert_eq!(error.code, "model.snapshot_invalid");
    }

    #[cfg(unix)]
    #[test]
    fn prepared_snapshot_rejects_symbolic_link_artifacts() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("model-symlink");
        let source = temp_dir("model-symlink-source");
        fs::write(source.join("config.json"), b"config").unwrap();
        fs::write(source.join("weights.bin"), b"weights").unwrap();
        let store = PreparedQwenModelStore::new(root.clone());
        store
            .install_with_fetcher(&fixture_spec(), &mut FixtureFetcher { root: source })
            .unwrap();
        let outside = root.join("outside-weights.bin");
        fs::write(&outside, b"weights").unwrap();
        let weights = root.join("fixture-model/weights.bin");
        fs::remove_file(&weights).unwrap();
        symlink(&outside, &weights).unwrap();

        let error = store.inspect(&fixture_spec()).err().unwrap();

        assert_eq!(error.code, "model.artifact_invalid");
    }
}
