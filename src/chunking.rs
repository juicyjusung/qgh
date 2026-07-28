use crate::embedding::{EmbeddingProviderError, EmbeddingTokenizer, TokenSpan};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt;

pub const CHUNKER_VERSION: &str = "markdown-token-v4";
pub const CHUNKER_FINGERPRINT: &str =
    "markdown-token-v4:source-markdown-boundaries-forward-progress-overlap";
pub const DEFAULT_TARGET_TOKENS: usize = 900;
pub const DEFAULT_BOUNDARY_SEARCH_TOKENS: usize = 200;
pub const DEFAULT_OVERLAP_TOKENS: usize = 135;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkerConfig {
    pub target_tokens: usize,
    pub overlap_tokens: usize,
    pub boundary_search_tokens: usize,
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self {
            target_tokens: DEFAULT_TARGET_TOKENS,
            overlap_tokens: DEFAULT_OVERLAP_TOKENS,
            boundary_search_tokens: DEFAULT_BOUNDARY_SEARCH_TOKENS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownChunk {
    pub chunk_index: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub token_start: usize,
    pub token_end: usize,
    pub token_count: usize,
    pub body: String,
    pub chunker_version: String,
    pub chunker_fingerprint: String,
    pub heading_path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkingError {
    message: String,
}

impl ChunkingError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ChunkingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ChunkingError {}

impl From<EmbeddingProviderError> for ChunkingError {
    fn from(value: EmbeddingProviderError) -> Self {
        Self::new(value.to_string())
    }
}

#[derive(Debug, Clone, Copy)]
struct TokenRange {
    start: usize,
    end: usize,
}

pub fn chunk_markdown(
    text: &str,
    tokenizer: &(impl EmbeddingTokenizer + ?Sized),
) -> Result<Vec<MarkdownChunk>, ChunkingError> {
    chunk_markdown_with_config_and_fingerprint(
        text,
        tokenizer,
        ChunkerConfig::default(),
        CHUNKER_FINGERPRINT,
    )
}

pub fn chunk_markdown_with_tokenizer_identity(
    text: &str,
    tokenizer: &(impl EmbeddingTokenizer + ?Sized),
    tokenizer_identity: &str,
) -> Result<Vec<MarkdownChunk>, ChunkingError> {
    let fingerprint = chunker_fingerprint_for_tokenizer_identity(tokenizer_identity);
    chunk_markdown_with_fingerprint(text, tokenizer, &fingerprint)
}

pub fn chunk_markdown_with_fingerprint(
    text: &str,
    tokenizer: &(impl EmbeddingTokenizer + ?Sized),
    fingerprint: &str,
) -> Result<Vec<MarkdownChunk>, ChunkingError> {
    chunk_markdown_with_config_and_fingerprint(
        text,
        tokenizer,
        ChunkerConfig::default(),
        fingerprint,
    )
}

pub fn chunker_fingerprint_for_tokenizer_identity(tokenizer_identity: &str) -> String {
    let mut hasher = Sha256::new();
    for field in [
        "qgh.chunker_contract.v1",
        CHUNKER_VERSION,
        CHUNKER_FINGERPRINT,
        tokenizer_identity,
    ] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    format!("{CHUNKER_VERSION}:{}", hex_digest(&hasher.finalize()))
}

pub fn chunk_markdown_with_config(
    text: &str,
    tokenizer: &(impl EmbeddingTokenizer + ?Sized),
    config: ChunkerConfig,
) -> Result<Vec<MarkdownChunk>, ChunkingError> {
    chunk_markdown_with_config_and_fingerprint(text, tokenizer, config, CHUNKER_FINGERPRINT)
}

fn chunk_markdown_with_config_and_fingerprint(
    text: &str,
    tokenizer: &(impl EmbeddingTokenizer + ?Sized),
    config: ChunkerConfig,
    fingerprint: &str,
) -> Result<Vec<MarkdownChunk>, ChunkingError> {
    if config.target_tokens == 0 {
        return Err(ChunkingError::new(
            "target_tokens must be greater than zero.",
        ));
    }

    // Chunk against canonical text while retaining exact source spans for
    // match-aware previews.
    let tokenized = tokenizer.tokenize_canonical(text)?;
    if tokenized.spans.len() != tokenized.original_spans.len() {
        return Err(ChunkingError::new(
            "tokenizer did not provide one original span per normalized token.",
        ));
    }
    let text = tokenized.text.as_str();
    let original_text = tokenized.original_text.as_str();
    let tokens = tokenized.spans;
    let original_tokens = tokenized.original_spans;
    validate_token_spans(text, &tokens)?;
    validate_token_spans(original_text, &original_tokens)?;
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let boundary_token_indexes = markdown_boundary_token_indexes(original_text, &original_tokens);
    let code_fence_token_ranges = code_fence_token_ranges(original_text, &original_tokens);
    let mut chunks = Vec::new();
    let mut token_start = 0;

    while token_start < tokens.len() {
        let token_end = choose_token_end(
            token_start,
            tokens.len(),
            &boundary_token_indexes,
            &code_fence_token_ranges,
            config,
        );
        let byte_start = original_tokens[token_start].start;
        let byte_end = original_tokens[token_end - 1].end;
        if byte_start >= byte_end
            || byte_end > original_text.len()
            || !original_text.is_char_boundary(byte_start)
            || !original_text.is_char_boundary(byte_end)
        {
            return Err(ChunkingError::new(
                "tokenizer produced an unmappable original UTF-8 span.",
            ));
        }
        chunks.push(MarkdownChunk {
            chunk_index: chunks.len(),
            byte_start,
            byte_end,
            token_start,
            token_end,
            token_count: token_end - token_start,
            body: original_text[byte_start..byte_end].to_string(),
            chunker_version: CHUNKER_VERSION.to_string(),
            chunker_fingerprint: fingerprint.to_string(),
            heading_path: heading_path(original_text, byte_start),
        });

        if token_end == tokens.len() {
            break;
        }

        let overlap_tokens = config.overlap_tokens.min(token_end - token_start - 1);
        let mut next_start = token_end.saturating_sub(overlap_tokens);
        next_start = adjust_start_out_of_code_fence(next_start, &code_fence_token_ranges);
        // Re-entering the prefix before the same fence can reduce progress to
        // one token per chunk when the prefix is no longer than the overlap.
        if next_start < token_end
            && code_fence_token_ranges
                .iter()
                .any(|range| range.start == token_end)
        {
            next_start = token_end;
        }
        if next_start <= token_start {
            next_start = token_end;
        }
        token_start = next_start;
    }

    Ok(chunks)
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_token_spans(text: &str, tokens: &[TokenSpan]) -> Result<(), ChunkingError> {
    let mut previous_end = 0;
    for token in tokens {
        if token.start >= token.end {
            return Err(ChunkingError::new(
                "tokenizer returned an empty token span.",
            ));
        }
        if token.end > text.len() {
            return Err(ChunkingError::new(
                "tokenizer returned a token span beyond input length.",
            ));
        }
        if token.start < previous_end {
            return Err(ChunkingError::new(
                "tokenizer returned overlapping token spans.",
            ));
        }
        if !text.is_char_boundary(token.start) || !text.is_char_boundary(token.end) {
            return Err(ChunkingError::new(
                "tokenizer returned non-character-boundary token spans.",
            ));
        }
        previous_end = token.end;
    }
    Ok(())
}

fn choose_token_end(
    token_start: usize,
    token_count: usize,
    boundary_token_indexes: &[usize],
    code_fence_token_ranges: &[TokenRange],
    config: ChunkerConfig,
) -> usize {
    if token_start + config.target_tokens >= token_count {
        return token_count;
    }

    let target = token_start + config.target_tokens;
    let search_start = token_start
        + config
            .target_tokens
            .saturating_sub(config.boundary_search_tokens);
    let search_end = (target + config.boundary_search_tokens).min(token_count);
    let mut selected = None;
    let mut selected_distance = usize::MAX;

    for candidate in boundary_token_indexes {
        if *candidate <= token_start || *candidate < search_start || *candidate > search_end {
            continue;
        }
        let distance = candidate.abs_diff(target);
        if distance < selected_distance
            || (distance == selected_distance
                && selected.is_none_or(|current| *candidate < current))
        {
            selected = Some(*candidate);
            selected_distance = distance;
        }
    }

    let proposed = selected.unwrap_or(target);
    adjust_end_out_of_code_fence(token_start, proposed, token_count, code_fence_token_ranges)
}

fn adjust_end_out_of_code_fence(
    token_start: usize,
    proposed: usize,
    token_count: usize,
    code_fence_token_ranges: &[TokenRange],
) -> usize {
    let mut adjusted = proposed;
    loop {
        let mut changed = false;
        for range in code_fence_token_ranges {
            if range.start < adjusted && adjusted < range.end {
                adjusted = if range.start > token_start {
                    range.start
                } else {
                    range.end.min(token_count)
                };
                changed = true;
                break;
            }
        }
        if !changed {
            break;
        }
    }
    adjusted.max(token_start + 1).min(token_count)
}

fn adjust_start_out_of_code_fence(
    proposed: usize,
    code_fence_token_ranges: &[TokenRange],
) -> usize {
    for range in code_fence_token_ranges {
        if range.start < proposed && proposed < range.end {
            return range.start;
        }
    }
    proposed
}

fn markdown_boundary_token_indexes(text: &str, tokens: &[TokenSpan]) -> Vec<usize> {
    let mut offsets = vec![0, text.len()];
    for boundary in markdown_boundary_offsets(text) {
        offsets.push(boundary);
    }
    offsets.sort_unstable();
    offsets.dedup();

    let mut token_indexes = offsets
        .into_iter()
        .map(|offset| token_index_at_byte(tokens, offset))
        .collect::<Vec<_>>();
    token_indexes.sort_unstable();
    token_indexes.dedup();
    token_indexes
}

fn markdown_boundary_offsets(text: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut line_start = 0;
    let mut open_fence: Option<FenceMarker> = None;

    for line in text.split_inclusive('\n') {
        let line_end = line_start + line.len();
        let line_without_newline = line.trim_end_matches(['\r', '\n']);
        if let Some(marker) = fence_marker(line_without_newline) {
            offsets.push(line_start);
            offsets.push(line_end);
            match open_fence {
                Some(open) if open.matches(marker) => open_fence = None,
                None => open_fence = Some(marker),
                _ => {}
            }
        } else if open_fence.is_none() {
            if is_heading(line_without_newline) {
                offsets.push(line_start);
                offsets.push(line_end);
            }
            if line_without_newline.trim().is_empty() {
                offsets.push(line_end);
            }
        }
        line_start = line_end;
    }

    offsets
}

fn code_fence_token_ranges(text: &str, tokens: &[TokenSpan]) -> Vec<TokenRange> {
    let mut ranges = Vec::new();
    let mut line_start = 0;
    let mut open_fence: Option<(FenceMarker, usize)> = None;

    for line in text.split_inclusive('\n') {
        let line_end = line_start + line.len();
        let line_without_newline = line.trim_end_matches(['\r', '\n']);
        if let Some(marker) = fence_marker(line_without_newline) {
            match open_fence {
                Some((open, fence_start)) if open.matches(marker) => {
                    ranges.push(TokenRange {
                        start: token_index_at_byte(tokens, fence_start),
                        end: token_index_at_byte(tokens, line_end),
                    });
                    open_fence = None;
                }
                None => open_fence = Some((marker, line_start)),
                _ => {}
            }
        }
        line_start = line_end;
    }

    if let Some((_, fence_start)) = open_fence {
        ranges.push(TokenRange {
            start: token_index_at_byte(tokens, fence_start),
            end: tokens.len(),
        });
    }

    ranges
        .into_iter()
        .filter(|range| range.start < range.end)
        .collect()
}

fn token_index_at_byte(tokens: &[TokenSpan], offset: usize) -> usize {
    tokens
        .iter()
        .position(|token| token.start >= offset)
        .unwrap_or(tokens.len())
}

#[derive(Debug, Clone, Copy)]
struct FenceMarker {
    marker: char,
    length: usize,
}

impl FenceMarker {
    fn matches(self, other: FenceMarker) -> bool {
        self.marker == other.marker && other.length >= self.length
    }
}

fn fence_marker(line: &str) -> Option<FenceMarker> {
    let leading_spaces = line
        .chars()
        .take_while(|character| *character == ' ')
        .count();
    if leading_spaces > 3 {
        return None;
    }
    let trimmed = &line[leading_spaces..];
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let length = trimmed
        .chars()
        .take_while(|character| *character == marker)
        .count();
    (length >= 3).then_some(FenceMarker { marker, length })
}

fn is_heading(line: &str) -> bool {
    let trimmed = line.trim_start();
    let level = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    (1..=6).contains(&level)
        && trimmed
            .chars()
            .nth(level)
            .is_some_and(|character| character.is_whitespace())
}

fn heading_path(text: &str, byte_offset: usize) -> Vec<String> {
    let mut path: Vec<(usize, String)> = Vec::new();
    let prefix = text.get(..byte_offset).unwrap_or(text);
    for line in prefix.lines() {
        let trimmed = line.trim_start();
        let level = trimmed
            .chars()
            .take_while(|character| *character == '#')
            .count();
        if !(1..=6).contains(&level)
            || !trimmed
                .chars()
                .nth(level)
                .is_some_and(|character| character.is_whitespace())
        {
            continue;
        }
        while path.last().is_some_and(|(current, _)| *current >= level) {
            path.pop();
        }
        let title = trimmed[level..].trim().to_string();
        path.push((level, title));
    }
    path.into_iter().map(|(_, title)| title).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::TokenizedText;

    struct WhitespaceTokenizer;

    impl EmbeddingTokenizer for WhitespaceTokenizer {
        fn tokenize(&self, text: &str) -> Result<Vec<TokenSpan>, EmbeddingProviderError> {
            let mut tokens = Vec::new();
            let mut token_start = None;
            for (index, character) in text.char_indices() {
                if character.is_whitespace() {
                    if let Some(start) = token_start.take() {
                        tokens.push(TokenSpan { start, end: index });
                    }
                } else if token_start.is_none() {
                    token_start = Some(index);
                }
            }
            if let Some(start) = token_start {
                tokens.push(TokenSpan {
                    start,
                    end: text.len(),
                });
            }
            Ok(tokens)
        }
    }

    struct NewlineCollapsingTokenizer;

    impl NewlineCollapsingTokenizer {
        fn canonicalize(text: &str) -> String {
            text.split('\n')
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        }
    }

    impl EmbeddingTokenizer for NewlineCollapsingTokenizer {
        fn tokenize(&self, text: &str) -> Result<Vec<TokenSpan>, EmbeddingProviderError> {
            Ok(self.tokenize_canonical(text)?.spans)
        }

        fn tokenize_canonical(&self, text: &str) -> Result<TokenizedText, EmbeddingProviderError> {
            let canonical = Self::canonicalize(text);
            let spans = WhitespaceTokenizer.tokenize(&canonical)?;
            let mut cursor = 0;
            let original_spans = spans
                .iter()
                .map(|span| {
                    let token = &canonical[span.start..span.end];
                    let relative = text[cursor..].find(token)?;
                    let start = cursor + relative;
                    let end = start + token.len();
                    cursor = end;
                    Some(TokenSpan { start, end })
                })
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    EmbeddingProviderError::structured(
                        "embedding.tokenizer_unmappable_offset",
                        "fixture tokenizer could not map normalized token to source",
                    )
                })?;
            Ok(TokenizedText {
                text: canonical,
                spans,
                original_text: text.to_string(),
                original_spans,
            })
        }
    }

    #[test]
    fn chunks_korean_english_and_mixed_markdown_on_boundaries_with_overlap_and_size() {
        let text = "# 한국어 English\n\nko01 ko02 ko03 ko04 ko05 ko06 ko07 ko08\n\n## Mixed\n\n한글09 en10 한글11 en12 한글13 en14 한글15 en16\n\n## English\n\nen17 en18 en19 en20 en21 en22 en23 en24";
        let config = ChunkerConfig {
            target_tokens: 10,
            overlap_tokens: 2,
            boundary_search_tokens: 4,
        };

        let chunks = chunk_markdown_with_config(text, &WhitespaceTokenizer, config).unwrap();

        assert!(chunks.len() >= 3);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.token_count
                    <= config.target_tokens + config.boundary_search_tokens)
        );
        for adjacent in chunks.windows(2) {
            assert_eq!(adjacent[1].token_start, adjacent[0].token_end - 2);
        }
        assert!(chunks[0].body.contains("한국어 English"));
        assert!(chunks
            .iter()
            .any(|chunk| chunk.body.contains("한글09 en10")));
        assert!(chunks.iter().any(|chunk| chunk.body.contains("en23 en24")));
        assert!(chunks[0].byte_end <= text.find("## Mixed").unwrap());
    }

    #[test]
    fn keeps_code_fences_atomic_when_target_lands_inside_fence() {
        let text = "intro01 intro02 intro03 intro04\n\n```rust\nfn main() {\n    println!(\"x\");\n}\n```\n\ntail01 tail02 tail03 tail04 tail05 tail06";
        let config = ChunkerConfig {
            target_tokens: 7,
            overlap_tokens: 2,
            boundary_search_tokens: 1,
        };

        let chunks = chunk_markdown_with_config(text, &WhitespaceTokenizer, config).unwrap();

        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            let fence_marker_count = chunk.body.matches("```").count();
            assert_ne!(
                fence_marker_count, 1,
                "chunk split a code fence in the middle: {chunk:?}"
            );
        }
        assert!(chunks.iter().any(|chunk| {
            chunk.body.contains("```rust") && chunk.body.contains("println!(\"x\");")
        }));
    }

    #[test]
    fn short_prefix_before_code_fence_has_bounded_chunk_count() {
        let text = "pre00 pre01 pre02 pre03\n\n```text\nfence00 fence01 fence02 fence03 fence04 fence05 fence06 fence07\n```\n\ntail00 tail01";
        let config = ChunkerConfig {
            target_tokens: 6,
            overlap_tokens: 4,
            boundary_search_tokens: 0,
        };

        let chunks = chunk_markdown_with_config(text, &WhitespaceTokenizer, config).unwrap();

        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn pathological_fence_shapes_are_atomic_bounded_and_deterministic() {
        fn tokens(prefix: &str, count: usize) -> String {
            (0..count)
                .map(|index| format!("{prefix}{index:05}"))
                .collect::<Vec<_>>()
                .join(" ")
        }

        fn fenced_shape(prefix_tokens: usize, fence_tokens: usize, tail_tokens: usize) -> String {
            format!(
                "{}\n\n```text\n{}\n```\n\n{}",
                tokens("pre", prefix_tokens),
                tokens("fence", fence_tokens),
                tokens("tail", tail_tokens)
            )
        }

        for text in [fenced_shape(135, 1_062, 24), fenced_shape(56, 6_188, 23)] {
            let first =
                chunk_markdown_with_config(&text, &WhitespaceTokenizer, ChunkerConfig::default())
                    .unwrap();
            let retry =
                chunk_markdown_with_config(&text, &WhitespaceTokenizer, ChunkerConfig::default())
                    .unwrap();

            assert_eq!(first, retry);
            assert_eq!(first.len(), 3);
            for chunk in &first {
                assert_eq!(chunk.body, text[chunk.byte_start..chunk.byte_end]);
                assert_ne!(chunk.body.matches("```").count(), 1);
            }
            for adjacent in first.windows(2) {
                assert!(adjacent[1].token_start > adjacent[0].token_start + 1);
            }
        }
    }

    #[test]
    fn chunk_size_uses_tokenizer_spans_not_character_count() {
        struct WholeInputTokenizer;

        impl EmbeddingTokenizer for WholeInputTokenizer {
            fn tokenize(&self, text: &str) -> Result<Vec<TokenSpan>, EmbeddingProviderError> {
                Ok(vec![TokenSpan {
                    start: 0,
                    end: text.len(),
                }])
            }
        }

        let text = "한국어".repeat(1_200);
        let config = ChunkerConfig {
            target_tokens: 2,
            overlap_tokens: 1,
            boundary_search_tokens: 0,
        };

        let chunks = chunk_markdown_with_config(&text, &WholeInputTokenizer, config).unwrap();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].token_count, 1);
        assert_eq!(chunks[0].body, text);
    }

    #[test]
    fn tokenizer_identity_changes_chunk_fingerprint_without_mutating_body() {
        let source = "raw body must remain byte-for-byte stable";
        let arctic = chunk_markdown_with_tokenizer_identity(
            source,
            &WhitespaceTokenizer,
            "arctic-tokenizer-contract",
        )
        .unwrap();
        let gte = chunk_markdown_with_tokenizer_identity(
            source,
            &WhitespaceTokenizer,
            "gte-tokenizer-contract",
        )
        .unwrap();
        let arctic_again = chunk_markdown_with_tokenizer_identity(
            source,
            &WhitespaceTokenizer,
            "arctic-tokenizer-contract",
        )
        .unwrap();

        assert_eq!(arctic[0].body, source);
        assert_eq!(gte[0].body, source);
        assert_ne!(arctic[0].chunker_fingerprint, gte[0].chunker_fingerprint);
        assert_eq!(
            arctic[0].chunker_fingerprint,
            arctic_again[0].chunker_fingerprint
        );
        assert_eq!(
            arctic[0].chunker_fingerprint,
            chunker_fingerprint_for_tokenizer_identity("arctic-tokenizer-contract")
        );
    }

    #[test]
    fn semantic_chunker_revision_invalidates_existing_generation_identity() {
        assert_eq!(CHUNKER_VERSION, "markdown-token-v4");
        assert_eq!(
            CHUNKER_FINGERPRINT,
            "markdown-token-v4:source-markdown-boundaries-forward-progress-overlap"
        );
        assert!(
            chunker_fingerprint_for_tokenizer_identity("fixture-tokenizer")
                .starts_with("markdown-token-v4:")
        );
        assert_eq!(crate::embedding::CHUNKER_VERSION, "qgh.chunker.v3");
    }

    #[test]
    fn rejects_invalid_tokenizer_spans() {
        struct InvalidTokenizer;

        impl EmbeddingTokenizer for InvalidTokenizer {
            fn tokenize(&self, _text: &str) -> Result<Vec<TokenSpan>, EmbeddingProviderError> {
                Ok(vec![TokenSpan { start: 2, end: 1 }])
            }
        }

        let error = chunk_markdown("abc", &InvalidTokenizer).unwrap_err();

        assert!(error.to_string().contains("empty token span"));
    }

    #[test]
    fn chunks_against_tokenizer_canonical_text_when_spans_target_normalized_form() {
        // Simulates an SPM-style normalizer that collapses newline runs into
        // single spaces: spans are only valid against the normalized text,
        // never the raw input — the shape of the fastembed XLM-R tokenizer.
        let text = "# 한국어 제목\n\n스토리지 리뷰 지적사항 반영 완료 본문입니다\n\n```swift\nlet key = contentKey\n```";
        let config = ChunkerConfig {
            target_tokens: 4,
            overlap_tokens: 1,
            boundary_search_tokens: 1,
        };

        let chunks = chunk_markdown_with_config(text, &NewlineCollapsingTokenizer, config).unwrap();

        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert_eq!(chunk.body, text[chunk.byte_start..chunk.byte_end]);
        }
        assert!(chunks.iter().any(|chunk| chunk.body.contains("지적사항")));
        assert_eq!(chunks.last().unwrap().byte_end, text.len());
        for chunk in &chunks {
            assert_ne!(
                chunk.body.matches("```").count(),
                1,
                "chunk split a source Markdown fence after canonical normalization: {chunk:?}"
            );
        }
        assert!(chunks.iter().any(|chunk| {
            chunk.body.contains("```swift") && chunk.body.contains("let key = contentKey")
        }));
    }

    #[test]
    fn preserves_source_heading_boundaries_when_canonical_text_collapses_newlines() {
        let text = "# 한국어 제목\n\nparagraph01 paragraph02 paragraph03 paragraph04 paragraph05";
        let chunks = chunk_markdown_with_config(
            text,
            &NewlineCollapsingTokenizer,
            ChunkerConfig {
                target_tokens: 4,
                overlap_tokens: 0,
                boundary_search_tokens: 1,
            },
        )
        .unwrap();

        assert_eq!(chunks[0].body, "# 한국어 제목");
        assert_eq!(chunks[1].heading_path, vec!["한국어 제목"]);

        let paragraphs = "one01 one02 one03\n\ntwo01 two02 two03 two04 two05";
        let paragraph_chunks = chunk_markdown_with_config(
            paragraphs,
            &NewlineCollapsingTokenizer,
            ChunkerConfig {
                target_tokens: 4,
                overlap_tokens: 0,
                boundary_search_tokens: 1,
            },
        )
        .unwrap();
        assert_eq!(paragraph_chunks[0].body, "one01 one02 one03");
    }

    #[test]
    fn chunks_keep_original_utf8_span_when_tokenizer_normalizes_whitespace() {
        struct CollapsingTokenizer;

        impl EmbeddingTokenizer for CollapsingTokenizer {
            fn tokenize(&self, text: &str) -> Result<Vec<TokenSpan>, EmbeddingProviderError> {
                Ok(self.tokenize_canonical(text)?.spans)
            }

            fn tokenize_canonical(
                &self,
                text: &str,
            ) -> Result<TokenizedText, EmbeddingProviderError> {
                Ok(TokenizedText {
                    text: "alpha beta".to_string(),
                    spans: vec![
                        TokenSpan { start: 0, end: 5 },
                        TokenSpan { start: 6, end: 10 },
                    ],
                    original_text: text.to_string(),
                    original_spans: vec![
                        TokenSpan { start: 0, end: 5 },
                        TokenSpan { start: 8, end: 12 },
                    ],
                })
            }
        }

        let source = "alpha\n\n beta";
        let chunks = chunk_markdown_with_config(
            source,
            &CollapsingTokenizer,
            ChunkerConfig {
                target_tokens: 2,
                overlap_tokens: 0,
                boundary_search_tokens: 0,
            },
        )
        .unwrap();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].byte_start, 0);
        assert_eq!(chunks[0].byte_end, source.len());
        assert_eq!(chunks[0].body, source);
    }

    #[test]
    fn rejects_unmappable_normalized_span_instead_of_storing_approximation() {
        struct BrokenTokenizer;

        impl EmbeddingTokenizer for BrokenTokenizer {
            fn tokenize(&self, text: &str) -> Result<Vec<TokenSpan>, EmbeddingProviderError> {
                Ok(self.tokenize_canonical(text)?.spans)
            }

            fn tokenize_canonical(
                &self,
                _text: &str,
            ) -> Result<TokenizedText, EmbeddingProviderError> {
                Ok(TokenizedText {
                    text: "normalized".to_string(),
                    spans: vec![TokenSpan { start: 0, end: 10 }],
                    original_text: "source".to_string(),
                    original_spans: Vec::new(),
                })
            }
        }

        let error = chunk_markdown("source", &BrokenTokenizer).unwrap_err();
        assert!(error
            .to_string()
            .contains("one original span per normalized token"));
    }
}
