use crate::error::QghError;
use crate::model::IndexSource;
use crate::paths::{ensure_private_dir, set_private_dir, set_private_file};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value, STORED, STRING, TEXT};
use tantivy::{Index, TantivyDocument, Term};

const SOURCE_INVENTORY_COMMIT_PREFIX: &str = "qgh.source_inventory.v1:";
const INDEX_BUILD_MARKER_FILE: &str = ".qgh-build-owner-v1";
const INDEX_BUILD_MARKER_SEAL_TEMP: &str = ".qgh-build-owner-v1.sealing";

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub source_id: String,
    pub source_updated_at: Option<String>,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct SearchFilters {
    pub repo: Option<String>,
    pub labels: Vec<String>,
    pub state: Option<String>,
    pub author: Option<String>,
    pub issue: Option<i64>,
    pub source_types: Vec<String>,
}

/// Versioned lexical ranking profiles are intentionally internal.  The
/// production default remains `V1`; experiments can opt into a named profile
/// without exposing arbitrary user-controlled boosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LexicalRankingProfile {
    #[default]
    V1,
    MetadataBoostV1,
}

/// Fixed lexical profiles available only to the release/live-qrels harness.
///
/// This deliberately exposes names, not boost values. Production callers do
/// not use this type and remain pinned to `V1` through `search_with_filters`.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalLexicalProfile {
    ProductionV1,
    MetadataBoostV1,
}

/// Returns the fixed lexical profile used by production query paths.
#[doc(hidden)]
pub fn production_lexical_profile_for_eval() -> EvalLexicalProfile {
    match LexicalRankingProfile::default() {
        LexicalRankingProfile::V1 => EvalLexicalProfile::ProductionV1,
        LexicalRankingProfile::MetadataBoostV1 => EvalLexicalProfile::MetadataBoostV1,
    }
}

impl From<EvalLexicalProfile> for LexicalRankingProfile {
    fn from(profile: EvalLexicalProfile) -> Self {
        match profile {
            EvalLexicalProfile::ProductionV1 => Self::V1,
            EvalLexicalProfile::MetadataBoostV1 => Self::MetadataBoostV1,
        }
    }
}

impl Default for SearchFilters {
    fn default() -> Self {
        Self {
            repo: None,
            labels: Vec::new(),
            state: None,
            author: None,
            issue: None,
            source_types: vec!["issue".to_string(), "issue_comment".to_string()],
        }
    }
}

pub fn rebuild(
    index_root: &Path,
    generation: i64,
    sources: &[IndexSource],
) -> Result<PathBuf, QghError> {
    ensure_private_dir(index_root)?;
    let shadow_path = index_root.join(format!("shadow-{generation}"));
    let generation_path = index_root.join(format!("generation-{generation}"));
    if generation_path.exists() || shadow_path.exists() {
        return Err(index_build_collision_error());
    }
    ensure_private_dir(&shadow_path)?;
    rebuild_in_shadow(&shadow_path, &generation_path, sources, None)?;
    Ok(generation_path)
}

pub(crate) fn prepare_owned_rebuild(
    index_root: &Path,
    generation: i64,
    owner_token: &str,
) -> Result<(), QghError> {
    ensure_private_dir(index_root)?;
    let shadow_path = index_root.join(format!("shadow-{generation}"));
    let generation_path = index_root.join(format!("generation-{generation}"));
    if shadow_path.exists() || generation_path.exists() {
        return Err(index_build_collision_error());
    }
    fs::create_dir(&shadow_path).map_err(|_| index_build_collision_error())?;
    set_private_dir(&shadow_path)?;
    let marker_path = shadow_path.join(INDEX_BUILD_MARKER_FILE);
    let marker_result = (|| -> Result<(), QghError> {
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker_path)
            .map_err(|_| index_build_collision_error())?;
        marker
            .write_all(index_build_marker(generation, owner_token).as_bytes())
            .map_err(|_| index_build_collision_error())?;
        marker
            .sync_all()
            .map_err(|_| index_build_collision_error())?;
        set_private_file(&marker_path)?;
        Ok(())
    })();
    if marker_result.is_err() {
        let _ = fs::remove_file(&marker_path);
        let _ = fs::remove_dir(&shadow_path);
    }
    marker_result
}

pub(crate) fn rebuild_owned(
    index_root: &Path,
    generation: i64,
    owner_token: &str,
    sources: &[IndexSource],
) -> Result<PathBuf, QghError> {
    ensure_private_dir(index_root)?;
    let shadow_path = index_root.join(format!("shadow-{generation}"));
    let generation_path = index_root.join(format!("generation-{generation}"));
    if generation_path.exists() {
        return Err(index_build_collision_error());
    }
    validate_owned_build_directory(&shadow_path, generation, owner_token)?;
    rebuild_in_shadow(
        &shadow_path,
        &generation_path,
        sources,
        Some((generation, owner_token)),
    )?;
    validate_owned_generation_directory(&generation_path, generation, owner_token)?;
    Ok(generation_path)
}

fn rebuild_in_shadow(
    shadow_path: &Path,
    generation_path: &Path,
    sources: &[IndexSource],
    ownership: Option<(i64, &str)>,
) -> Result<(), QghError> {
    let (schema, fields) = schema();
    let index =
        Index::create_in_dir(shadow_path, schema).map_err(|e| QghError::index(e.to_string()))?;
    let mut writer = index
        .writer(50_000_000)
        .map_err(|e| QghError::index(e.to_string()))?;
    for source in sources {
        writer
            .add_document(index_source_document(&fields, source))
            .map_err(|e| QghError::index(e.to_string()))?;
    }
    let mut prepared_commit = writer
        .prepare_commit()
        .map_err(|e| QghError::index(e.to_string()))?;
    prepared_commit.set_payload(&format!(
        "{SOURCE_INVENTORY_COMMIT_PREFIX}{}",
        source_inventory_digest(sources)
    ));
    prepared_commit
        .commit()
        .map_err(|e| QghError::index(e.to_string()))?;
    writer
        .wait_merging_threads()
        .map_err(|e| QghError::index(e.to_string()))?;
    if let Some((generation, owner_token)) = ownership {
        seal_owned_generation_directory(shadow_path, generation, owner_token)?;
    }
    if generation_path.exists() {
        return Err(index_build_collision_error());
    }
    rename_without_replacement(shadow_path, generation_path)?;
    set_private_dir(generation_path)?;
    Ok(())
}

fn seal_owned_generation_directory(
    path: &Path,
    generation: i64,
    owner_token: &str,
) -> Result<(), QghError> {
    validate_owned_build_directory(path, generation, owner_token)?;
    write_owned_generation_seal(path, generation, owner_token)
}

pub(crate) fn adopt_legacy_generation_directory(
    path: &Path,
    generation: i64,
    owner_token: &str,
) -> Result<(), QghError> {
    if validate_owned_generation_directory(path, generation, owner_token).is_ok() {
        return Ok(());
    }
    validate_legacy_build_directory(path)?;
    validate_legacy_tantivy_file_inventory(path)?;
    write_owned_generation_seal(path, generation, owner_token)?;
    validate_owned_generation_directory(path, generation, owner_token)
}

fn write_owned_generation_seal(
    path: &Path,
    generation: i64,
    owner_token: &str,
) -> Result<(), QghError> {
    let seal_temp = path.join(INDEX_BUILD_MARKER_SEAL_TEMP);
    let tree_digest = owned_generation_tree_digest(path)?;
    let sealed = sealed_index_build_marker(generation, owner_token, &tree_digest);
    if seal_temp.exists() {
        let metadata =
            fs::symlink_metadata(&seal_temp).map_err(|_| index_build_collision_error())?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || fs::read_to_string(&seal_temp).map_err(|_| index_build_collision_error())? != sealed
        {
            return Err(index_build_collision_error());
        }
    } else {
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&seal_temp)
            .map_err(|_| index_build_collision_error())?;
        if marker.write_all(sealed.as_bytes()).is_err()
            || marker.sync_all().is_err()
            || set_private_file(&seal_temp).is_err()
        {
            let _ = fs::remove_file(&seal_temp);
            return Err(index_build_collision_error());
        }
    }
    fs::rename(&seal_temp, path.join(INDEX_BUILD_MARKER_FILE))
        .map_err(|_| index_build_collision_error())?;
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| index_build_collision_error())?;
    Ok(())
}

fn validate_legacy_build_directory(path: &Path) -> Result<(), QghError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| index_build_collision_error())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(index_build_collision_error());
    }
    let marker_path = path.join(INDEX_BUILD_MARKER_FILE);
    let marker_metadata =
        fs::symlink_metadata(&marker_path).map_err(|_| index_build_collision_error())?;
    if marker_metadata.file_type().is_symlink() || !marker_metadata.is_file() {
        return Err(index_build_collision_error());
    }
    let observed = fs::read_to_string(marker_path).map_err(|_| index_build_collision_error())?;
    let Some(digest) = observed.strip_prefix("qgh.index-build-owner.v1:") else {
        return Err(index_build_collision_error());
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(index_build_collision_error());
    }
    Ok(())
}

fn validate_legacy_tantivy_file_inventory(path: &Path) -> Result<(), QghError> {
    let index = Index::open_in_dir(path).map_err(|_| index_build_collision_error())?;
    if !index
        .validate_checksum()
        .map_err(|_| index_build_collision_error())?
        .is_empty()
    {
        return Err(index_build_collision_error());
    }
    let managed = index.directory().list_managed_files();
    if managed.iter().any(|file| file.components().count() != 1) {
        return Err(index_build_collision_error());
    }
    let mut allowed = managed.iter().cloned().collect::<BTreeSet<_>>();
    for internal in [
        ".managed.json",
        ".tantivy-meta.lock",
        ".tantivy-writer.lock",
        INDEX_BUILD_MARKER_FILE,
        INDEX_BUILD_MARKER_SEAL_TEMP,
    ] {
        allowed.insert(PathBuf::from(internal));
    }
    for managed_path in &managed {
        let metadata = fs::symlink_metadata(path.join(managed_path))
            .map_err(|_| index_build_collision_error())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(index_build_collision_error());
        }
    }
    for entry in fs::read_dir(path).map_err(|_| index_build_collision_error())? {
        let entry = entry.map_err(|_| index_build_collision_error())?;
        let relative = PathBuf::from(entry.file_name());
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|_| index_build_collision_error())?;
        if !allowed.contains(&relative) || metadata.file_type().is_symlink() || !metadata.is_file()
        {
            return Err(index_build_collision_error());
        }
    }
    Ok(())
}

pub(crate) fn validate_owned_build_directory(
    path: &Path,
    generation: i64,
    owner_token: &str,
) -> Result<(), QghError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| index_build_collision_error())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(index_build_collision_error());
    }
    let marker_path = path.join(INDEX_BUILD_MARKER_FILE);
    let marker_metadata =
        fs::symlink_metadata(&marker_path).map_err(|_| index_build_collision_error())?;
    if marker_metadata.file_type().is_symlink() || !marker_metadata.is_file() {
        return Err(index_build_collision_error());
    }
    let observed = fs::read_to_string(marker_path).map_err(|_| index_build_collision_error())?;
    if observed != index_build_marker(generation, owner_token) {
        return Err(index_build_collision_error());
    }
    Ok(())
}

pub(crate) fn validate_owned_generation_directory(
    path: &Path,
    generation: i64,
    owner_token: &str,
) -> Result<(), QghError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| index_build_collision_error())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(index_build_collision_error());
    }
    let marker_path = path.join(INDEX_BUILD_MARKER_FILE);
    let marker_metadata =
        fs::symlink_metadata(&marker_path).map_err(|_| index_build_collision_error())?;
    if marker_metadata.file_type().is_symlink() || !marker_metadata.is_file() {
        return Err(index_build_collision_error());
    }
    let observed = fs::read_to_string(marker_path).map_err(|_| index_build_collision_error())?;
    let tree_digest = owned_generation_tree_digest(path)?;
    if observed != sealed_index_build_marker(generation, owner_token, &tree_digest) {
        return Err(index_build_collision_error());
    }
    Ok(())
}

fn index_build_marker(generation: i64, owner_token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"qgh.index-build-owner.v1");
    hasher.update(generation.to_le_bytes());
    hasher.update(owner_token.as_bytes());
    format!("qgh.index-build-owner.v1:{}", digest_hex(hasher))
}

fn sealed_index_build_marker(generation: i64, owner_token: &str, tree_digest: &str) -> String {
    format!(
        "qgh.index-generation-owner.v1:{}:{tree_digest}",
        index_build_owner_digest(generation, owner_token)
    )
}

fn index_build_owner_digest(generation: i64, owner_token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"qgh.index-build-owner.v1");
    hasher.update(generation.to_le_bytes());
    hasher.update(owner_token.as_bytes());
    digest_hex(hasher)
}

fn owned_generation_tree_digest(root: &Path) -> Result<String, QghError> {
    let mut entries = Vec::new();
    collect_owned_generation_entries(root, root, &mut entries)?;
    entries.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"qgh.index-generation-tree.v1");
    for path in entries {
        let relative = path
            .strip_prefix(root)
            .map_err(|_| index_build_collision_error())?;
        let relative_bytes = relative.as_os_str().as_encoded_bytes();
        hasher.update((relative_bytes.len() as u64).to_le_bytes());
        hasher.update(relative_bytes);
        let metadata = fs::symlink_metadata(&path).map_err(|_| index_build_collision_error())?;
        if metadata.file_type().is_symlink() {
            return Err(index_build_collision_error());
        }
        if metadata.is_dir() {
            hasher.update(b"d");
        } else if metadata.is_file() {
            hasher.update(b"f");
            hasher.update(metadata.len().to_le_bytes());
            let mut file = fs::File::open(&path).map_err(|_| index_build_collision_error())?;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let read = file
                    .read(&mut buffer)
                    .map_err(|_| index_build_collision_error())?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
        } else {
            return Err(index_build_collision_error());
        }
    }
    Ok(digest_hex(hasher))
}

fn collect_owned_generation_entries(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<PathBuf>,
) -> Result<(), QghError> {
    let mut children = fs::read_dir(directory)
        .map_err(|_| index_build_collision_error())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| index_build_collision_error())?;
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| index_build_collision_error())?;
        if relative == Path::new(INDEX_BUILD_MARKER_FILE)
            || relative == Path::new(INDEX_BUILD_MARKER_SEAL_TEMP)
        {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|_| index_build_collision_error())?;
        if metadata.file_type().is_symlink() {
            return Err(index_build_collision_error());
        }
        entries.push(path.clone());
        if metadata.is_dir() {
            collect_owned_generation_entries(root, &path, entries)?;
        } else if !metadata.is_file() {
            return Err(index_build_collision_error());
        }
    }
    Ok(())
}

fn index_build_collision_error() -> QghError {
    QghError::validation(
        "publication.tantivy_artifact_not_ready",
        "The reserved Tantivy generation is unavailable.",
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn rename_without_replacement(from: &Path, to: &Path) -> Result<(), QghError> {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;

    let from =
        CString::new(from.as_os_str().as_bytes()).map_err(|_| index_build_collision_error())?;
    let to = CString::new(to.as_os_str().as_bytes()).map_err(|_| index_build_collision_error())?;

    #[cfg(target_os = "macos")]
    let result = {
        unsafe extern "C" {
            fn renamex_np(from: *const c_char, to: *const c_char, flags: c_uint) -> c_int;
        }
        const RENAME_EXCL: c_uint = 0x0000_0004;
        // SAFETY: both arguments are owned, NUL-terminated path buffers and
        // remain alive for the duration of the syscall.
        unsafe { renamex_np(from.as_ptr(), to.as_ptr(), RENAME_EXCL) }
    };

    #[cfg(target_os = "linux")]
    let result = {
        unsafe extern "C" {
            fn renameat2(
                olddirfd: c_int,
                oldpath: *const c_char,
                newdirfd: c_int,
                newpath: *const c_char,
                flags: c_uint,
            ) -> c_int;
        }
        const AT_FDCWD: c_int = -100;
        const RENAME_NOREPLACE: c_uint = 1;
        // SAFETY: both arguments are owned, NUL-terminated path buffers and
        // remain alive for the duration of the syscall.
        unsafe {
            renameat2(
                AT_FDCWD,
                from.as_ptr(),
                AT_FDCWD,
                to.as_ptr(),
                RENAME_NOREPLACE,
            )
        }
    };

    if result == 0 {
        Ok(())
    } else {
        Err(index_build_collision_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn rename_without_replacement(from: &Path, to: &Path) -> Result<(), QghError> {
    if to.exists() {
        return Err(index_build_collision_error());
    }
    fs::rename(from, to).map_err(|_| index_build_collision_error())
}

pub(crate) fn source_inventory_digest(sources: &[IndexSource]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"qgh.source_inventory.v1");
    hasher.update((sources.len() as u64).to_le_bytes());
    for source in sources {
        hash_text(&mut hasher, &source.source_id);
        hash_text(&mut hasher, &source.entity_type);
        hash_text(&mut hasher, &source.repo);
        hasher.update(source.issue_number.to_le_bytes());
        hash_text(&mut hasher, &source.state);
        hasher.update((source.labels.len() as u64).to_le_bytes());
        for label in &source.labels {
            hash_text(&mut hasher, label);
        }
        match &source.author {
            Some(author) => {
                hasher.update([1]);
                hash_text(&mut hasher, author);
            }
            None => hasher.update([0]),
        }
        hash_text(&mut hasher, &source.title);
        hash_text(&mut hasher, &source.body);
        hash_text(&mut hasher, &source.parent_issue_title);
        hash_text(&mut hasher, &source.github_updated_at);
        hash_text(&mut hasher, &source.indexed_at);
    }
    digest_hex(hasher)
}

pub(crate) fn committed_source_inventory_digest(index: &Index) -> Result<Option<String>, QghError> {
    let payload = index
        .load_metas()
        .map_err(|error| QghError::index(error.to_string()))?
        .payload;
    Ok(payload.and_then(|payload| {
        let digest = payload.strip_prefix(SOURCE_INVENTORY_COMMIT_PREFIX)?;
        (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| digest.to_string())
    }))
}

fn hash_text(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn digest_hex(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn search(
    active_path: &Path,
    query_text: &str,
    limit: usize,
) -> Result<Vec<SearchHit>, QghError> {
    search_with_filters(active_path, query_text, &SearchFilters::default(), limit)
}

pub fn search_with_filters(
    active_path: &Path,
    query_text: &str,
    filters: &SearchFilters,
    limit: usize,
) -> Result<Vec<SearchHit>, QghError> {
    search_with_filters_profile(
        active_path,
        query_text,
        filters,
        LexicalRankingProfile::default(),
        limit,
    )
}

fn search_with_filters_profile(
    active_path: &Path,
    query_text: &str,
    filters: &SearchFilters,
    profile: LexicalRankingProfile,
    limit: usize,
) -> Result<Vec<SearchHit>, QghError> {
    if !active_path.exists() {
        return Err(QghError::index("Tantivy index artifact is missing."));
    }
    if limit == 0 || filters.source_types.is_empty() {
        return Ok(Vec::new());
    }
    let index = Index::open_in_dir(active_path).map_err(|e| QghError::index(e.to_string()))?;
    let schema = index.schema();
    let source_id = schema
        .get_field("source_id")
        .map_err(|e| QghError::index(e.to_string()))?;
    let entity_type = schema
        .get_field("entity_type")
        .map_err(|e| QghError::index(e.to_string()))?;
    let title = schema
        .get_field("title")
        .map_err(|e| QghError::index(e.to_string()))?;
    let body = schema
        .get_field("body")
        .map_err(|e| QghError::index(e.to_string()))?;
    let labels = schema
        .get_field("labels")
        .map_err(|e| QghError::index(e.to_string()))?;
    let label_exact = if filters.labels.is_empty() {
        None
    } else {
        Some(schema.get_field("label_exact").map_err(|_| {
            QghError::validation(
                "validation.stale_index_label_filter",
                "The local BM25 index predates label filtering support and cannot honor a label filter yet.",
            )
            .with_hint("Run `qgh sync` to rebuild the local search index, then retry the label-filtered query.")
        })?)
    };
    let repo = schema
        .get_field("repo")
        .map_err(|e| QghError::index(e.to_string()))?;
    let issue_number = schema
        .get_field("issue_number")
        .map_err(|e| QghError::index(e.to_string()))?;
    let state = schema
        .get_field("state")
        .map_err(|e| QghError::index(e.to_string()))?;
    let author = schema
        .get_field("author")
        .map_err(|e| QghError::index(e.to_string()))?;
    let updated_at = schema.get_field("updated_at").ok();
    let reader = index.reader().map_err(|e| QghError::index(e.to_string()))?;
    let searcher = reader.searcher();
    let mut query_fields = vec![title, body, labels, repo, issue_number];
    if let Ok(parent_issue_title) = schema.get_field("parent_issue_title") {
        query_fields.push(parent_issue_title);
    }
    if let Ok(cjk_ngrams) = schema.get_field("cjk_ngrams") {
        query_fields.push(cjk_ngrams);
    }
    let mut parser = QueryParser::for_index(&index, query_fields);
    if matches!(profile, LexicalRankingProfile::MetadataBoostV1) {
        parser.set_field_boost(title, 2.0);
        if let Ok(parent_issue_title) = schema.get_field("parent_issue_title") {
            parser.set_field_boost(parent_issue_title, 2.0);
        }
        if let Ok(cjk_ngrams) = schema.get_field("cjk_ngrams") {
            parser.set_field_boost(cjk_ngrams, 0.25);
        }
    }
    let expanded_query = expand_cjk_query(query_text);
    let query = parser.parse_query(&expanded_query).map_err(|_| {
        QghError::validation(
            "validation.invalid_query",
            "The query could not be parsed by the local search index.",
        )
    })?;
    let filter_fields = FilterFields {
        entity_type,
        repo,
        issue_number,
        state,
        label_exact,
        author,
    };
    let query = filtered_query(query, &filter_fields, filters);
    let top_docs = searcher
        .search(&query, &TopDocs::with_limit(limit))
        .map_err(|e| QghError::index(e.to_string()))?;
    let mut hits = Vec::new();
    for (score, address) in top_docs {
        let doc = searcher
            .doc::<TantivyDocument>(address)
            .map_err(|e| QghError::index(e.to_string()))?;
        let Some(value) = doc.get_first(source_id) else {
            continue;
        };
        let Some(source_id_text) = value.as_str() else {
            continue;
        };
        hits.push(SearchHit {
            source_id: source_id_text.to_string(),
            source_updated_at: updated_at
                .and_then(|field| doc.get_first(field))
                .and_then(|value| value.as_str())
                .map(str::to_string),
            score,
        });
    }
    Ok(hits)
}

/// Fixed experimental profile for release/live-qrels evaluation only.
///
/// This is intentionally not parameterized by boosts and is not used by the
/// production query path, which remains pinned to `LexicalRankingProfile::V1`.
#[doc(hidden)]
pub fn search_with_lexical_profile_for_eval(
    active_path: &Path,
    query_text: &str,
    filters: &SearchFilters,
    profile: EvalLexicalProfile,
    limit: usize,
) -> Result<Vec<SearchHit>, QghError> {
    search_with_filters_profile(active_path, query_text, filters, profile.into(), limit)
}

#[doc(hidden)]
pub fn search_with_metadata_boost_v1_for_eval(
    active_path: &Path,
    query_text: &str,
    filters: &SearchFilters,
    limit: usize,
) -> Result<Vec<SearchHit>, QghError> {
    search_with_lexical_profile_for_eval(
        active_path,
        query_text,
        filters,
        EvalLexicalProfile::MetadataBoostV1,
        limit,
    )
}

#[cfg(test)]
struct LexicalProfileComparison {
    v1: Vec<SearchHit>,
    metadata_boost_v1: Vec<SearchHit>,
}

#[cfg(test)]
fn compare_lexical_profiles(
    active_path: &Path,
    query_text: &str,
    filters: &SearchFilters,
    limit: usize,
) -> Result<LexicalProfileComparison, QghError> {
    Ok(LexicalProfileComparison {
        v1: search_with_filters_profile(
            active_path,
            query_text,
            filters,
            LexicalRankingProfile::V1,
            limit,
        )?,
        metadata_boost_v1: search_with_metadata_boost_v1_for_eval(
            active_path,
            query_text,
            filters,
            limit,
        )?,
    })
}

fn filtered_query(
    text_query: Box<dyn Query>,
    fields: &FilterFields,
    filters: &SearchFilters,
) -> Box<dyn Query> {
    let mut clauses = vec![(Occur::Must, text_query)];
    push_source_type_filter(&mut clauses, fields, filters);
    if let Some(repo) = &filters.repo {
        clauses.push((Occur::Must, term_query(fields.repo, repo)));
    }
    if let Some(issue) = filters.issue {
        clauses.push((
            Occur::Must,
            term_query(fields.issue_number, &issue.to_string()),
        ));
    }
    if let Some(author) = &filters.author {
        clauses.push((Occur::Must, term_query(fields.author, author)));
    }
    if let Some(state) = &filters.state {
        clauses.push((Occur::Must, term_query(fields.state, state)));
    }
    if let Some(label_exact) = fields.label_exact {
        for label in &filters.labels {
            clauses.push((Occur::Must, term_query(label_exact, label)));
        }
    }
    if clauses.len() == 1 {
        return clauses.pop().expect("text query exists").1;
    }
    Box::new(BooleanQuery::new(clauses))
}

fn push_source_type_filter(
    clauses: &mut Vec<(Occur, Box<dyn Query>)>,
    fields: &FilterFields,
    filters: &SearchFilters,
) {
    let includes_issue = filters
        .source_types
        .iter()
        .any(|source_type| source_type == "issue");
    let includes_comment = filters
        .source_types
        .iter()
        .any(|source_type| source_type == "issue_comment");
    if includes_issue && includes_comment {
        return;
    }
    let source_type_terms = filters
        .source_types
        .iter()
        .map(|source_type| (Occur::Should, term_query(fields.entity_type, source_type)))
        .collect::<Vec<_>>();
    clauses.push((Occur::Must, Box::new(BooleanQuery::new(source_type_terms))));
}

fn term_query(field: Field, text: &str) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_text(field, text),
        IndexRecordOption::Basic,
    ))
}

struct FilterFields {
    entity_type: Field,
    repo: Field,
    issue_number: Field,
    state: Field,
    label_exact: Option<Field>,
    author: Field,
}

struct Fields {
    source_id: Field,
    entity_type: Field,
    repo: Field,
    issue_number: Field,
    state: Field,
    labels: Field,
    label_exact: Field,
    author: Field,
    title: Field,
    body: Field,
    parent_issue_title: Field,
    cjk_ngrams: Field,
    updated_at: Field,
    indexed_at: Field,
}

fn schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let source_id = builder.add_text_field("source_id", STRING | STORED);
    let entity_type = builder.add_text_field("entity_type", STRING | STORED);
    let repo = builder.add_text_field("repo", STRING | STORED);
    let issue_number = builder.add_text_field("issue_number", STRING | STORED);
    let state = builder.add_text_field("state", STRING | STORED);
    let labels = builder.add_text_field("labels", TEXT | STORED);
    let label_exact = builder.add_text_field("label_exact", STRING);
    let author = builder.add_text_field("author", STRING | STORED);
    let title = builder.add_text_field("title", TEXT | STORED);
    let body = builder.add_text_field("body", TEXT | STORED);
    let parent_issue_title = builder.add_text_field("parent_issue_title", TEXT | STORED);
    let cjk_ngrams = builder.add_text_field("cjk_ngrams", TEXT);
    let updated_at = builder.add_text_field("updated_at", STRING | STORED);
    let indexed_at = builder.add_text_field("indexed_at", STRING | STORED);
    (
        builder.build(),
        Fields {
            source_id,
            entity_type,
            repo,
            issue_number,
            state,
            labels,
            label_exact,
            author,
            title,
            body,
            parent_issue_title,
            cjk_ngrams,
            updated_at,
            indexed_at,
        },
    )
}

fn index_source_document(fields: &Fields, source: &IndexSource) -> TantivyDocument {
    let mut document = TantivyDocument::default();
    document.add_text(fields.source_id, &source.source_id);
    document.add_text(fields.entity_type, &source.entity_type);
    document.add_text(fields.repo, &source.repo);
    document.add_text(fields.issue_number, source.issue_number.to_string());
    document.add_text(fields.state, &source.state);
    document.add_text(fields.labels, source.labels.join(" "));
    for label in &source.labels {
        document.add_text(fields.label_exact, label);
    }
    document.add_text(fields.author, source.author.as_deref().unwrap_or_default());
    document.add_text(fields.title, &source.title);
    document.add_text(fields.body, &source.body);
    document.add_text(fields.parent_issue_title, &source.parent_issue_title);
    document.add_text(fields.cjk_ngrams, cjk_ngram_text(source));
    document.add_text(fields.updated_at, &source.github_updated_at);
    document.add_text(fields.indexed_at, &source.indexed_at);
    document
}

fn cjk_ngram_text(source: &IndexSource) -> String {
    cjk_ngrams(&format!(
        "{} {} {}",
        source.title, source.body, source.parent_issue_title
    ))
}

fn expand_cjk_query(query_text: &str) -> String {
    let ngrams = cjk_ngrams(query_text);
    if ngrams.is_empty() {
        query_text.to_string()
    } else {
        format!("{query_text} {ngrams}")
    }
}

fn cjk_ngrams(text: &str) -> String {
    let mut terms = Vec::new();
    let mut run = Vec::new();
    for ch in text.chars() {
        if is_cjk(ch) {
            run.push(ch);
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
        if run.len() < size {
            continue;
        }
        for window in run.windows(size) {
            terms.push(window.iter().collect());
        }
    }
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3040..=0x30ff | 0x3400..=0x9fff | 0xac00..=0xd7af
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IndexSource;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn search_rejects_missing_artifact_instead_of_returning_empty_results() {
        let index_root = temp_index_root("missing-search-artifact");
        let missing = index_root.join("generation-1");

        let error = search(&missing, "query-not-logged", 5).unwrap_err();
        assert_eq!(error.code, "index.failure");
        assert_eq!(error.message, "Tantivy index artifact is missing.");

        let _ = fs::remove_dir_all(index_root);
    }

    #[test]
    fn rebuild_uses_generation_path_and_warm_bm25_p95_stays_under_500ms() {
        let index_root = temp_index_root("bm25-performance");
        let sources = (0..10_000)
            .map(|number| IndexSource {
                source_id: format!("qgh://github.com/issue/NODE{number}"),
                entity_type: "issue".to_string(),
                repo: "owner/repo".to_string(),
                issue_number: number,
                state: "open".to_string(),
                labels: vec!["mvp".to_string()],
                author: Some("alice".to_string()),
                title: format!("Perf issue {number}"),
                body: format!("BM25 performance fixture body needle{number} sharedtoken"),
                parent_issue_title: String::new(),
                github_updated_at: "2026-01-01T00:00:00Z".to_string(),
                indexed_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .collect::<Vec<_>>();

        let generation_path = rebuild(&index_root, 1, &sources).unwrap();
        assert!(generation_path.ends_with("generation-1"));
        assert!(generation_path.exists());

        let cold_start = Instant::now();
        let cold_hits = search(&generation_path, "needle9999", 5).unwrap();
        let _cold_start_latency = cold_start.elapsed();
        assert_eq!(cold_hits[0].source_id, "qgh://github.com/issue/NODE9999");

        let mut warm_latencies = Vec::new();
        for _ in 0..20 {
            let started = Instant::now();
            let hits = search(&generation_path, "sharedtoken", 5).unwrap();
            warm_latencies.push(started.elapsed());
            assert!(!hits.is_empty());
        }
        warm_latencies.sort();
        let p95 = warm_latencies[(warm_latencies.len() * 95 / 100).min(warm_latencies.len() - 1)];
        assert!(
            p95 <= Duration::from_millis(500),
            "BM25 warm p95 exceeded 500ms: {p95:?}"
        );

        let _ = fs::remove_dir_all(index_root);
    }

    #[test]
    fn rebuild_fails_closed_without_deleting_a_preexisting_shadow() {
        let index_root = temp_index_root("foreign-shadow-collision");
        let shadow_path = index_root.join("shadow-1");
        fs::create_dir_all(&shadow_path).unwrap();
        let sentinel = shadow_path.join("user-backup");
        fs::write(&sentinel, "preserve").unwrap();

        let error = rebuild(&index_root, 1, &[]).unwrap_err();

        assert_eq!(error.code, "publication.tantivy_artifact_not_ready");
        assert!(sentinel.exists());
        assert!(!index_root.join("generation-1").exists());
        let _ = fs::remove_dir_all(index_root);
    }

    #[test]
    fn rebuild_fails_closed_without_deleting_a_preexisting_generation() {
        let index_root = temp_index_root("foreign-generation-collision");
        let generation_path = index_root.join("generation-2");
        fs::create_dir_all(&generation_path).unwrap();
        let sentinel = generation_path.join("user-backup");
        fs::write(&sentinel, "preserve").unwrap();

        let error = rebuild(&index_root, 2, &[]).unwrap_err();

        assert_eq!(error.code, "publication.tantivy_artifact_not_ready");
        assert!(sentinel.exists());
        assert!(!index_root.join("shadow-2").exists());
        let _ = fs::remove_dir_all(index_root);
    }

    #[test]
    fn cjk_ngram_fallback_matches_unsegmented_mixed_query() {
        let index_root = temp_index_root("cjk-ngram-fallback");
        let source = IndexSource {
            source_id: "qgh://github.com/issue/I_kwDOCJK1".to_string(),
            entity_type: "issue".to_string(),
            repo: "owner/repo".to_string(),
            issue_number: 77,
            state: "open".to_string(),
            labels: vec!["i18n".to_string()],
            author: Some("alice".to_string()),
            title: "OAuth 인증 토큰 만료".to_string(),
            body: "로그인 실패는 인증 토큰 갱신 누락 때문에 발생합니다.".to_string(),
            parent_issue_title: String::new(),
            github_updated_at: "2026-01-01T00:00:00Z".to_string(),
            indexed_at: "2026-01-01T00:00:00Z".to_string(),
        };

        let generation_path = rebuild(&index_root, 1, &[source]).unwrap();
        let hits = search(&generation_path, "인증토큰", 5).unwrap();

        assert_eq!(
            hits.first().map(|hit| hit.source_id.as_str()),
            Some("qgh://github.com/issue/I_kwDOCJK1")
        );
        assert_eq!(
            hits.first()
                .and_then(|hit| hit.source_updated_at.as_deref()),
            Some("2026-01-01T00:00:00Z")
        );
        let _ = fs::remove_dir_all(index_root);
    }

    #[test]
    fn search_filters_apply_before_top_docs_limit() {
        let index_root = temp_index_root("bm25-prefilter");
        let noisy_body = "needle ".repeat(50);
        let sources = vec![
            test_source(
                "NOISY_REPO",
                "other/repo",
                "open",
                "bob",
                &["ready-for-agent"],
                &noisy_body,
            ),
            test_source(
                "NOISY_LABEL",
                "owner/repo",
                "open",
                "bob",
                &["ready-for-human"],
                &noisy_body,
            ),
            test_source(
                "NOISY_LABEL_PARTS",
                "owner/repo",
                "open",
                "bob",
                &["ready", "for", "agent"],
                &noisy_body,
            ),
            test_source(
                "NOISY_STATE",
                "owner/repo",
                "closed",
                "bob",
                &["ready-for-agent"],
                &noisy_body,
            ),
            test_source(
                "NOISY_AUTHOR",
                "owner/repo",
                "open",
                "alice",
                &["ready-for-agent"],
                &noisy_body,
            ),
            test_source(
                "ALLOWED",
                "owner/repo",
                "open",
                "bob",
                &["ready-for-agent"],
                "needle",
            ),
        ];

        let generation_path = rebuild(&index_root, 1, &sources).unwrap();
        let hits = search_with_filters(
            &generation_path,
            "needle",
            &SearchFilters {
                repo: Some("owner/repo".to_string()),
                labels: vec!["ready-for-agent".to_string()],
                state: Some("open".to_string()),
                author: Some("bob".to_string()),
                issue: None,
                source_types: vec!["issue".to_string()],
            },
            1,
        )
        .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_id, "qgh://github.com/issue/ALLOWED");
        let _ = fs::remove_dir_all(index_root);
    }

    #[test]
    fn default_search_is_v1_and_profile_comparison_is_eval_only() {
        let index_root = temp_index_root("lexical-profile-isolation");
        let generation_path = rebuild(
            &index_root,
            1,
            &[
                test_source("ONE", "owner/repo", "open", "alice", &[], "needle"),
                test_source("TWO", "owner/repo", "open", "alice", &[], "needle needle"),
            ],
        )
        .unwrap();

        let default_hits =
            search_with_filters(&generation_path, "needle", &SearchFilters::default(), 5).unwrap();
        let comparison =
            compare_lexical_profiles(&generation_path, "needle", &SearchFilters::default(), 5)
                .unwrap();

        assert_eq!(LexicalRankingProfile::default(), LexicalRankingProfile::V1);
        assert_eq!(
            production_lexical_profile_for_eval(),
            EvalLexicalProfile::ProductionV1,
            "the live-eval identity must derive from the actual production default"
        );
        assert_eq!(
            source_ids(&default_hits),
            source_ids(&comparison.v1),
            "the production search interface must stay pinned to V1"
        );
        assert_eq!(comparison.v1.len(), comparison.metadata_boost_v1.len());
        let _ = fs::remove_dir_all(index_root);
    }

    #[test]
    fn metadata_boost_profile_improves_comment_parent_context_but_has_a_body_heavy_limit() {
        let index_root = temp_index_root("lexical-profile-comment-only");
        let cases = [
            ("ROLLBACK", "rollback recovery", "rollback recovery"),
            ("CACHE", "cache replay", "cache replay"),
            ("RACE", "publish race", "publish race"),
            (
                "REPEATED",
                "generation recovery",
                "generation recovery generation recovery",
            ),
        ];
        let total = cases.len();
        let sources = cases
            .iter()
            .flat_map(|(suffix, query, issue_body)| {
                [
                    profile_source(&format!("ISSUE_{suffix}"), "issue", issue_body, ""),
                    profile_source(
                        &format!("COMMENT_{suffix}"),
                        "issue_comment",
                        "The authoritative answer exists only in this comment.",
                        query,
                    ),
                ]
            })
            .collect::<Vec<_>>();
        let generation_path = rebuild(&index_root, 1, &sources).unwrap();
        let mut v1_top1 = 0;
        let mut metadata_top1 = 0;
        for (suffix, query, _) in cases {
            let gold_suffix = format!("COMMENT_{suffix}");
            let comparison =
                compare_lexical_profiles(&generation_path, query, &SearchFilters::default(), 6)
                    .unwrap();
            eprintln!(
                "lexical_profile_case query={query:?} v1={:?} metadata={:?}",
                ranked_sources(&comparison.v1),
                ranked_sources(&comparison.metadata_boost_v1)
            );
            v1_top1 += usize::from(top_source_has_suffix(&comparison.v1, &gold_suffix));
            metadata_top1 += usize::from(top_source_has_suffix(
                &comparison.metadata_boost_v1,
                &gold_suffix,
            ));
        }

        eprintln!(
            "lexical_profile_eval comment_only_top1 v1={v1_top1}/{total} metadata_boost_v1={metadata_top1}/{total}"
        );
        assert!(metadata_top1 > v1_top1);
        assert_eq!(metadata_top1, total - 1);
        let _ = fs::remove_dir_all(index_root);
    }

    #[test]
    fn label_filter_on_pre_label_exact_index_fails_with_actionable_resync_hint() {
        // Simulates an on-disk index built before label_exact existed
        // (pre-#55 schema): label filtering must not panic or surface a raw
        // Tantivy schema error — it must fail with a structured, actionable
        // error telling the user to resync, and unfiltered queries against
        // the same stale index must keep working (BM25-only stays complete).
        let index_root = temp_index_root("bm25-stale-schema-label-filter");
        let generation_path = index_root.join("generation-1");
        fs::create_dir_all(&generation_path).unwrap();

        let mut builder = Schema::builder();
        let source_id = builder.add_text_field("source_id", STRING | STORED);
        let entity_type = builder.add_text_field("entity_type", STRING | STORED);
        let repo = builder.add_text_field("repo", STRING | STORED);
        let issue_number = builder.add_text_field("issue_number", STRING | STORED);
        let state = builder.add_text_field("state", STRING | STORED);
        let labels = builder.add_text_field("labels", TEXT | STORED);
        let author = builder.add_text_field("author", STRING | STORED);
        let title = builder.add_text_field("title", TEXT | STORED);
        let body = builder.add_text_field("body", TEXT | STORED);
        let old_schema = builder.build();
        let index = Index::create_in_dir(&generation_path, old_schema).unwrap();
        let mut writer = index.writer(15_000_000).unwrap();
        let mut document = TantivyDocument::default();
        document.add_text(source_id, "qgh://github.com/issue/OLD_SCHEMA");
        document.add_text(entity_type, "issue");
        document.add_text(repo, "owner/repo");
        document.add_text(issue_number, "1");
        document.add_text(state, "open");
        document.add_text(labels, "ready-for-agent");
        document.add_text(author, "bob");
        document.add_text(title, "Pre-label_exact issue");
        document.add_text(body, "needle");
        writer.add_document(document).unwrap();
        writer.commit().unwrap();
        writer.wait_merging_threads().unwrap();

        let unfiltered = search(&generation_path, "needle", 5).unwrap();
        assert_eq!(
            unfiltered.len(),
            1,
            "stale index must keep serving BM25 queries without a label filter"
        );

        let error = search_with_filters(
            &generation_path,
            "needle",
            &SearchFilters {
                labels: vec!["ready-for-agent".to_string()],
                source_types: vec!["issue".to_string()],
                ..SearchFilters::default()
            },
            5,
        )
        .unwrap_err();
        assert_eq!(error.code, "validation.stale_index_label_filter");
        assert!(error.hint.is_some_and(|hint| hint.contains("qgh sync")));

        let _ = fs::remove_dir_all(index_root);
    }

    fn test_source(
        node_id: &str,
        repo: &str,
        state: &str,
        author: &str,
        labels: &[&str],
        body: &str,
    ) -> IndexSource {
        IndexSource {
            source_id: format!("qgh://github.com/issue/{node_id}"),
            entity_type: "issue".to_string(),
            repo: repo.to_string(),
            issue_number: 1,
            state: state.to_string(),
            labels: labels.iter().map(|label| label.to_string()).collect(),
            author: Some(author.to_string()),
            title: format!("Prefilter {node_id}"),
            body: body.to_string(),
            parent_issue_title: String::new(),
            github_updated_at: "2026-01-01T00:00:00Z".to_string(),
            indexed_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn source_ids(hits: &[SearchHit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.source_id.as_str()).collect()
    }

    fn top_source_has_suffix(hits: &[SearchHit], suffix: &str) -> bool {
        hits.first()
            .is_some_and(|hit| hit.source_id.ends_with(suffix))
    }

    fn ranked_sources(hits: &[SearchHit]) -> Vec<(&str, f32)> {
        hits.iter()
            .map(|hit| (hit.source_id.as_str(), hit.score))
            .collect()
    }

    fn profile_source(
        node_id: &str,
        entity_type: &str,
        body: &str,
        parent_issue_title: &str,
    ) -> IndexSource {
        let source_kind = match entity_type {
            "issue_comment" => "issue-comment",
            other => other,
        };
        IndexSource {
            source_id: format!("qgh://github.com/{source_kind}/{node_id}"),
            entity_type: entity_type.to_string(),
            repo: "owner/repo".to_string(),
            issue_number: 47,
            state: "open".to_string(),
            labels: Vec::new(),
            author: Some("alice".to_string()),
            title: String::new(),
            body: body.to_string(),
            parent_issue_title: parent_issue_title.to_string(),
            github_updated_at: "2026-01-01T00:00:00Z".to_string(),
            indexed_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn temp_index_root(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("qgh-index-{name}-{nanos}"));
        fs::create_dir_all(&root).unwrap();
        root
    }
}
