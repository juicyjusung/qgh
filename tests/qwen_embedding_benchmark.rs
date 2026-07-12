#![cfg(feature = "fastembed-provider")]

use qgh::embedding::{EmbeddingProvider, EmbeddingTokenizer};
use qgh::search_eval::{
    load_qwen_embedding, qwen_model_spec, LocalModelDevice, PreparedQwenModelStore,
    QWEN_EMBEDDING_OUTPUT_DIMENSION, QWEN_EMBEDDING_PRESET_ID,
};
use serde_json::json;
use std::path::PathBuf;
use std::time::Instant;

const BATCH_SIZES: [usize; 5] = [1, 2, 4, 8, 16];
const TARGET_TOKEN_LENGTHS: [usize; 3] = [64, 256, 900];
const QUICK_BATCH_SIZES: [usize; 1] = [16];
const QUICK_TARGET_TOKEN_LENGTHS: [usize; 1] = [900];

#[derive(Debug)]
struct ThroughputSample {
    target_tokens: usize,
    actual_tokens: usize,
    batch_size: usize,
    chunks_per_second: f64,
    elapsed_ms: f64,
    minimum_cosine: f32,
}

#[derive(Debug)]
struct MixedCorpusSample {
    chunks_per_second: f64,
    elapsed_ms: f64,
    minimum_cosine: f32,
}

#[test]
#[ignore = "requires the pinned Qwen embedding snapshot and an Apple Metal device"]
fn native_metal_embedding_batch_profile_preserves_outputs() {
    if std::env::var("QGH_QWEN_BATCH_BENCH").as_deref() != Ok("1") {
        eprintln!("skipped: set QGH_QWEN_BATCH_BENCH=1 for the explicit Metal benchmark");
        return;
    }

    let prepared_root = PathBuf::from(
        std::env::var("QGH_QWEN_PREPARED_MODELS")
            .expect("QGH_QWEN_PREPARED_MODELS must point to the prepared model store"),
    );
    let spec = qwen_model_spec(QWEN_EMBEDDING_PRESET_ID).unwrap();
    let snapshot = PreparedQwenModelStore::new(prepared_root)
        .inspect(&spec)
        .expect("verify prepared Qwen embedding snapshot");
    let load_started = Instant::now();
    let runtime = load_qwen_embedding(&snapshot, LocalModelDevice::Metal)
        .expect("load the native Metal Qwen adapter");
    let cold_load_ms = load_started.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(runtime.runtime_profile.as_str(), "metal_f16");

    let quick = std::env::var("QGH_QWEN_BATCH_BENCH_QUICK").as_deref() == Ok("1");
    let target_token_lengths = if quick {
        QUICK_TARGET_TOKEN_LENGTHS.as_slice()
    } else {
        TARGET_TOKEN_LENGTHS.as_slice()
    };
    let batch_sizes = if quick {
        QUICK_BATCH_SIZES.as_slice()
    } else {
        BATCH_SIZES.as_slice()
    };

    let mut samples = Vec::new();
    for &target_tokens in target_token_lengths {
        let (text, actual_tokens) =
            synthetic_document(&runtime.tokenizer, target_tokens).expect("build synthetic text");
        assert!(actual_tokens >= target_tokens);
        assert!(actual_tokens <= 1_024);

        runtime
            .provider
            .embed_documents(&[text.as_str()])
            .expect("warm the representative token length");
        let reference = runtime
            .provider
            .embed_documents(&[text.as_str()])
            .expect("embed singleton parity reference")
            .pop()
            .unwrap();

        for &batch_size in batch_sizes {
            let inputs = vec![text.as_str(); batch_size];
            let started = Instant::now();
            let vectors = runtime
                .provider
                .embed_documents(&inputs)
                .expect("embed synthetic Metal batch");
            let elapsed = started.elapsed();
            assert_eq!(vectors.len(), batch_size);
            assert!(vectors.iter().all(|vector| {
                vector.len() == QWEN_EMBEDDING_OUTPUT_DIMENSION
                    && vector.iter().all(|value| value.is_finite())
                    && (l2_norm(vector) - 1.0).abs() < 1e-4
            }));
            let minimum_cosine = vectors
                .iter()
                .map(|vector| cosine(&reference, vector))
                .fold(f32::INFINITY, f32::min);
            assert!(
                minimum_cosine >= 0.99999,
                "batch {batch_size} at {actual_tokens} tokens changed output cosine to {minimum_cosine}"
            );
            let elapsed_seconds = elapsed.as_secs_f64();
            samples.push(ThroughputSample {
                target_tokens,
                actual_tokens,
                batch_size,
                chunks_per_second: batch_size as f64 / elapsed_seconds,
                elapsed_ms: elapsed_seconds * 1_000.0,
                minimum_cosine,
            });
        }
    }

    let mixed_corpus = mixed_corpus_profile(&runtime.tokenizer, &runtime.provider);
    assert!(mixed_corpus.minimum_cosine >= 0.99999);
    let parity = ranking_parity(&runtime.provider);
    assert!(parity.iter().all(|(_, minimum_cosine, exact_ranking)| {
        *minimum_cosine >= 0.99999 && *exact_ranking
    }));

    let long_batch_16 = samples
        .iter()
        .find(|sample| sample.target_tokens == 900 && sample.batch_size == 16)
        .unwrap();
    for sample in &samples {
        println!(
            "{}",
            json!({
                "kind": "throughput",
                "target_tokens": sample.target_tokens,
                "actual_tokens": sample.actual_tokens,
                "batch_size": sample.batch_size,
                "chunks_per_second": sample.chunks_per_second,
                "elapsed_ms": sample.elapsed_ms,
                "minimum_cosine": sample.minimum_cosine,
            })
        );
    }
    for (batch_size, minimum_cosine, exact_ranking) in &parity {
        println!(
            "{}",
            json!({
                "kind": "ranking_parity",
                "batch_size": batch_size,
                "minimum_cosine": minimum_cosine,
                "exact_ranking": exact_ranking,
            })
        );
    }
    println!(
        "{}",
        json!({
            "kind": "mixed_corpus",
            "chunks_per_second": mixed_corpus.chunks_per_second,
            "elapsed_ms": mixed_corpus.elapsed_ms,
            "minimum_cosine": mixed_corpus.minimum_cosine,
        })
    );
    println!(
        "{}",
        json!({
            "kind": "summary",
            "runtime_profile": runtime.runtime_profile.as_str(),
            "cold_load_ms": cold_load_ms,
            "long_batch_16_chunks_per_second": long_batch_16.chunks_per_second,
            "minimum_output_cosine": samples
                .iter()
                .map(|sample| sample.minimum_cosine)
                .fold(f32::INFINITY, f32::min),
            "all_rankings_exact": parity.iter().all(|(_, _, exact)| *exact),
        })
    );

    if let Ok(raw_minimum) = std::env::var("QGH_QWEN_BENCH_MIN_LONG_CPS") {
        let minimum = raw_minimum
            .parse::<f64>()
            .expect("QGH_QWEN_BENCH_MIN_LONG_CPS must be a number");
        assert!(
            long_batch_16.chunks_per_second >= minimum,
            "long Metal throughput {:.3} chunks/s is below required {:.3} chunks/s",
            long_batch_16.chunks_per_second,
            minimum
        );
    }
}

fn synthetic_document(
    tokenizer: &impl EmbeddingTokenizer,
    target_tokens: usize,
) -> Result<(String, usize), Box<dyn std::error::Error>> {
    let fragments = [
        "Repository issue comments describe retrieval freshness and citation evidence. ",
        "한국어 검색 질의는 영어 GitHub 이슈와 안정적으로 연결되어야 합니다. ",
        "The vector publication rejects stale generations before hybrid ranking. ",
        "`query -> get -> cite` preserves source identity and canonical URLs. ",
    ];
    let mut text = String::new();
    let mut fragment = 0usize;
    loop {
        text.push_str(fragments[fragment % fragments.len()]);
        fragment += 1;
        let actual_tokens = tokenizer.count_tokens(&text)?;
        if actual_tokens >= target_tokens {
            return Ok((text, actual_tokens));
        }
    }
}

fn mixed_corpus_profile(
    tokenizer: &impl EmbeddingTokenizer,
    provider: &impl EmbeddingProvider,
) -> MixedCorpusSample {
    let target_lengths = [
        64, 256, 64, 900, 64, 256, 64, 900, 64, 256, 64, 900, 64, 256, 64, 900,
    ];
    let texts = target_lengths
        .into_iter()
        .enumerate()
        .map(|(index, target_tokens)| {
            let (body, _) = synthetic_document(tokenizer, target_tokens)
                .expect("build mixed-corpus synthetic text");
            format!("Synthetic public item {index}. {body}")
        })
        .collect::<Vec<_>>();
    let references = texts
        .iter()
        .map(|text| {
            provider
                .embed_documents(&[text.as_str()])
                .expect("embed mixed-corpus singleton reference")
                .pop()
                .unwrap()
        })
        .collect::<Vec<_>>();
    let refs = texts.iter().map(String::as_str).collect::<Vec<_>>();
    let started = Instant::now();
    let vectors = provider
        .embed_documents(&refs)
        .expect("embed mixed-corpus proxy");
    let elapsed = started.elapsed();
    assert_eq!(vectors.len(), texts.len());
    let minimum_cosine = references
        .iter()
        .zip(&vectors)
        .map(|(reference, vector)| cosine(reference, vector))
        .fold(f32::INFINITY, f32::min);
    MixedCorpusSample {
        chunks_per_second: texts.len() as f64 / elapsed.as_secs_f64(),
        elapsed_ms: elapsed.as_secs_f64() * 1_000.0,
        minimum_cosine,
    }
}

fn ranking_parity(provider: &impl EmbeddingProvider) -> Vec<(usize, f32, bool)> {
    let query = provider
        .embed_query("How does qgh reject a stale vector publication?")
        .expect("embed ranking query");
    let documents = [
        "qgh rejects stale vector generations before publishing hybrid retrieval results.",
        "Vector coverage is validated against the active source publication snapshot.",
        "The terminal color palette can be configured for command line output.",
        "A sourdough recipe uses flour, water, salt, and a fermented starter.",
    ];
    let reference_vectors = documents
        .iter()
        .map(|document| {
            provider
                .embed_documents(&[*document])
                .expect("embed singleton ranking reference")
                .pop()
                .unwrap()
        })
        .collect::<Vec<_>>();
    let expected_ranking = ranking(&query, &reference_vectors);

    BATCH_SIZES
        .into_iter()
        .map(|batch_size| {
            let mut inputs = documents.iter().copied().collect::<Vec<_>>();
            while inputs.len() < batch_size {
                inputs.push("Synthetic filler about an unrelated public weather report.");
            }
            let mut vectors = Vec::new();
            for batch in inputs.chunks(batch_size) {
                vectors.extend(
                    provider
                        .embed_documents(batch)
                        .expect("embed ranking parity batch"),
                );
            }
            vectors.truncate(documents.len());
            let minimum_cosine = reference_vectors
                .iter()
                .zip(&vectors)
                .map(|(reference, vector)| cosine(reference, vector))
                .fold(f32::INFINITY, f32::min);
            let exact_ranking = ranking(&query, &vectors) == expected_ranking;
            (batch_size, minimum_cosine, exact_ranking)
        })
        .collect()
}

fn ranking(query: &[f32], documents: &[Vec<f32>]) -> Vec<usize> {
    let mut scores = documents
        .iter()
        .enumerate()
        .map(|(index, document)| (index, dot(query, document)))
        .collect::<Vec<_>>();
    scores.sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
    scores.into_iter().map(|(index, _)| index).collect()
}

fn cosine(left: &[f32], right: &[f32]) -> f32 {
    dot(left, right) / (l2_norm(left) * l2_norm(right))
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum()
}

fn l2_norm(vector: &[f32]) -> f32 {
    vector.iter().map(|value| value * value).sum::<f32>().sqrt()
}
