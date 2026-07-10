use crate::error::QghError;
use crate::model::IndexSource;
use crate::paths::{ensure_private_dir, set_private_dir};
use std::fs;
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value, STORED, STRING, TEXT};
use tantivy::{Index, TantivyDocument, Term};

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub source_id: String,
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
pub enum LexicalRankingProfile {
    #[default]
    V1,
    MetadataBoostV1,
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
            .add_document(index_source_document(&fields, source))
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

pub fn search_with_filters_profile(
    active_path: &Path,
    query_text: &str,
    filters: &SearchFilters,
    profile: LexicalRankingProfile,
    limit: usize,
) -> Result<Vec<SearchHit>, QghError> {
    if limit == 0 || filters.source_types.is_empty() {
        return Ok(Vec::new());
    }
    if !active_path.exists() {
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
        parser.set_field_boost(labels, 1.5);
        parser.set_field_boost(issue_number, 1.5);
        if let Ok(parent_issue_title) = schema.get_field("parent_issue_title") {
            parser.set_field_boost(parent_issue_title, 1.5);
        }
    }
    let expanded_query = expand_cjk_query(query_text);
    let query = parser.parse_query(&expanded_query).map_err(|e| {
        QghError::validation("validation.invalid_query", format!("Invalid query: {e}"))
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
            score,
        });
    }
    Ok(hits)
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
