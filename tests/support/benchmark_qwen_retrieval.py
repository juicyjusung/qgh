#!/usr/bin/env python3
"""Machine-only Qwen embedding/reranker evaluation over public qgh qrels."""

from __future__ import annotations

from collections import defaultdict
import argparse
import gc
import hashlib
import importlib.metadata
import json
import math
import platform
import re
import resource
from pathlib import Path
import subprocess
import sys
import time


RRF_K = 60
TOP_K = 20
CHUNK_TOKENS = 900
CHUNK_OVERLAP = 135
RERANK_DEPTH = 20
QUALITY_RSS_LIMIT_BYTES = 5 * 1024 * 1024 * 1024 // 2
EMBEDDING_MODEL_ID = "Qwen/Qwen3-Embedding-0.6B"
EMBEDDING_REVISION = "97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3"
RERANKER_MODEL_ID = "Qwen/Qwen3-Reranker-0.6B"
RERANKER_REVISION = "e61197ed45024b0ed8a2d74b80b4d909f1255473"
REPORT_SCHEMA_VERSION = "qgh.qwen_screening_benchmark.v2"
CHECKPOINT_SCHEMA_VERSION = "qgh.qwen_screening_checkpoint.v2"
TASK_INSTRUCTION = (
    "Given a GitHub issue search query, retrieve relevant GitHub issue or "
    "comment passages that satisfy the information need"
)
WEIGHT_GRID = (0.25, 0.5, 0.75, 1.0)
DIMENSION_GRID = (384, 1024)
SENSITIVE_OUTPUT_PATTERNS = (
    re.compile(r"(?:/Users/|/home/|[A-Za-z]:\\\\Users\\\\)"),
    re.compile(r"authorization\s*:", re.IGNORECASE),
    re.compile(r"(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})"),
)
WEIGHTED_CLASSES = (
    "english_semantic",
    "korean_semantic",
    "ko_query_en_source",
    "en_query_ko_source",
    "comment_only",
    "long_context",
)
WEIGHTED_CLASS_WEIGHTS = (0.50, 0.20, 0.15, 0.10, 0.025, 0.025)


def _parent_title(source: dict) -> str:
    title = source["title"]
    prefix = f"Comment on issue #{source['issue_number']}:"
    if title.startswith(prefix):
        return title[len(prefix) :].strip()
    return title.strip()


def build_document_text(source: dict) -> str:
    """Build qgh.context.v1-equivalent model input without changing source data."""
    if source["entity_type"] == "issue_comment":
        context = [
            f"Repository: {source['repo']}",
            f"Parent issue: #{source['issue_number']}",
            f"Parent title: {_parent_title(source)}",
        ]
    else:
        context = [
            f"Repository: {source['repo']}",
            f"Issue: #{source['issue_number']}",
            f"Title: {source['title'].strip()}",
        ]
    return "\n".join([*context, "", source["body"]])


def weighted_rrf(
    bm25: list[str], dense: list[str], *, dense_weight: float, exact: bool
) -> list[str]:
    if exact:
        return list(bm25)
    scores: dict[str, float] = defaultdict(float)
    first_seen: dict[str, int] = {}
    for branch, weight in ((bm25, 1.0), (dense, dense_weight)):
        for rank, source_id in enumerate(branch, 1):
            first_seen.setdefault(source_id, len(first_seen))
            scores[source_id] += weight / (RRF_K + rank)
    return sorted(
        scores, key=lambda source_id: (-scores[source_id], first_seen[source_id])
    )


def apply_reranker(
    candidates: list[str], scores: dict[str, float], *, exact: bool
) -> list[str]:
    if exact:
        return list(candidates)
    pool_order = {source_id: index for index, source_id in enumerate(candidates)}
    return sorted(
        candidates,
        key=lambda source_id: (
            -scores.get(source_id, float("-inf")),
            pool_order[source_id],
        ),
    )


def query_metrics(relevant: list[dict], ranked: list[str]) -> dict[str, float]:
    grades = {gold["source_id"]: gold["grade"] for gold in relevant}

    def recall_at(cutoff: int) -> float:
        if not grades:
            return 0.0
        found = {source_id for source_id in ranked[:cutoff] if source_id in grades}
        return len(found) / len(grades)

    first_hit = next(
        (
            index
            for index, source_id in enumerate(ranked[:10], 1)
            if source_id in grades
        ),
        None,
    )
    dcg = sum(
        ((2 ** grades.get(source_id, 0)) - 1) / math.log2(index + 1)
        for index, source_id in enumerate(ranked[:10], 1)
    )
    ideal = sum(
        ((2**grade) - 1) / math.log2(index + 1)
        for index, grade in enumerate(sorted(grades.values(), reverse=True)[:10], 1)
    )
    return {
        "ndcg_at_10": dcg / ideal if ideal else 0.0,
        "mrr_at_10": 1.0 / first_hit if first_hit else 0.0,
        "recall_at_5": recall_at(5),
        "recall_at_10": recall_at(10),
        "recall_at_20": recall_at(20),
    }


def redacted_event(
    *, query_id: str, query_class: str, query: str, rankings: dict[str, list[str]]
) -> dict:
    return {
        "query_id": query_id,
        "query_sha256": hashlib.sha256(query.encode()).hexdigest(),
        "class": query_class,
        "rankings": rankings,
    }


def build_redacted_events(
    qrels: list[dict], rankings: dict[str, dict[str, list[str]]]
) -> list[dict]:
    return [
        redacted_event(
            query_id=qrel["query_id"],
            query_class=qrel["class"],
            query=qrel["query"],
            rankings={
                name: values[qrel["query_id"]] for name, values in rankings.items()
            },
        )
        for qrel in qrels
    ]


def read_jsonl(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def load_bm25_rankings(path: Path, qrels: list[dict]) -> dict[str, list[str]]:
    events = {event["query_id"]: event for event in read_jsonl(path)}
    rankings = {}
    for qrel in qrels:
        event = events.get(qrel["query_id"])
        if event is None:
            raise RuntimeError("BM25 evidence is missing a query identity")
        query_sha = hashlib.sha256(qrel["query"].encode()).hexdigest()
        if event.get("query_sha256") != query_sha:
            raise RuntimeError("BM25 query identity does not match the qrel")
        rankings[qrel["query_id"]] = list(event["ranked_source_ids"])
    return rankings


def source_matches_filter(source: dict, filters: dict) -> bool:
    if source["repo"] != filters["repo"]:
        return False
    if "issue_number" in filters and source["issue_number"] != filters["issue_number"]:
        return False
    if "source_type" in filters and source["entity_type"] != filters["source_type"]:
        return False
    return True


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = math.ceil(fraction * len(ordered)) - 1
    return ordered[max(0, min(index, len(ordered) - 1))]


def aggregate_metrics(
    corpus: list[dict], qrels: list[dict], rankings: dict[str, list[str]]
) -> dict:
    sources = {source["source_id"]: source for source in corpus}
    grouped: dict[str, list[dict[str, float]]] = defaultdict(list)
    exact_total = exact_hits = 0
    comment_total = comment_hits = 0
    hard_filter_violations = 0
    for qrel in qrels:
        ranked = rankings[qrel["query_id"]][:TOP_K]
        metrics = query_metrics(qrel["relevant"], ranked)
        grouped[qrel["class"]].append(metrics)
        if qrel["class"] == "exact_identifier":
            exact_total += 1
            gold = {relevant["source_id"] for relevant in qrel["relevant"]}
            exact_hits += int(bool(ranked) and ranked[0] in gold)
        if qrel["class"] == "comment_only":
            comment_total += 1
            comment_gold = {
                relevant["source_id"]
                for relevant in qrel["relevant"]
                if relevant["grade"] > 0
                and sources.get(relevant["source_id"], {}).get("entity_type")
                == "issue_comment"
            }
            comment_hits += int(
                any(source_id in comment_gold for source_id in ranked[:5])
            )
        for source_id in ranked:
            source = sources.get(source_id)
            if source is None or not source_matches_filter(source, qrel["filters"]):
                hard_filter_violations += 1

    per_class = {}
    for query_class, values in grouped.items():
        per_class[query_class] = {
            "query_count": len(values),
            **{
                name: sum(value[name] for value in values) / len(values)
                for name in (
                    "ndcg_at_10",
                    "mrr_at_10",
                    "recall_at_5",
                    "recall_at_10",
                    "recall_at_20",
                )
            },
        }
    weighted_ndcg = sum(
        weight * per_class.get(query_class, {}).get("ndcg_at_10", 0.0)
        for query_class, weight in zip(WEIGHTED_CLASSES, WEIGHTED_CLASS_WEIGHTS)
    )
    weighted_mrr = sum(
        weight * per_class.get(query_class, {}).get("mrr_at_10", 0.0)
        for query_class, weight in zip(WEIGHTED_CLASSES, WEIGHTED_CLASS_WEIGHTS)
    )
    return {
        "query_count": len(qrels),
        "per_class": per_class,
        "weighted_ndcg_at_10": weighted_ndcg,
        "weighted_mrr_at_10": weighted_mrr,
        "exact_top_1": exact_hits / exact_total if exact_total else 1.0,
        "comment_gold_recall_at_5": comment_hits / comment_total
        if comment_total
        else 1.0,
        "hard_filter_violations": hard_filter_violations,
    }


def complement_metrics(
    qrels: list[dict],
    bm25: dict[str, list[str]],
    candidate: dict[str, list[str]],
) -> dict[str, int]:
    counts = {
        "positive_query_count": 0,
        "bm25_miss_at_5": 0,
        "rescued_at_5": 0,
        "bm25_hit_preserved_at_5": 0,
        "bm25_hit_harmed_at_5": 0,
        "bm25_miss_at_10": 0,
        "rescued_at_10": 0,
        "bm25_hit_preserved_at_10": 0,
        "bm25_hit_harmed_at_10": 0,
    }
    for qrel in qrels:
        gold = {relevant["source_id"] for relevant in qrel["relevant"]}
        if not gold:
            continue
        counts["positive_query_count"] += 1
        for cutoff in (5, 10):
            baseline_hit = any(
                source_id in gold for source_id in bm25[qrel["query_id"]][:cutoff]
            )
            candidate_hit = any(
                source_id in gold for source_id in candidate[qrel["query_id"]][:cutoff]
            )
            if baseline_hit:
                key = (
                    f"bm25_hit_preserved_at_{cutoff}"
                    if candidate_hit
                    else f"bm25_hit_harmed_at_{cutoff}"
                )
            else:
                counts[f"bm25_miss_at_{cutoff}"] += 1
                key = f"rescued_at_{cutoff}" if candidate_hit else None
            if key is not None:
                counts[key] += 1
    return counts


def build_chunks(
    corpus: list[dict], tokenizer
) -> tuple[list[str], list[str], dict[str, list[str]]]:
    texts = []
    source_ids = []
    by_source: dict[str, list[str]] = defaultdict(list)
    step = CHUNK_TOKENS - CHUNK_OVERLAP
    for source in corpus:
        token_ids = tokenizer.encode(source["body"], add_special_tokens=False)
        windows = [
            token_ids[index : index + CHUNK_TOKENS]
            for index in range(0, len(token_ids), step)
        ]
        if not windows:
            windows = [[]]
        for window in windows:
            chunk_source = dict(source)
            chunk_source["body"] = tokenizer.decode(window, skip_special_tokens=True)
            text = build_document_text(chunk_source)
            texts.append(text)
            source_ids.append(source["source_id"])
            by_source[source["source_id"]].append(text)
            if len(window) < CHUNK_TOKENS:
                break
    return texts, source_ids, dict(by_source)


def normalize_dimension(matrix, dimension: int):
    import numpy as np

    truncated = matrix[:, :dimension]
    norms = np.linalg.norm(truncated, axis=1, keepdims=True)
    if np.any(norms == 0) or not np.isfinite(truncated).all():
        raise RuntimeError("embedding output is non-finite or zero length")
    return truncated / norms


def dense_rankings(
    corpus: list[dict],
    qrels: list[dict],
    query_embeddings,
    chunk_embeddings,
    chunk_source_ids: list[str],
) -> dict[str, list[str]]:
    sources = {source["source_id"]: source for source in corpus}
    rankings = {}
    for query_index, qrel in enumerate(qrels):
        scores = query_embeddings[query_index] @ chunk_embeddings.T
        source_scores: dict[str, float] = {}
        for chunk_index, source_id in enumerate(chunk_source_ids):
            if not source_matches_filter(sources[source_id], qrel["filters"]):
                continue
            source_scores[source_id] = max(
                source_scores.get(source_id, float("-inf")), float(scores[chunk_index])
            )
        rankings[qrel["query_id"]] = sorted(
            source_scores,
            key=lambda source_id: (-source_scores[source_id], source_id),
        )[:TOP_K]
    return rankings


def fused_rankings(
    qrels: list[dict],
    bm25: dict[str, list[str]],
    dense: dict[str, list[str]],
    weight: float,
) -> dict[str, list[str]]:
    return {
        qrel["query_id"]: weighted_rrf(
            bm25[qrel["query_id"]],
            dense[qrel["query_id"]],
            dense_weight=weight,
            exact=qrel["class"] == "exact_identifier",
        )[:TOP_K]
        for qrel in qrels
    }


def select_dev_fusion(
    corpus: list[dict],
    qrels: list[dict],
    bm25: dict[str, list[str]],
    dense_by_dimension: dict[int, dict[str, list[str]]],
) -> tuple[dict, list[dict]]:
    candidates = []
    for dimension in DIMENSION_GRID:
        for weight in WEIGHT_GRID:
            rankings = fused_rankings(
                qrels, bm25, dense_by_dimension[dimension], weight
            )
            metrics = aggregate_metrics(corpus, qrels, rankings)
            complement = complement_metrics(qrels, bm25, rankings)
            candidates.append(
                {
                    "dimension": dimension,
                    "dense_weight": weight,
                    "metrics": metrics,
                    "bm25_complement": complement,
                }
            )

    def selection_key(candidate: dict) -> tuple:
        complement = candidate["bm25_complement"]
        ko_en = candidate["metrics"]["per_class"].get("ko_query_en_source", {})
        return (
            complement["rescued_at_5"] - complement["bm25_hit_harmed_at_5"],
            -complement["bm25_hit_harmed_at_5"],
            ko_en.get("recall_at_5", 0.0),
            candidate["metrics"]["weighted_ndcg_at_10"],
            -candidate["dimension"],
            -candidate["dense_weight"],
        )

    selected = max(candidates, key=selection_key)
    return selected, candidates


def snapshot_bytes(model_id: str, revision: str, cache_dir: Path) -> int:
    from huggingface_hub import snapshot_download

    root = Path(
        snapshot_download(
            model_id,
            revision=revision,
            cache_dir=cache_dir,
            local_files_only=True,
        )
    )
    return sum(path.stat().st_size for path in root.rglob("*") if path.is_file())


def rss_high_water_bytes(value: int, system: str) -> int:
    return value if system == "Darwin" else value * 1024


def process_high_water_rss_bytes() -> int:
    usage = resource.getrusage(resource.RUSAGE_SELF)
    return rss_high_water_bytes(int(usage.ru_maxrss), platform.system())


class ResourceProbe:
    def __init__(self, device: str) -> None:
        import psutil

        self.process = psutil.Process()
        self.device = device
        self.peak_rss_bytes = max(
            self.process.memory_info().rss, process_high_water_rss_bytes()
        )
        self.peak_mps_allocated_bytes = 0
        self.peak_mps_driver_bytes = 0

    def observe(self) -> None:
        self.peak_rss_bytes = max(
            self.peak_rss_bytes,
            self.process.memory_info().rss,
            process_high_water_rss_bytes(),
        )
        if self.device == "mps":
            import torch

            self.peak_mps_allocated_bytes = max(
                self.peak_mps_allocated_bytes, torch.mps.current_allocated_memory()
            )
            self.peak_mps_driver_bytes = max(
                self.peak_mps_driver_bytes, torch.mps.driver_allocated_memory()
            )

    def report(self) -> dict:
        self.observe()
        return {
            "peak_rss_bytes": self.peak_rss_bytes,
            "rss_measurement": "os_process_high_water",
            "peak_mps_allocated_bytes": self.peak_mps_allocated_bytes,
            "peak_mps_driver_bytes": self.peak_mps_driver_bytes,
            "quality_rss_limit_bytes": QUALITY_RSS_LIMIT_BYTES,
            "quality_rss_gate_passed": self.peak_rss_bytes <= QUALITY_RSS_LIMIT_BYTES,
        }


def encode_queries_one_by_one(
    model, qrels: list[dict], prompt: str, probe: ResourceProbe
):
    import numpy as np

    values = []
    latencies = []
    for qrel in qrels:
        started = time.perf_counter()
        value = model.encode(
            [qrel["query"]],
            prompt=prompt,
            normalize_embeddings=True,
            batch_size=1,
            show_progress_bar=False,
        )[0]
        latencies.append((time.perf_counter() - started) * 1000)
        values.append(value)
        probe.observe()
    return np.stack(values), latencies


def run_embedding(
    *,
    device: str,
    cache_dir: Path,
    corpus: list[dict],
    dev: list[dict],
    test: list[dict],
    bm25_dev: dict[str, list[str]],
    bm25_test: dict[str, list[str]],
) -> tuple[
    dict, dict, dict[str, list[str]], dict[str, list[str]], dict[str, list[str]]
]:
    from sentence_transformers import SentenceTransformer

    probe = ResourceProbe(device)
    loaded_started = time.perf_counter()
    model = SentenceTransformer(
        EMBEDDING_MODEL_ID,
        revision=EMBEDDING_REVISION,
        device=device,
        cache_folder=str(cache_dir),
        local_files_only=True,
    )
    model.max_seq_length = 1024
    model_load_ms = (time.perf_counter() - loaded_started) * 1000
    probe.observe()
    texts, chunk_source_ids, chunks_by_source = build_chunks(corpus, model.tokenizer)
    prompt = f"Instruct: {TASK_INSTRUCTION}\nQuery:"

    indexing_started = time.perf_counter()
    chunk_embeddings = model.encode(
        texts,
        normalize_embeddings=True,
        batch_size=8,
        show_progress_bar=False,
    )
    indexing_seconds = time.perf_counter() - indexing_started
    probe.observe()

    all_qrels = [*dev, *test]
    query_embeddings, query_latencies = encode_queries_one_by_one(
        model, all_qrels, prompt, probe
    )
    dev_query_embeddings = query_embeddings[: len(dev)]
    test_query_embeddings = query_embeddings[len(dev) :]
    dense_dev_by_dimension = {}
    dense_test_by_dimension = {}
    for dimension in DIMENSION_GRID:
        dimension_chunks = normalize_dimension(chunk_embeddings, dimension)
        dense_dev_by_dimension[dimension] = dense_rankings(
            corpus,
            dev,
            normalize_dimension(dev_query_embeddings, dimension),
            dimension_chunks,
            chunk_source_ids,
        )
        dense_test_by_dimension[dimension] = dense_rankings(
            corpus,
            test,
            normalize_dimension(test_query_embeddings, dimension),
            dimension_chunks,
            chunk_source_ids,
        )

    selected, dev_grid = select_dev_fusion(
        corpus, dev, bm25_dev, dense_dev_by_dimension
    )
    dimension = selected["dimension"]
    weight = selected["dense_weight"]
    hybrid_dev = fused_rankings(
        dev, bm25_dev, dense_dev_by_dimension[dimension], weight
    )
    hybrid_test = fused_rankings(
        test, bm25_test, dense_test_by_dimension[dimension], weight
    )
    report = {
        "model_id": EMBEDDING_MODEL_ID,
        "revision": EMBEDDING_REVISION,
        "instruction": TASK_INSTRUCTION,
        "native_dimension": int(chunk_embeddings.shape[1]),
        "selected_dimension": dimension,
        "selected_dense_weight": weight,
        "dev_grid": dev_grid,
        "dev_metrics": aggregate_metrics(corpus, dev, hybrid_dev),
        "dev_bm25_complement": complement_metrics(dev, bm25_dev, hybrid_dev),
        "screening_metrics": aggregate_metrics(corpus, test, hybrid_test),
        "screening_bm25_complement": complement_metrics(test, bm25_test, hybrid_test),
        "resources": {
            "snapshot_bytes": snapshot_bytes(
                EMBEDDING_MODEL_ID, EMBEDDING_REVISION, cache_dir
            ),
            "model_load_ms": model_load_ms,
            "chunk_count": len(texts),
            "chunk_tokens": CHUNK_TOKENS,
            "chunk_overlap_tokens": CHUNK_OVERLAP,
            "indexing_seconds": indexing_seconds,
            "indexing_chunks_per_second": len(texts) / indexing_seconds,
            "warm_query_sample_count": len(query_latencies),
            "warm_query_p50_ms": percentile(query_latencies, 0.50),
            "warm_query_p95_ms": percentile(query_latencies, 0.95),
            "measured_50k_chunk_count": None,
            "resource_evidence_complete": False,
            **probe.report(),
        },
    }
    del model, chunk_embeddings, query_embeddings
    gc.collect()
    return (
        report,
        chunks_by_source,
        hybrid_dev,
        hybrid_test,
        dense_test_by_dimension[dimension],
    )


def rerank_split(
    *,
    model,
    qrels: list[dict],
    bm25: dict[str, list[str]],
    hybrid: dict[str, list[str]],
    chunks_by_source: dict[str, list[str]],
    probe: ResourceProbe,
) -> tuple[dict[str, list[str]], dict[str, list[str]], list[float], int]:
    bm25_reranked = {}
    hybrid_reranked = {}
    latencies = []
    pair_count = 0
    for qrel in qrels:
        query_id = qrel["query_id"]
        if qrel["class"] == "exact_identifier":
            bm25_reranked[query_id] = list(bm25[query_id])
            hybrid_reranked[query_id] = list(hybrid[query_id])
            continue
        union = list(
            dict.fromkeys(
                [*bm25[query_id][:RERANK_DEPTH], *hybrid[query_id][:RERANK_DEPTH]]
            )
        )
        pairs = []
        pair_sources = []
        for source_id in union:
            for text in chunks_by_source[source_id]:
                pairs.append([qrel["query"], text])
                pair_sources.append(source_id)
        started = time.perf_counter()
        values = model.predict(pairs, batch_size=4, show_progress_bar=False)
        latencies.append((time.perf_counter() - started) * 1000)
        pair_count += len(pairs)
        source_scores: dict[str, float] = {}
        for source_id, value in zip(pair_sources, values):
            source_scores[source_id] = max(
                source_scores.get(source_id, float("-inf")), float(value)
            )
        bm25_reranked[query_id] = apply_reranker(
            bm25[query_id][:RERANK_DEPTH], source_scores, exact=False
        )
        hybrid_reranked[query_id] = apply_reranker(
            hybrid[query_id][:RERANK_DEPTH], source_scores, exact=False
        )
        probe.observe()
    return bm25_reranked, hybrid_reranked, latencies, pair_count


def run_reranker(
    *,
    device: str,
    cache_dir: Path,
    corpus: list[dict],
    dev: list[dict],
    test: list[dict],
    bm25_dev: dict[str, list[str]],
    bm25_test: dict[str, list[str]],
    hybrid_dev: dict[str, list[str]],
    hybrid_test: dict[str, list[str]],
    chunks_by_source: dict[str, list[str]],
) -> tuple[dict, dict[str, list[str]], dict[str, list[str]]]:
    from sentence_transformers import CrossEncoder

    probe = ResourceProbe(device)
    loaded_started = time.perf_counter()
    model = CrossEncoder(
        RERANKER_MODEL_ID,
        revision=RERANKER_REVISION,
        device=device,
        cache_folder=str(cache_dir),
        local_files_only=True,
        max_length=1024,
        prompts={"qgh": TASK_INSTRUCTION},
        default_prompt_name="qgh",
    )
    model_load_ms = (time.perf_counter() - loaded_started) * 1000
    probe.observe()
    dev_bm25_reranked, dev_hybrid_reranked, dev_latencies, dev_pairs = rerank_split(
        model=model,
        qrels=dev,
        bm25=bm25_dev,
        hybrid=hybrid_dev,
        chunks_by_source=chunks_by_source,
        probe=probe,
    )
    test_bm25_reranked, test_hybrid_reranked, test_latencies, test_pairs = rerank_split(
        model=model,
        qrels=test,
        bm25=bm25_test,
        hybrid=hybrid_test,
        chunks_by_source=chunks_by_source,
        probe=probe,
    )
    latencies = [*dev_latencies, *test_latencies]
    elapsed_seconds = sum(latencies) / 1000
    report = {
        "model_id": RERANKER_MODEL_ID,
        "revision": RERANKER_REVISION,
        "instruction": TASK_INSTRUCTION,
        "rerank_depth": RERANK_DEPTH,
        "dev": {
            "bm25_rerank_metrics": aggregate_metrics(corpus, dev, dev_bm25_reranked),
            "bm25_rerank_complement": complement_metrics(
                dev, bm25_dev, dev_bm25_reranked
            ),
            "hybrid_rerank_metrics": aggregate_metrics(
                corpus, dev, dev_hybrid_reranked
            ),
            "hybrid_rerank_complement": complement_metrics(
                dev, bm25_dev, dev_hybrid_reranked
            ),
        },
        "screening": {
            "bm25_rerank_metrics": aggregate_metrics(corpus, test, test_bm25_reranked),
            "bm25_rerank_complement": complement_metrics(
                test, bm25_test, test_bm25_reranked
            ),
            "hybrid_rerank_metrics": aggregate_metrics(
                corpus, test, test_hybrid_reranked
            ),
            "hybrid_rerank_complement": complement_metrics(
                test, bm25_test, test_hybrid_reranked
            ),
        },
        "resources": {
            "snapshot_bytes": snapshot_bytes(
                RERANKER_MODEL_ID, RERANKER_REVISION, cache_dir
            ),
            "model_load_ms": model_load_ms,
            "pair_count": dev_pairs + test_pairs,
            "scoring_seconds": elapsed_seconds,
            "pairs_per_second": (dev_pairs + test_pairs) / elapsed_seconds,
            "query_candidate_pool_p50_ms": percentile(latencies, 0.50),
            "query_candidate_pool_p95_ms": percentile(latencies, 0.95),
            "measured_50k_chunk_count": None,
            "resource_evidence_complete": False,
            **probe.report(),
        },
    }
    del model
    gc.collect()
    return report, test_bm25_reranked, test_hybrid_reranked


def write_report(
    *,
    output_root: Path,
    device: str,
    corpus: list[dict],
    screening_qrels: list[dict],
    report: dict,
    rankings: dict[str, dict[str, list[str]]],
    extra_payloads: tuple[str, ...] = (),
) -> Path:
    output_root.mkdir(parents=True, exist_ok=True)
    events = build_redacted_events(screening_qrels, rankings)
    rendered_report = json.dumps(report, ensure_ascii=False, sort_keys=True)
    rendered_events = (
        "\n".join(
            json.dumps(event, ensure_ascii=False, sort_keys=True) for event in events
        )
        + "\n"
    )
    assert_redacted_payload(
        corpus,
        screening_qrels,
        rendered_report,
        rendered_events,
        *extra_payloads,
    )
    report_path = output_root / f"qwen-benchmark-{device}.json"
    report_path.write_text(
        json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    )
    (output_root / f"qwen-events-{device}.jsonl").write_text(rendered_events)
    return report_path


def command_output(arguments: list[str]) -> str:
    return subprocess.check_output(arguments, text=True).strip()


def capture_git_identity() -> tuple[str, bool]:
    return (
        command_output(["git", "rev-parse", "HEAD"]),
        command_output(["git", "status", "--porcelain"]) == "",
    )


def assert_redacted_payload(
    corpus: list[dict], qrels: list[dict], *payloads: str
) -> None:
    payload = "\n".join(payloads)
    raw_values = [record.get("query", "") for record in qrels]
    raw_values.extend(
        source.get(field, "")
        for source in corpus
        for field in ("title", "body", "snippet")
    )
    if any(value in payload for value in raw_values if len(value) >= 16):
        raise RuntimeError("raw query or source text escaped into benchmark artifacts")
    if any(pattern.search(payload) for pattern in SENSITIVE_OUTPUT_PATTERNS):
        raise RuntimeError("sensitive path, header, or token escaped into artifacts")


def run(args: argparse.Namespace) -> Path:
    started = time.perf_counter()
    git_head, worktree_clean = capture_git_identity()
    if not worktree_clean:
        raise RuntimeError("benchmark requires a clean worktree")
    fixture_root = args.fixture_root
    bm25_root = args.bm25_root
    corpus_path = fixture_root / "corpus.jsonl"
    dev_path = fixture_root / "qrels-dev.jsonl"
    test_path = fixture_root / "qrels-test.jsonl"
    provenance_path = fixture_root / "provenance.json"
    corpus = read_jsonl(corpus_path)
    dev = read_jsonl(dev_path)
    test = read_jsonl(test_path)
    provenance = json.loads(provenance_path.read_text())
    for path, expected in (
        (corpus_path, provenance["corpus_sha256"]),
        (dev_path, provenance["qrels_dev_sha256"]),
        (test_path, provenance["qrels_test_sha256"]),
    ):
        if sha256_file(path) != expected:
            raise RuntimeError("fixture hash does not match provenance")
    bm25_dev = load_bm25_rankings(bm25_root / "dev-events.jsonl", dev)
    bm25_test = load_bm25_rankings(bm25_root / "heldout-events.jsonl", test)
    cache_dir = args.cache_dir
    cache_dir.mkdir(parents=True, exist_ok=True)
    print(f"qwen-benchmark device={args.device} phase=embedding", file=sys.stderr)
    embedding, chunks_by_source, hybrid_dev, hybrid_test, dense_test = run_embedding(
        device=args.device,
        cache_dir=cache_dir,
        corpus=corpus,
        dev=dev,
        test=test,
        bm25_dev=bm25_dev,
        bm25_test=bm25_test,
    )
    print(f"qwen-benchmark device={args.device} phase=reranker", file=sys.stderr)
    reranker, bm25_reranked_test, hybrid_reranked_test = run_reranker(
        device=args.device,
        cache_dir=cache_dir,
        corpus=corpus,
        dev=dev,
        test=test,
        bm25_dev=bm25_dev,
        bm25_test=bm25_test,
        hybrid_dev=hybrid_dev,
        hybrid_test=hybrid_test,
        chunks_by_source=chunks_by_source,
    )
    report = {
        "schema_version": REPORT_SCHEMA_VERSION,
        "evaluation_state": "screening_only_previously_opened_heldout",
        "promotion_eligible": False,
        "promotion_blockers": [
            "heldout_previously_opened",
            "production_runtime_adapter_not_implemented",
            "50k_resource_gate_not_measured",
        ],
        "git_head": git_head,
        "worktree_clean_at_start": worktree_clean,
        "device": args.device,
        "host": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
        },
        "runtime": {
            name: importlib.metadata.version(name)
            for name in (
                "torch",
                "transformers",
                "sentence-transformers",
                "numpy",
                "psutil",
            )
        },
        "inputs": {
            "snapshot_at": provenance["snapshot_at"],
            "corpus_count": len(corpus),
            "dev_query_count": len(dev),
            "screening_query_count": len(test),
            "corpus_sha256": sha256_file(corpus_path),
            "qrels_dev_sha256": sha256_file(dev_path),
            "qrels_screening_sha256": sha256_file(test_path),
            "bm25_dev_events_sha256": sha256_file(bm25_root / "dev-events.jsonl"),
            "bm25_screening_events_sha256": sha256_file(
                bm25_root / "heldout-events.jsonl"
            ),
        },
        "bm25": {
            "dev_metrics": aggregate_metrics(corpus, dev, bm25_dev),
            "screening_metrics": aggregate_metrics(corpus, test, bm25_test),
        },
        "embedding": embedding,
        "reranker": reranker,
        "total_runtime_seconds": time.perf_counter() - started,
    }
    final_git_head, final_worktree_clean = capture_git_identity()
    if final_git_head != git_head or not final_worktree_clean:
        raise RuntimeError("Git identity changed during benchmark execution")
    report["raw_query_or_body_logged"] = False
    report["absolute_path_logged"] = False
    args.output_root.mkdir(parents=True, exist_ok=True)
    checkpoint_path = args.output_root / f"qwen-benchmark-{args.device}.checkpoint.json"
    checkpoint = {
        "schema_version": CHECKPOINT_SCHEMA_VERSION,
        "report": report,
        "screening_rankings": {
            "bm25": bm25_test,
            "qwen_dense": dense_test,
            "qwen_hybrid": hybrid_test,
            "qwen_bm25_rerank": bm25_reranked_test,
            "qwen_hybrid_rerank": hybrid_reranked_test,
        },
    }
    rendered_checkpoint = (
        json.dumps(
            checkpoint,
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
        )
        + "\n"
    )
    report_path = write_report(
        output_root=args.output_root,
        device=args.device,
        corpus=corpus,
        screening_qrels=test,
        report=report,
        rankings={
            "bm25": bm25_test,
            "qwen_dense": dense_test,
            "qwen_hybrid": hybrid_test,
            "qwen_bm25_rerank": bm25_reranked_test,
            "qwen_hybrid_rerank": hybrid_reranked_test,
        },
        extra_payloads=(rendered_checkpoint,),
    )
    checkpoint_path.write_text(rendered_checkpoint)
    print(
        json.dumps(
            {
                "schema_version": report["schema_version"],
                "device": args.device,
                "report_sha256": sha256_file(report_path),
            },
            sort_keys=True,
        )
    )
    return report_path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixture-root", type=Path, required=True)
    parser.add_argument("--bm25-root", type=Path, required=True)
    parser.add_argument("--output-root", type=Path, required=True)
    parser.add_argument("--cache-dir", type=Path, required=True)
    parser.add_argument("--device", choices=("cpu", "mps"), required=True)
    return parser.parse_args()


if __name__ == "__main__":
    run(parse_args())
