use crate::error::QghError;
use crate::model::IndexSource;
use crate::paths::{ensure_private_dir, set_private_dir};
use std::fs;
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, Value, STORED, STRING, TEXT};
use tantivy::{doc, Index, TantivyDocument};

pub struct SearchHit {
    pub source_id: String,
    pub score: f32,
}

pub fn rebuild(
    index_root: &Path,
    generation: i64,
    sources: &[IndexSource],
) -> Result<PathBuf, QghError> {
    ensure_private_dir(index_root)?;
    let shadow_path = index_root.join(format!("shadow-{generation}"));
    let generation_path = index_root.join(format!("generation-{generation}"));
    if shadow_path.exists() {
        fs::remove_dir_all(&shadow_path)?;
    }
    if generation_path.exists() {
        fs::remove_dir_all(&generation_path)?;
    }
    ensure_private_dir(&shadow_path)?;
    let (schema, fields) = schema();
    let index =
        Index::create_in_dir(&shadow_path, schema).map_err(|e| QghError::index(e.to_string()))?;
    let mut writer = index
        .writer(50_000_000)
        .map_err(|e| QghError::index(e.to_string()))?;
    for source in sources {
        writer
            .add_document(doc!(
                fields.source_id => source.source_id.clone(),
                fields.entity_type => source.entity_type.clone(),
                fields.repo => source.repo.clone(),
                fields.issue_number => source.issue_number.to_string(),
                fields.state => source.state.clone(),
                fields.labels => source.labels.join(" "),
                fields.author => source.author.clone().unwrap_or_default(),
                fields.title => source.title.clone(),
                fields.body => source.body.clone(),
                fields.parent_issue_title => source.parent_issue_title.clone(),
                fields.cjk_ngrams => cjk_ngram_text(source),
                fields.updated_at => source.github_updated_at.clone(),
                fields.indexed_at => source.indexed_at.clone(),
            ))
            .map_err(|e| QghError::index(e.to_string()))?;
    }
    writer
        .commit()
        .map_err(|e| QghError::index(e.to_string()))?;
    writer
        .wait_merging_threads()
        .map_err(|e| QghError::index(e.to_string()))?;
    fs::rename(&shadow_path, &generation_path)?;
    set_private_dir(&generation_path)?;
    Ok(generation_path)
}

pub fn search(
    active_path: &Path,
    query_text: &str,
    limit: usize,
) -> Result<Vec<SearchHit>, QghError> {
    if !active_path.exists() {
        return Ok(Vec::new());
    }
    let index = Index::open_in_dir(active_path).map_err(|e| QghError::index(e.to_string()))?;
    let schema = index.schema();
    let source_id = schema
        .get_field("source_id")
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
    let repo = schema
        .get_field("repo")
        .map_err(|e| QghError::index(e.to_string()))?;
    let issue_number = schema
        .get_field("issue_number")
        .map_err(|e| QghError::index(e.to_string()))?;
    let reader = index.reader().map_err(|e| QghError::index(e.to_string()))?;
    let searcher = reader.searcher();
    let mut query_fields = vec![title, body, labels, repo, issue_number];
    if let Ok(parent_issue_title) = schema.get_field("parent_issue_title") {
        query_fields.push(parent_issue_title);
    }
    if let Ok(cjk_ngrams) = schema.get_field("cjk_ngrams") {
        query_fields.push(cjk_ngrams);
    }
    let parser = QueryParser::for_index(&index, query_fields);
    let expanded_query = expand_cjk_query(query_text);
    let query = parser.parse_query(&expanded_query).map_err(|e| {
        QghError::validation("validation.invalid_query", format!("Invalid query: {e}"))
    })?;
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
            score,
        });
    }
    Ok(hits)
}

struct Fields {
    source_id: Field,
    entity_type: Field,
    repo: Field,
    issue_number: Field,
    state: Field,
    labels: Field,
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
        let _ = fs::remove_dir_all(index_root);
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
