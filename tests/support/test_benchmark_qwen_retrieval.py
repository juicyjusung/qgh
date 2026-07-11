#!/usr/bin/env python3

from __future__ import annotations

import copy
import importlib.util
import json
import math
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("benchmark_qwen_retrieval.py")
MODULE_SPEC = importlib.util.spec_from_file_location("qwen_benchmark", MODULE_PATH)
assert MODULE_SPEC is not None and MODULE_SPEC.loader is not None
BENCHMARK = importlib.util.module_from_spec(MODULE_SPEC)
MODULE_SPEC.loader.exec_module(BENCHMARK)


class QwenBenchmarkContractTests(unittest.TestCase):
    def test_document_context_is_deterministic_without_mutating_public_corpus(
        self,
    ) -> None:
        source = {
            "entity_type": "issue_comment",
            "repo": "example/public",
            "issue_number": 47,
            "title": "Comment on issue #47: Search publication",
            "body": "Public comment body",
        }
        before = copy.deepcopy(source)

        first = BENCHMARK.build_document_text(source)
        second = BENCHMARK.build_document_text(source)

        self.assertEqual(first, second)
        self.assertIn("Repository: example/public", first)
        self.assertIn("Parent issue: #47", first)
        self.assertIn("Parent title: Search publication", first)
        self.assertTrue(first.endswith("Public comment body"))
        self.assertEqual(source, before)

    def test_weighted_rrf_preserves_exact_route_and_fuses_semantic_candidates(
        self,
    ) -> None:
        bm25 = ["source-a", "source-b", "source-c"]
        dense = ["source-c", "source-d"]

        self.assertEqual(
            BENCHMARK.weighted_rrf(bm25, dense, dense_weight=0.5, exact=True),
            bm25,
        )
        self.assertEqual(
            BENCHMARK.weighted_rrf(bm25, dense, dense_weight=0.5, exact=False),
            ["source-c", "source-a", "source-b", "source-d"],
        )

    def test_reranker_only_reorders_the_frozen_candidate_pool(self) -> None:
        candidates = ["source-a", "source-b", "source-c"]
        scores = {
            "source-a": 0.1,
            "source-b": 0.9,
            "source-c": 0.5,
            "outside-filtered-pool": 1.0,
        }

        self.assertEqual(
            BENCHMARK.apply_reranker(candidates, scores, exact=False),
            ["source-b", "source-c", "source-a"],
        )
        self.assertEqual(
            BENCHMARK.apply_reranker(candidates, scores, exact=True), candidates
        )

    def test_query_metrics_match_the_frozen_live_eval_contract(self) -> None:
        metrics = BENCHMARK.query_metrics(
            [{"source_id": "relevant", "grade": 3}],
            ["distractor", "relevant"],
        )

        self.assertTrue(math.isclose(metrics["ndcg_at_10"], 1 / math.log2(3)))
        self.assertEqual(metrics["mrr_at_10"], 0.5)
        self.assertEqual(metrics["recall_at_5"], 1.0)
        self.assertEqual(metrics["recall_at_10"], 1.0)

    def test_machine_events_hash_queries_instead_of_logging_them(self) -> None:
        raw_query = "private-looking query must not be logged"
        event = BENCHMARK.redacted_event(
            query_id="test-001",
            query_class="english_semantic",
            query=raw_query,
            rankings={"qwen_hybrid": ["source-a"]},
        )

        rendered = json.dumps(event)
        self.assertNotIn(raw_query, rendered)
        self.assertEqual(
            event["query_sha256"],
            "f66ed72b8284e70d2c1bff3f906910cbf29a26219115aa016593f1629ab6a133",
        )

    def test_artifact_redaction_fails_closed_on_raw_text_and_paths(self) -> None:
        corpus = [
            {
                "title": "Public issue title long enough to audit",
                "body": "Public issue body long enough to audit",
            }
        ]
        qrels = [{"query": "Public query long enough to audit"}]

        for escaped in (
            corpus[0]["title"],
            corpus[0]["body"],
            qrels[0]["query"],
            "/Users/example/private/cache",
            "Authorization: Bearer redacted",
            "github_pat_abcdefghijklmnopqrstuvwxyz123456",
        ):
            with self.subTest(escaped=escaped[:16]):
                with self.assertRaises(RuntimeError):
                    BENCHMARK.assert_redacted_payload(corpus, qrels, escaped)

        BENCHMARK.assert_redacted_payload(
            corpus,
            qrels,
            '{"query_sha256":"abc","source_id":"public-source"}',
        )

    def test_os_high_water_rss_is_normalized_to_bytes(self) -> None:
        self.assertEqual(BENCHMARK.rss_high_water_bytes(1234, "Darwin"), 1234)
        self.assertEqual(BENCHMARK.rss_high_water_bytes(1234, "Linux"), 1263616)

    def test_screening_events_do_not_require_dev_rankings(self) -> None:
        screening = [
            {
                "query_id": "test-001",
                "query": "public screening query",
                "class": "english_semantic",
            }
        ]

        events = BENCHMARK.build_redacted_events(
            screening,
            {"qwen_hybrid": {"test-001": ["source-a"]}},
        )

        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["query_id"], "test-001")


if __name__ == "__main__":
    unittest.main()
