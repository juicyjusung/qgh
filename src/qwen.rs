use crate::config::LocalModelDevice;
use crate::embedding::{
    EmbeddingEngine, EmbeddingProviderError, EmbeddingRuntimeContract, EmbeddingTokenizer,
    LocalEmbeddingProvider, NormalizationKind, TokenSpan, TokenizedText,
};
use crate::error::QghError;
use crate::local_models::{PreparedQwenModelSnapshot, QWEN_EMBEDDING_QUERY_PREFIX};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use fastembed::{Qwen3Config, Qwen3Model, Qwen3TextEmbedding};
use serde::Deserialize;
use std::fs;
use std::sync::Mutex;
use tokenizers::{PaddingDirection, PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

pub const QWEN_EMBEDDING_NATIVE_DIMENSION: usize = 1024;
pub const QWEN_EMBEDDING_OUTPUT_DIMENSION: usize = 384;
pub const QWEN_EMBEDDING_MAX_LENGTH: usize = 1024;
pub const QWEN_EMBEDDING_BATCH_SIZE: usize = 4;
const QWEN_EMBEDDING_BATCH_MAX_TOKENS: usize = 128;
pub const QWEN_RERANK_DEPTH: usize = 10;
pub const QWEN_RERANK_MAX_LENGTH: usize = 384;
const QWEN_RERANK_MICRO_BATCH: usize = 2;
const QWEN_RERANK_QUERY_MAX_TOKENS: usize = 96;
const QWEN_RERANK_MIN_DOCUMENT_TOKENS: usize = 128;
const TRUE_TOKEN_ID: u32 = 9_693;
const FALSE_TOKEN_ID: u32 = 2_152;
const RERANK_TASK: &str =
    "Given a GitHub issue or comment search query, retrieve relevant issue or comment passages.";
const RERANK_SYSTEM: &str = "Judge whether the Document meets the requirements based on the Query and the Instruct provided. Note that the answer can only be \"yes\" or \"no\".";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QwenEmbeddingRuntimeProfile {
    CpuF32,
    MetalF16,
}

impl QwenEmbeddingRuntimeProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CpuF32 => "cpu_f32",
            Self::MetalF16 => "metal_f16",
        }
    }
}

pub fn qwen_embedding_runtime_profile_id(requested: LocalModelDevice) -> &'static str {
    match requested {
        LocalModelDevice::Cpu => QwenEmbeddingRuntimeProfile::CpuF32.as_str(),
        LocalModelDevice::Metal => QwenEmbeddingRuntimeProfile::MetalF16.as_str(),
        LocalModelDevice::Auto => {
            if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                QwenEmbeddingRuntimeProfile::MetalF16.as_str()
            } else {
                QwenEmbeddingRuntimeProfile::CpuF32.as_str()
            }
        }
    }
}

pub fn validate_qwen_embedding_device(
    requested: LocalModelDevice,
) -> Result<(), EmbeddingProviderError> {
    if resolve_embedding_profile(requested)? == QwenEmbeddingRuntimeProfile::MetalF16 {
        metal_device()?;
    }
    Ok(())
}

pub struct QwenEmbeddingParts {
    pub tokenizer: QwenEmbeddingTokenizer,
    pub provider: LocalEmbeddingProvider<QwenEmbeddingEngine>,
    pub runtime_profile: QwenEmbeddingRuntimeProfile,
}

pub struct QwenEmbeddingEngine {
    model: Mutex<Qwen3TextEmbedding>,
    batch_tokenizer: Tokenizer,
    runtime_profile: QwenEmbeddingRuntimeProfile,
}

pub struct QwenEmbeddingTokenizer {
    tokenizer: Tokenizer,
}

impl QwenEmbeddingTokenizer {
    pub fn fit_input(&self, text: &str) -> Result<String, EmbeddingProviderError> {
        fit_qwen_embedding_input(&self.tokenizer, text)
    }
}

pub fn load_qwen_embedding_tokenizer(
    snapshot: &PreparedQwenModelSnapshot,
) -> Result<QwenEmbeddingTokenizer, EmbeddingProviderError> {
    snapshot
        .revalidate_artifact_identities()
        .map_err(embedding_snapshot_error)?;
    let tokenizer_path = snapshot
        .artifact_path("tokenizer.json")
        .map_err(embedding_snapshot_error)?;
    let tokenizer =
        Tokenizer::from_file(tokenizer_path).map_err(|_| embedding_tokenizer_error())?;
    snapshot
        .revalidate_artifact_identities()
        .map_err(embedding_snapshot_error)?;
    Ok(QwenEmbeddingTokenizer { tokenizer })
}

pub fn load_qwen_embedding(
    snapshot: &PreparedQwenModelSnapshot,
    requested_device: LocalModelDevice,
) -> Result<QwenEmbeddingParts, EmbeddingProviderError> {
    snapshot
        .revalidate_artifact_identities()
        .map_err(embedding_snapshot_error)?;
    let runtime_profile = resolve_embedding_profile(requested_device)?;
    let (device, dtype) = match runtime_profile {
        QwenEmbeddingRuntimeProfile::CpuF32 => (Device::Cpu, DType::F32),
        QwenEmbeddingRuntimeProfile::MetalF16 => (metal_device()?, DType::F16),
    };
    let config_path = snapshot
        .artifact_path("config.json")
        .map_err(embedding_snapshot_error)?;
    let weights_path = snapshot
        .artifact_path("model.safetensors")
        .map_err(embedding_snapshot_error)?;
    let config: Qwen3Config =
        serde_json::from_slice(&fs::read(config_path).map_err(|_| embedding_runtime_error())?)
            .map_err(|_| embedding_runtime_error())?;
    let builder = unsafe {
        VarBuilder::from_mmaped_safetensors(&[weights_path], dtype, &device)
            .map_err(|_| embedding_runtime_error())?
    };
    let model = Qwen3Model::new(config, builder).map_err(|_| embedding_runtime_error())?;
    let chunk_tokenizer = load_qwen_embedding_tokenizer(snapshot)?.tokenizer;
    let mut inference_tokenizer = chunk_tokenizer.clone();
    inference_tokenizer.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::BatchLongest,
        direction: PaddingDirection::Left,
        ..Default::default()
    }));
    inference_tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: QWEN_EMBEDDING_MAX_LENGTH,
            ..Default::default()
        }))
        .map_err(|_| embedding_tokenizer_error())?;
    let batch_tokenizer = chunk_tokenizer.clone();
    let engine = QwenEmbeddingEngine {
        model: Mutex::new(Qwen3TextEmbedding::new(model, inference_tokenizer)),
        batch_tokenizer,
        runtime_profile,
    };
    let provider = LocalEmbeddingProvider::with_contract(
        engine,
        EmbeddingRuntimeContract {
            query_prefix: Some(QWEN_EMBEDDING_QUERY_PREFIX.to_string()),
            document_prefix: Some(String::new()),
            normalization: NormalizationKind::L2,
            native_dimension: QWEN_EMBEDDING_NATIVE_DIMENSION,
            output_dimension: QWEN_EMBEDDING_OUTPUT_DIMENSION,
        },
    )?;
    snapshot
        .revalidate_artifact_identities()
        .map_err(embedding_snapshot_error)?;
    Ok(QwenEmbeddingParts {
        tokenizer: QwenEmbeddingTokenizer {
            tokenizer: chunk_tokenizer,
        },
        provider,
        runtime_profile,
    })
}

fn resolve_embedding_profile(
    requested: LocalModelDevice,
) -> Result<QwenEmbeddingRuntimeProfile, EmbeddingProviderError> {
    match requested {
        LocalModelDevice::Cpu => Ok(QwenEmbeddingRuntimeProfile::CpuF32),
        LocalModelDevice::Metal => {
            if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                Ok(QwenEmbeddingRuntimeProfile::MetalF16)
            } else {
                Err(embedding_device_error())
            }
        }
        LocalModelDevice::Auto => {
            if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                Ok(QwenEmbeddingRuntimeProfile::MetalF16)
            } else {
                Ok(QwenEmbeddingRuntimeProfile::CpuF32)
            }
        }
    }
}

impl EmbeddingEngine for QwenEmbeddingEngine {
    fn embed_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingProviderError> {
        let texts = texts
            .iter()
            .map(|text| fit_qwen_embedding_input(&self.batch_tokenizer, text))
            .collect::<Result<Vec<_>, _>>()?;
        if self.runtime_profile == QwenEmbeddingRuntimeProfile::CpuF32 {
            let model = self.model.lock().map_err(|_| embedding_runtime_error())?;
            let mut vectors = Vec::with_capacity(texts.len());
            for batch in texts.chunks(QWEN_EMBEDDING_BATCH_SIZE) {
                vectors.extend(
                    model
                        .embed(batch)
                        .map_err(|_| embedding_inference_error())?,
                );
            }
            return Ok(vectors);
        }

        let token_lengths = texts
            .iter()
            .map(|text| {
                self.batch_tokenizer
                    .encode(text.as_str(), true)
                    .map(|encoding| encoding.len())
                    .map_err(|_| embedding_tokenizer_error())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batches = qwen_embedding_batch_plan(self.runtime_profile, &token_lengths);
        let model = self.model.lock().map_err(|_| embedding_runtime_error())?;
        let mut vectors = vec![None; texts.len()];
        for batch_order in batches {
            let batch = batch_order
                .iter()
                .map(|index| texts[*index].as_str())
                .collect::<Vec<_>>();
            let batch_vectors = model
                .embed(&batch)
                .map_err(|_| embedding_inference_error())?;
            if batch_vectors.len() != batch_order.len() {
                return Err(embedding_inference_error());
            }
            for (index, vector) in batch_order.iter().zip(batch_vectors) {
                vectors[*index] = Some(vector);
            }
        }
        vectors
            .into_iter()
            .map(|vector| vector.ok_or_else(embedding_inference_error))
            .collect()
    }
}

/// Fits one document or query to the pinned Qwen window before inference.
///
/// The context prefix is at the beginning of the input, so truncating at the
/// last complete token preserves repository/issue identity and removes only
/// the trailing body. The model tokenizer retains its own truncation as a
/// defense in depth, but production inputs must already satisfy this bound.
pub fn fit_qwen_embedding_input(
    tokenizer: &Tokenizer,
    text: &str,
) -> Result<String, EmbeddingProviderError> {
    let encoding = tokenizer
        .encode(text, true)
        .map_err(|_| embedding_tokenizer_error())?;
    if encoding.len() <= QWEN_EMBEDDING_MAX_LENGTH {
        return Ok(text.to_string());
    }

    let special_tokens = encoding
        .get_special_tokens_mask()
        .iter()
        .filter(|special| **special != 0)
        .count();
    let regular_token_limit = QWEN_EMBEDDING_MAX_LENGTH
        .checked_sub(special_tokens)
        .ok_or_else(embedding_tokenizer_error)?;
    let mut retained_tokens = 0usize;
    let mut retained_byte_end = 0usize;
    for ((start, end), special) in encoding
        .get_offsets()
        .iter()
        .zip(encoding.get_special_tokens_mask())
    {
        if *special != 0 {
            continue;
        }
        if retained_tokens == regular_token_limit {
            break;
        }
        if start < end {
            retained_byte_end = *end;
        }
        retained_tokens += 1;
    }
    if retained_byte_end == 0
        || retained_byte_end > text.len()
        || !text.is_char_boundary(retained_byte_end)
    {
        return Err(embedding_tokenizer_error());
    }
    let fitted = text[..retained_byte_end].to_string();
    let fitted_length = tokenizer
        .encode(fitted.as_str(), true)
        .map_err(|_| embedding_tokenizer_error())?
        .len();
    if fitted_length > QWEN_EMBEDDING_MAX_LENGTH {
        return Err(EmbeddingProviderError::structured(
            "embedding.input_window_exceeded",
            "Prepared embedding input could not be fitted to the local model window.",
        )
        .with_details(serde_json::json!({
            "maximum_tokens": QWEN_EMBEDDING_MAX_LENGTH,
            "actual_tokens": fitted_length
        })));
    }
    Ok(fitted)
}

fn qwen_embedding_batch_plan(
    runtime_profile: QwenEmbeddingRuntimeProfile,
    token_lengths: &[usize],
) -> Vec<Vec<usize>> {
    if runtime_profile == QwenEmbeddingRuntimeProfile::CpuF32 {
        return (0..token_lengths.len())
            .collect::<Vec<_>>()
            .chunks(QWEN_EMBEDDING_BATCH_SIZE)
            .map(<[usize]>::to_vec)
            .collect();
    }

    let mut input_order = (0..token_lengths.len()).collect::<Vec<_>>();
    input_order.sort_by_key(|index| token_lengths[*index]);
    let mut batches = Vec::new();
    let mut start = 0usize;
    while start < input_order.len() {
        let mut end = start + 1;
        if token_lengths[input_order[start]] <= QWEN_EMBEDDING_BATCH_MAX_TOKENS {
            while end < input_order.len()
                && end - start < QWEN_EMBEDDING_BATCH_SIZE
                && token_lengths[input_order[end]] <= QWEN_EMBEDDING_BATCH_MAX_TOKENS
            {
                end += 1;
            }
        }
        batches.push(input_order[start..end].to_vec());
        start = end;
    }
    batches
}

impl EmbeddingTokenizer for QwenEmbeddingTokenizer {
    fn tokenize(&self, text: &str) -> Result<Vec<TokenSpan>, EmbeddingProviderError> {
        Ok(self.tokenize_canonical(text)?.spans)
    }

    fn tokenize_canonical(&self, text: &str) -> Result<TokenizedText, EmbeddingProviderError> {
        use tokenizers::Normalizer;

        let mut normalized = tokenizers::NormalizedString::from(text);
        if let Some(normalizer) = self.tokenizer.get_normalizer() {
            normalizer
                .normalize(&mut normalized)
                .map_err(|_| embedding_tokenizer_error())?;
        }
        let canonical = normalized.get().to_string();
        let encoding = self
            .tokenizer
            .encode(canonical.as_str(), false)
            .map_err(|_| embedding_tokenizer_error())?;
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
                .ok_or_else(embedding_tokenizer_error)?;
            if original.start >= original.end
                || original.end > text.len()
                || !text.is_char_boundary(original.start)
                || !text.is_char_boundary(original.end)
            {
                return Err(embedding_tokenizer_error());
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QwenRerankerRuntimeProfile {
    CpuF32,
    MetalF32,
}

impl QwenRerankerRuntimeProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CpuF32 => "cpu_f32",
            Self::MetalF32 => "metal_f32",
        }
    }
}

pub struct QwenReranker {
    model: Mutex<Qwen3Model>,
    tokenizer: Tokenizer,
    pad_token_id: u32,
    pub runtime_profile: QwenRerankerRuntimeProfile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogitScoreConfig {
    true_token_id: u32,
    false_token_id: u32,
}

pub fn load_qwen_reranker(
    snapshot: &PreparedQwenModelSnapshot,
    requested_device: LocalModelDevice,
) -> Result<QwenReranker, QghError> {
    snapshot.revalidate_artifact_identities()?;
    let runtime_profile = resolve_reranker_profile(requested_device)?;
    let device = match runtime_profile {
        QwenRerankerRuntimeProfile::CpuF32 => Device::Cpu,
        QwenRerankerRuntimeProfile::MetalF32 => {
            metal_device().map_err(|_| reranker_device_error())?
        }
    };
    let config: Qwen3Config = serde_json::from_slice(
        &fs::read(snapshot.artifact_path("config.json")?).map_err(|_| reranker_runtime_error())?,
    )
    .map_err(|_| reranker_runtime_error())?;
    if !config.tie_word_embeddings {
        return Err(reranker_contract_error());
    }
    let logit_config: LogitScoreConfig = serde_json::from_slice(
        &fs::read(snapshot.artifact_path("1_LogitScore/config.json")?)
            .map_err(|_| reranker_runtime_error())?,
    )
    .map_err(|_| reranker_contract_error())?;
    if logit_config.true_token_id != TRUE_TOKEN_ID || logit_config.false_token_id != FALSE_TOKEN_ID
    {
        return Err(reranker_contract_error());
    }
    let tokenizer = Tokenizer::from_file(snapshot.artifact_path("tokenizer.json")?)
        .map_err(|_| reranker_runtime_error())?;
    if tokenizer.token_to_id("yes") != Some(TRUE_TOKEN_ID)
        || tokenizer.token_to_id("no") != Some(FALSE_TOKEN_ID)
    {
        return Err(reranker_contract_error());
    }
    let pad_token_id = tokenizer
        .token_to_id("<|endoftext|>")
        .ok_or_else(reranker_contract_error)?;
    let builder = unsafe {
        VarBuilder::from_mmaped_safetensors(
            &[snapshot.artifact_path("model.safetensors")?],
            DType::F32,
            &device,
        )
        .map_err(|_| reranker_runtime_error())?
    };
    let model =
        Qwen3Model::new(config, builder.pp("model")).map_err(|_| reranker_runtime_error())?;
    snapshot.revalidate_artifact_identities()?;
    Ok(QwenReranker {
        model: Mutex::new(model),
        tokenizer,
        pad_token_id,
        runtime_profile,
    })
}

fn resolve_reranker_profile(
    requested: LocalModelDevice,
) -> Result<QwenRerankerRuntimeProfile, QghError> {
    match requested {
        LocalModelDevice::Cpu => Ok(QwenRerankerRuntimeProfile::CpuF32),
        LocalModelDevice::Metal | LocalModelDevice::Auto => {
            if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                Ok(QwenRerankerRuntimeProfile::MetalF32)
            } else {
                Err(reranker_device_error())
            }
        }
    }
}

impl QwenReranker {
    pub fn score(&self, query: &str, documents: &[String]) -> Result<Vec<f32>, QghError> {
        if documents.is_empty() || documents.len() > QWEN_RERANK_DEPTH {
            return Err(reranker_contract_error());
        }
        let encoded = documents
            .iter()
            .map(|document| self.encode_pair(query, document))
            .collect::<Result<Vec<_>, _>>()?;
        let model = self.model.lock().map_err(|_| reranker_runtime_error())?;
        let mut scores = Vec::with_capacity(encoded.len());
        for batch in encoded.chunks(QWEN_RERANK_MICRO_BATCH) {
            scores.extend(score_reranker_batch(&model, batch, self.pad_token_id)?);
        }
        if scores.len() != documents.len() || scores.iter().any(|score| !score.is_finite()) {
            return Err(reranker_inference_error());
        }
        Ok(scores)
    }

    fn encode_pair(&self, query: &str, document: &str) -> Result<Vec<u32>, QghError> {
        let prefix = format!(
            "<|im_start|>system\n{RERANK_SYSTEM}<|im_end|>\n<|im_start|>user\n<Instruct>: {RERANK_TASK}\n<Query>: "
        );
        let document_separator = "\n<Document>: ";
        let suffix = "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";
        let prefix_ids = self
            .tokenizer
            .encode(prefix, false)
            .map_err(|_| reranker_inference_error())?;
        let query_ids = self
            .tokenizer
            .encode(query, false)
            .map_err(|_| reranker_inference_error())?;
        let separator_ids = self
            .tokenizer
            .encode(document_separator, false)
            .map_err(|_| reranker_inference_error())?;
        let document_ids = self
            .tokenizer
            .encode(document, false)
            .map_err(|_| reranker_inference_error())?;
        let suffix_ids = self
            .tokenizer
            .encode(suffix, false)
            .map_err(|_| reranker_inference_error())?;
        let fixed = prefix_ids
            .len()
            .checked_add(separator_ids.len())
            .and_then(|length| length.checked_add(suffix_ids.len()))
            .ok_or_else(reranker_contract_error)?;
        let content_budget = QWEN_RERANK_MAX_LENGTH
            .checked_sub(fixed)
            .ok_or_else(reranker_contract_error)?;
        if content_budget < QWEN_RERANK_MIN_DOCUMENT_TOKENS {
            return Err(reranker_contract_error());
        }
        let query_budget = content_budget
            .saturating_sub(QWEN_RERANK_MIN_DOCUMENT_TOKENS)
            .min(QWEN_RERANK_QUERY_MAX_TOKENS);
        let query_length = query_ids.len().min(query_budget);
        let document_budget = content_budget
            .checked_sub(query_length)
            .ok_or_else(reranker_contract_error)?;
        let mut ids = Vec::with_capacity(QWEN_RERANK_MAX_LENGTH);
        ids.extend_from_slice(prefix_ids.get_ids());
        ids.extend_from_slice(&query_ids.get_ids()[..query_length]);
        ids.extend_from_slice(separator_ids.get_ids());
        ids.extend_from_slice(
            &document_ids.get_ids()[..document_ids.get_ids().len().min(document_budget)],
        );
        ids.extend_from_slice(suffix_ids.get_ids());
        if ids.len() > QWEN_RERANK_MAX_LENGTH {
            return Err(reranker_contract_error());
        }
        Ok(ids)
    }
}

fn score_reranker_batch(
    model: &Qwen3Model,
    rows: &[Vec<u32>],
    pad_token_id: u32,
) -> Result<Vec<f32>, QghError> {
    let batch_size = rows.len();
    let seq_len = rows
        .iter()
        .map(Vec::len)
        .max()
        .ok_or_else(reranker_contract_error)?;
    let mut input_ids = Vec::with_capacity(batch_size * seq_len);
    let mut attention_mask = Vec::with_capacity(batch_size * seq_len);
    for row in rows {
        let padding = seq_len - row.len();
        input_ids.extend(std::iter::repeat_n(pad_token_id, padding));
        input_ids.extend_from_slice(row);
        attention_mask.extend(std::iter::repeat_n(0f32, padding));
        attention_mask.extend(std::iter::repeat_n(1f32, row.len()));
    }
    let device = model.device();
    let input_ids = Tensor::from_vec(input_ids, (batch_size, seq_len), device)
        .map_err(|_| reranker_inference_error())?;
    let attention_mask = Tensor::from_vec(attention_mask, (batch_size, seq_len), device)
        .map_err(|_| reranker_inference_error())?;
    let causal_mask = build_attention_mask(&attention_mask)?;
    let hidden = model
        .forward(&input_ids, Some(&causal_mask))
        .map_err(|_| reranker_inference_error())?;
    let last_hidden = hidden
        .i((.., seq_len - 1, ..))
        .and_then(|tensor| tensor.contiguous())
        .map_err(|_| reranker_inference_error())?;
    let label_ids = Tensor::from_slice(&[FALSE_TOKEN_ID, TRUE_TOKEN_ID], (2,), device)
        .map_err(|_| reranker_inference_error())?;
    let label_embeddings = model
        .embed_tokens(&label_ids)
        .and_then(|tensor| tensor.contiguous())
        .map_err(|_| reranker_inference_error())?;
    let label_projection = label_embeddings
        .t()
        .and_then(|tensor| tensor.contiguous())
        .map_err(|_| reranker_inference_error())?;
    let logits = last_hidden
        .matmul(&label_projection)
        .and_then(|tensor| tensor.to_dtype(DType::F32))
        .and_then(|tensor| tensor.to_vec2::<f32>())
        .map_err(|_| reranker_inference_error())?;
    Ok(logits.into_iter().map(|row| row[1] - row[0]).collect())
}

fn build_attention_mask(mask_2d: &Tensor) -> Result<Tensor, QghError> {
    let (batch_size, seq_len) = mask_2d.dims2().map_err(|_| reranker_inference_error())?;
    let device = mask_2d.device();
    let mask_value = -1e4f32;
    let mut causal = vec![0.0f32; seq_len * seq_len];
    for row in 0..seq_len {
        for column in (row + 1)..seq_len {
            causal[row * seq_len + column] = mask_value;
        }
    }
    let causal = Tensor::from_vec(causal, (1, 1, seq_len, seq_len), device)
        .map_err(|_| reranker_inference_error())?;
    let expanded = mask_2d
        .unsqueeze(1)
        .and_then(|tensor| tensor.unsqueeze(2))
        .and_then(|tensor| tensor.expand((batch_size, 1, seq_len, seq_len)))
        .and_then(|tensor| tensor.to_dtype(DType::F32))
        .map_err(|_| reranker_inference_error())?;
    let inverted = Tensor::ones_like(&expanded)
        .and_then(|ones| ones.sub(&expanded))
        .map_err(|_| reranker_inference_error())?;
    let pad_value = Tensor::new(&[mask_value], device).map_err(|_| reranker_inference_error())?;
    let padding = inverted
        .broadcast_mul(&pad_value)
        .map_err(|_| reranker_inference_error())?;
    causal
        .broadcast_as((batch_size, 1, seq_len, seq_len))
        .and_then(|causal| causal.add(&padding))
        .map_err(|_| reranker_inference_error())
}

fn metal_device() -> Result<Device, EmbeddingProviderError> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Device::new_metal(0).map_err(|_| embedding_device_error())
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        Err(embedding_device_error())
    }
}

fn embedding_snapshot_error(_error: QghError) -> EmbeddingProviderError {
    EmbeddingProviderError::structured(
        "embedding.qwen_snapshot_invalid",
        "The prepared Qwen embedding snapshot failed integrity validation.",
    )
}

fn embedding_runtime_error() -> EmbeddingProviderError {
    EmbeddingProviderError::structured(
        "embedding.qwen_runtime_unavailable",
        "The local Qwen embedding runtime could not be initialized.",
    )
}

fn embedding_device_error() -> EmbeddingProviderError {
    EmbeddingProviderError::structured(
        "embedding.qwen_device_unavailable",
        "The configured Qwen embedding device is unavailable.",
    )
}

fn embedding_tokenizer_error() -> EmbeddingProviderError {
    EmbeddingProviderError::structured(
        "embedding.qwen_tokenizer_failed",
        "The local Qwen embedding tokenizer failed.",
    )
}

fn embedding_inference_error() -> EmbeddingProviderError {
    EmbeddingProviderError::structured(
        "embedding.qwen_inference_failed",
        "The local Qwen embedding runtime failed to produce embeddings.",
    )
}

fn reranker_device_error() -> QghError {
    QghError::validation(
        "reranker.device_unavailable",
        "The configured local reranker device is unavailable.",
    )
}

fn reranker_runtime_error() -> QghError {
    QghError::validation(
        "reranker.runtime_unavailable",
        "The local reranker runtime could not be initialized.",
    )
}

fn reranker_contract_error() -> QghError {
    QghError::validation(
        "reranker.contract_invalid",
        "The local reranker contract is invalid.",
    )
}

fn reranker_inference_error() -> QghError {
    QghError::validation(
        "reranker.inference_failed",
        "The local reranker failed to score the complete candidate set.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::EmbeddingProvider;
    use crate::local_models::{
        qwen_model_spec, PreparedQwenModelStore, QWEN_EMBEDDING_PRESET_ID, QWEN_RERANKER_PRESET_ID,
    };
    use std::path::PathBuf;

    #[test]
    fn runtime_profiles_do_not_silently_cross_device_boundaries() {
        validate_qwen_embedding_device(LocalModelDevice::Cpu).unwrap();
        assert_eq!(
            resolve_embedding_profile(LocalModelDevice::Cpu).unwrap(),
            QwenEmbeddingRuntimeProfile::CpuF32
        );
        assert_eq!(
            resolve_reranker_profile(LocalModelDevice::Cpu).unwrap(),
            QwenRerankerRuntimeProfile::CpuF32
        );
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            validate_qwen_embedding_device(LocalModelDevice::Metal).unwrap();
            assert_eq!(
                resolve_embedding_profile(LocalModelDevice::Auto).unwrap(),
                QwenEmbeddingRuntimeProfile::MetalF16
            );
            assert_eq!(
                resolve_reranker_profile(LocalModelDevice::Auto).unwrap(),
                QwenRerankerRuntimeProfile::MetalF32
            );
        } else {
            assert!(validate_qwen_embedding_device(LocalModelDevice::Metal).is_err());
        }
    }

    #[test]
    fn reranker_limits_are_fixed_product_contracts() {
        assert_eq!(QWEN_EMBEDDING_BATCH_SIZE, 4);
        assert_eq!(QWEN_RERANK_DEPTH, 10);
        assert_eq!(QWEN_RERANK_MAX_LENGTH, 384);
        assert_eq!(QWEN_RERANK_MICRO_BATCH, 2);
        assert_eq!(QWEN_RERANK_QUERY_MAX_TOKENS, 96);
        assert_eq!(QWEN_RERANK_MIN_DOCUMENT_TOKENS, 128);
    }

    #[test]
    fn cpu_embedding_batch_plan_preserves_fixed_order_and_batch_size() {
        let token_lengths = [900, 64, 256, 32, 129, 128, 5];

        assert_eq!(
            qwen_embedding_batch_plan(QwenEmbeddingRuntimeProfile::CpuF32, &token_lengths),
            vec![vec![0, 1, 2, 3], vec![4, 5, 6]]
        );
    }

    #[test]
    fn metal_embedding_batch_plan_groups_short_inputs_and_singletons_long_inputs() {
        let token_lengths = [900, 64, 256, 32, 129, 128, 5];

        assert_eq!(
            qwen_embedding_batch_plan(QwenEmbeddingRuntimeProfile::MetalF16, &token_lengths),
            vec![vec![6, 3, 1, 5], vec![4], vec![2], vec![0]]
        );
    }

    #[test]
    #[ignore = "requires explicitly installed pinned Qwen model snapshots"]
    fn live_prepared_embedding_snapshot_loads_sync_tokenizer_without_runtime() {
        let root = PathBuf::from(
            std::env::var("QGH_QWEN_PREPARED_MODELS")
                .expect("QGH_QWEN_PREPARED_MODELS must point to the prepared store"),
        );
        let spec = qwen_model_spec(QWEN_EMBEDDING_PRESET_ID).unwrap();
        let snapshot = PreparedQwenModelStore::new(root).inspect(&spec).unwrap();

        let tokenizer = load_qwen_embedding_tokenizer(&snapshot).unwrap();
        let tokens = tokenizer.tokenize("public sync tokenizer smoke").unwrap();

        assert!(!tokens.is_empty());
    }

    #[test]
    #[ignore = "requires explicitly installed pinned Qwen model snapshots"]
    fn live_prepared_embedding_adapter_produces_normalized_mrl_vectors() {
        let root = PathBuf::from(
            std::env::var("QGH_QWEN_PREPARED_MODELS")
                .expect("QGH_QWEN_PREPARED_MODELS must point to the prepared store"),
        );
        let spec = qwen_model_spec(QWEN_EMBEDDING_PRESET_ID).unwrap();
        let snapshot = PreparedQwenModelStore::new(root).inspect(&spec).unwrap();
        let runtime = load_qwen_embedding(&snapshot, LocalModelDevice::Auto).unwrap();
        let query = runtime
            .provider
            .embed_query("How is a stale publication rejected?")
            .unwrap();
        let documents = runtime
            .provider
            .embed_documents(&[
                "A stale publication generation is rejected before retrieval.",
                "The command line supports configurable terminal colors.",
            ])
            .unwrap();

        assert_eq!(query.len(), QWEN_EMBEDDING_OUTPUT_DIMENSION);
        assert!((query.iter().map(|value| value * value).sum::<f32>().sqrt() - 1.0).abs() < 1e-4);
        let score = |document: &[f32]| {
            query
                .iter()
                .zip(document)
                .map(|(left, right)| left * right)
                .sum::<f32>()
        };
        assert!(score(&documents[0]) > score(&documents[1]));
    }

    #[test]
    #[ignore = "requires explicitly installed pinned Qwen model snapshots"]
    fn live_prepared_reranker_adapter_preserves_public_relevance_order() {
        let root = PathBuf::from(
            std::env::var("QGH_QWEN_PREPARED_MODELS")
                .expect("QGH_QWEN_PREPARED_MODELS must point to the prepared store"),
        );
        let spec = qwen_model_spec(QWEN_RERANKER_PRESET_ID).unwrap();
        let snapshot = PreparedQwenModelStore::new(root).inspect(&spec).unwrap();
        let runtime = load_qwen_reranker(&snapshot, LocalModelDevice::Auto).unwrap();
        let long_query = "extended query context ".repeat(1_000);
        let relevant_encoding = runtime
            .encode_pair(&long_query, "The capital of China is Beijing.")
            .unwrap();
        let unrelated_encoding = runtime
            .encode_pair(&long_query, "Gravity attracts physical bodies.")
            .unwrap();
        assert_ne!(relevant_encoding, unrelated_encoding);
        assert!(relevant_encoding.len() <= QWEN_RERANK_MAX_LENGTH);
        assert!(unrelated_encoding.len() <= QWEN_RERANK_MAX_LENGTH);
        let scores = runtime
            .score(
                "What is the capital of China?",
                &[
                    "The capital of China is Beijing.".to_string(),
                    "Gravity attracts physical bodies.".to_string(),
                ],
            )
            .unwrap();

        assert_eq!(scores.len(), 2);
        assert!(scores[0] > scores[1]);
    }
}
