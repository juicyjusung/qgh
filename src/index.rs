use crate::error::QghError;
use crate::model::IndexSource;
use crate::paths::{ensure_private_dir, set_private_dir};
use std::fs;
use std::path::Path;
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
    active_path: &Path,
    generation: i64,
    sources: &[IndexSource],
) -> Result<(), QghError> {
    ensure_private_dir(index_root)?;
    let shadow_path = index_root.join(format!("shadow-{generation}"));
    if shadow_path.exists() {
        fs::remove_dir_all(&shadow_path)?;
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
    if active_path.exists() {
        fs::remove_dir_all(active_path)?;
    }
    fs::rename(&shadow_path, active_path)?;
    set_private_dir(active_path)?;
    Ok(())
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
    let parser = QueryParser::for_index(&index, query_fields);
    let query = parser.parse_query(query_text).map_err(|e| {
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
            updated_at,
            indexed_at,
        },
    )
}
