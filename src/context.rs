use sha2::{Digest, Sha256};

/// Exact metadata-context template persisted with every production embedding.
pub const METADATA_CONTEXT_TEMPLATE_VERSION: &str = "qgh.context.v1";

/// Immutable source metadata allowed in contextual document embeddings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingSourceContext<'a> {
    Issue {
        repository: &'a str,
        issue_number: i64,
        title: &'a str,
    },
    Comment {
        repository: &'a str,
        parent_issue_number: i64,
        parent_issue_title: &'a str,
    },
}

/// Contextual text used only for embedding input and its generation hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedEmbeddingInput {
    text: String,
}

impl PreparedEmbeddingInput {
    pub fn context_template_version(&self) -> &'static str {
        METADATA_CONTEXT_TEMPLATE_VERSION
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn context_hash(&self, model_manifest_hash: &str, chunker_fingerprint: &str) -> String {
        embedding_context_hash(
            model_manifest_hash,
            chunker_fingerprint,
            self.context_template_version(),
            self.as_str(),
        )
    }
}

/// Build the deterministic prefix without changing the authoritative chunk.
pub fn prepare_embedding_input(
    context: EmbeddingSourceContext<'_>,
    chunk: &str,
) -> PreparedEmbeddingInput {
    let prefix = match context {
        EmbeddingSourceContext::Issue {
            repository,
            issue_number,
            title,
        } => format!("Repository: {repository}\nIssue #{issue_number}: {title}"),
        EmbeddingSourceContext::Comment {
            repository,
            parent_issue_number,
            parent_issue_title,
        } => format!(
            "Repository: {repository}\nComment on issue #{parent_issue_number}: {parent_issue_title}"
        ),
    };
    PreparedEmbeddingInput {
        text: format!("{prefix}\n\n{chunk}"),
    }
}

pub fn embedding_context_hash(
    model_manifest_hash: &str,
    chunker_fingerprint: &str,
    context_template_version: &str,
    embedding_input: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model_manifest_hash.as_bytes());
    hasher.update([0]);
    hasher.update(chunker_fingerprint.as_bytes());
    hasher.update([0]);
    hasher.update(context_template_version.as_bytes());
    hasher.update([0]);
    hasher.update(embedding_input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
