#!/usr/bin/env python3

from __future__ import annotations

import contextlib
import copy
import hashlib
import importlib.util
import io
import json
import tempfile
import unittest
from collections import Counter
from pathlib import Path
from unittest import mock
from urllib.parse import parse_qs, quote, urlparse


MODULE_PATH = Path(__file__).with_name("build_fresh_blind_model_eval_fixture.py")
MODULE_SPEC = importlib.util.spec_from_file_location("fresh_blind_builder", MODULE_PATH)
assert MODULE_SPEC is not None and MODULE_SPEC.loader is not None
BUILDER = importlib.util.module_from_spec(MODULE_SPEC)
MODULE_SPEC.loader.exec_module(BUILDER)


CLASS_COUNTS = {
    "english_semantic": 20,
    "korean_semantic": 15,
    "ko_query_en_source": 10,
    "en_query_ko_source": 10,
    "exact_identifier": 10,
    "comment_only": 5,
    "long_context": 5,
    "negative": 5,
}


def source_id(kind: str, node_id: str) -> str:
    return f"qgh://github.com/{kind}/{quote(node_id, safe='')}"


def issue(
    repo: str, number: int, node_id: str, body: str = "Safe public issue body"
) -> dict:
    return {
        "url": f"https://api.github.com/repos/{repo}/issues/{number}",
        "html_url": f"https://github.com/{repo}/issues/{number}",
        "node_id": node_id,
        "number": number,
        "title": f"Public issue {number}",
        "body": body,
        "updated_at": "2026-07-11T00:00:00Z",
    }


def comment(repo: str, issue_number: int, comment_id: int, node_id: str) -> dict:
    return {
        "url": f"https://api.github.com/repos/{repo}/issues/comments/{comment_id}",
        "html_url": f"https://github.com/{repo}/issues/{issue_number}#issuecomment-{comment_id}",
        "issue_url": f"https://api.github.com/repos/{repo}/issues/{issue_number}",
        "node_id": node_id,
        "id": comment_id,
        "body": "Safe public comment body",
        "updated_at": "2026-07-11T00:00:00Z",
    }


def jsonl_bytes(rows: list[dict]) -> bytes:
    return "".join(
        json.dumps(row, ensure_ascii=False, sort_keys=True) + "\n" for row in rows
    ).encode()


def build_dev_rows() -> tuple[list[dict], list[dict], list[dict]]:
    issues = [
        issue("juicyjusung/qgh", number, f"QGH/{number}") for number in range(1, 35)
    ]
    comments = [
        comment("juicyjusung/qgh", number, 8000 + number, f"QGHC/{number}")
        for number in range(1, 5)
    ]
    rows = []
    for index, number in enumerate(range(5, 35), 1):
        rows.append(
            {
                "schema_version": "qgh.live_model_qrel.v1",
                "query_id": f"dev-{index:03d}",
                "split": "dev",
                "query": f"Synthetic dev query {index}",
                "class": "english_semantic",
                "relevant": [
                    {
                        "source_id": source_id("issue", f"QGH/{number}"),
                        "grade": 3,
                        "rationale": "Synthetic public dev judgment.",
                    }
                ],
                "filters": {"repo": "juicyjusung/qgh"},
                "rationale": "Synthetic public dev judgment.",
                "labeler": "test labeler",
                "adjudicators": ["test labeler", "test adjudicator"],
                "ambiguous": False,
                "second_adjudication": None,
            }
        )
    for number in range(1, 5):
        index = len(rows) + 1
        rows.append(
            {
                "schema_version": "qgh.live_model_qrel.v1",
                "query_id": f"dev-{index:03d}",
                "split": "dev",
                "query": f"Synthetic comment query {number}",
                "class": "comment_only",
                "relevant": [
                    {
                        "source_id": source_id("issue-comment", f"QGHC/{number}"),
                        "grade": 3,
                        "rationale": "Synthetic public dev comment judgment.",
                    }
                ],
                "filters": {"repo": "juicyjusung/qgh", "source_type": "issue_comment"},
                "rationale": "Synthetic public dev comment judgment.",
                "labeler": "test labeler",
                "adjudicators": ["test labeler", "test adjudicator"],
                "ambiguous": False,
                "second_adjudication": None,
            }
        )
    while len(rows) < 40:
        index = len(rows) + 1
        rows.append(
            {
                "schema_version": "qgh.live_model_qrel.v1",
                "query_id": f"dev-{index:03d}",
                "split": "dev",
                "query": f"Synthetic negative query {index}",
                "class": "negative",
                "relevant": [],
                "filters": {"repo": "juicyjusung/qgh"},
                "rationale": "No relevant source.",
                "labeler": "test labeler",
                "adjudicators": ["test labeler", "test adjudicator"],
                "ambiguous": False,
                "second_adjudication": None,
            }
        )
    return rows, issues, comments


def build_spec() -> dict:
    qrels = []
    for query_class, count in CLASS_COUNTS.items():
        for _ in range(count):
            index = len(qrels) + 1
            gold = []
            if query_class == "comment_only":
                gold = [
                    {
                        "repo": "example/public-repo",
                        "issue_number": 102,
                        "comment_id": 9001,
                        "grade": 3,
                    }
                ]
            elif query_class != "negative":
                gold = [
                    {
                        "repo": "example/public-repo",
                        "issue_number": 101,
                        "grade": 3,
                    }
                ]
            qrels.append(
                {
                    "query_id": f"test-{index:03d}",
                    "query": f"Fresh blind query {index}",
                    "class": query_class,
                    "repo": "example/public-repo",
                    "rationale": "The public gold source directly covers the information need.",
                    "gold": gold,
                }
            )
    return {
        "schema_version": "qgh.fresh_blind_model_eval_spec.v2",
        "dev_repo": "juicyjusung/qgh",
        "distractor_limit": 10,
        "pooled_query_ids": [f"test-{index:03d}" for index in range(1, 11)],
        "repositories": [
            {
                "repo": "juicyjusung/qgh",
                "license": "NOASSERTION",
                "license_url": "https://github.com/juicyjusung/qgh",
            },
            {
                "repo": "example/public-repo",
                "license": "Apache-2.0",
                "license_url": "https://github.com/example/public-repo/blob/main/LICENSE",
            },
        ],
        "qrels": qrels,
    }


class FakeGitHub:
    def __init__(self, qgh_issues: list[dict], qgh_comments: list[dict]):
        self.qgh_issues = qgh_issues
        self.qgh_comments = qgh_comments
        self.calls: list[str] = []
        self.external_issues = {
            100: issue("example/public-repo", 100, "EXT/100"),
            101: issue("example/public-repo", 101, "EXT/101"),
            102: issue("example/public-repo", 102, "EXT/102"),
        }
        self.external_comment = comment("example/public-repo", 102, 9001, "EXTC/9001")

    def __call__(self, url: str):
        self.calls.append(url)
        parsed = urlparse(url)
        if parsed.scheme != "https" or parsed.netloc != "api.github.com":
            raise AssertionError("builder called a non-public API endpoint")
        query = parse_qs(parsed.query)
        page = int(query.get("page", ["1"])[0])
        if parsed.path == "/repos/juicyjusung/qgh/issues":
            offset = (page - 1) * 100
            return copy.deepcopy(self.qgh_issues[offset : offset + 100])
        if parsed.path == "/repos/juicyjusung/qgh/issues/comments":
            offset = (page - 1) * 100
            return copy.deepcopy(self.qgh_comments[offset : offset + 100])
        if parsed.path == "/repos/example/public-repo/issues":
            pr = issue("example/public-repo", 200, "PR/200")
            pr["pull_request"] = {
                "url": "https://api.github.com/repos/example/public-repo/pulls/200"
            }
            secret_like = "gh" + "p_" + ("x" * 24)
            local_path = "/" + "Users" + "/example/private"
            return [
                copy.deepcopy(pr),
                issue("example/public-repo", 201, "EMPTY/201", body=""),
                issue("example/public-repo", 202, "SECRET/202", body=secret_like),
                issue("example/public-repo", 203, "PATH/203", body=f"see {local_path}"),
                copy.deepcopy(self.external_issues[100]),
                copy.deepcopy(self.external_issues[101]),
            ]
        if parsed.path == "/repos/example/public-repo/issues/102":
            return copy.deepcopy(self.external_issues[102])
        if parsed.path == "/repos/example/public-repo/issues/comments/9001":
            return copy.deepcopy(self.external_comment)
        raise AssertionError(f"unexpected public API path: {parsed.path}")


class FreshBlindBuilderTests(unittest.TestCase):
    def test_builds_split_safe_fixture_with_exact_provenance(self) -> None:
        dev_rows, qgh_issues, qgh_comments = build_dev_rows()
        fake = FakeGitHub(qgh_issues, qgh_comments)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spec_path = root / "spec.json"
            dev_path = root / "qrels-dev.jsonl"
            output_dir = root / "target" / "qgh-eval" / "fresh-blind"
            spec_path.write_text(json.dumps(build_spec()), encoding="utf-8")
            dev_bytes = jsonl_bytes(dev_rows)
            dev_path.write_bytes(dev_bytes)
            stdout = io.StringIO()
            stderr = io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                BUILDER.build_fixture(
                    spec_path=spec_path,
                    dev_qrels_path=dev_path,
                    output_dir=output_dir,
                    snapshot_at="2026-07-11T01:02:03Z",
                    fetch_json=fake,
                )

            self.assertEqual(stdout.getvalue(), "")
            self.assertEqual(stderr.getvalue(), "")
            self.assertEqual((output_dir / "qrels-dev.jsonl").read_bytes(), dev_bytes)
            test_rows = [
                json.loads(line)
                for line in (output_dir / "qrels-test.jsonl").read_text().splitlines()
            ]
            corpus_rows = [
                json.loads(line)
                for line in (output_dir / "corpus.jsonl").read_text().splitlines()
            ]
            provenance = json.loads((output_dir / "provenance.json").read_text())

            self.assertEqual(len(test_rows), 80)
            self.assertEqual(Counter(row["class"] for row in test_rows), CLASS_COUNTS)
            self.assertTrue(all(row["split"] == "test" for row in test_rows))
            self.assertIn(
                source_id("issue", "EXT/101"), {row["source_id"] for row in corpus_rows}
            )
            comment_row = next(
                row
                for row in corpus_rows
                if row["entity_type"] == "issue_comment"
                and row["repo"] == "example/public-repo"
            )
            self.assertEqual(
                comment_row["source_id"], source_id("issue-comment", "EXTC/9001")
            )
            self.assertTrue(
                any(
                    row["repo"] == "example/public-repo"
                    and row["issue_number"] == 102
                    and row["entity_type"] == "issue"
                    for row in corpus_rows
                )
            )
            self.assertNotIn("PR/200", "".join(row["source_id"] for row in corpus_rows))
            self.assertNotIn(
                "SECRET/202", "".join(row["source_id"] for row in corpus_rows)
            )
            self.assertNotIn(
                "PATH/203", "".join(row["source_id"] for row in corpus_rows)
            )

            self.assertEqual(
                provenance["schema_version"], "qgh.live_model_provenance.v1"
            )
            self.assertEqual(provenance["acquisition"]["authentication"], "none")
            self.assertEqual(provenance["acquisition"]["raw_response_committed"], False)
            self.assertEqual(provenance["dev_query_count"], 40)
            self.assertEqual(provenance["test_query_count"], 80)
            self.assertEqual(
                provenance["judgment_pool"]["multi_source_query_count"], 10
            )
            self.assertEqual(
                set(provenance),
                {
                    "schema_version",
                    "snapshot_at",
                    "acquisition",
                    "repositories",
                    "exclusions",
                    "exclusion_counts",
                    "adjudication",
                    "judgment_pool",
                    "corpus_sha256",
                    "qrels_dev_sha256",
                    "qrels_test_sha256",
                    "dev_query_count",
                    "test_query_count",
                },
            )
            repo_counts = {
                row["repo"]: row["source_count"] for row in provenance["repositories"]
            }
            self.assertEqual(repo_counts, Counter(row["repo"] for row in corpus_rows))
            for filename, key in [
                ("corpus.jsonl", "corpus_sha256"),
                ("qrels-dev.jsonl", "qrels_dev_sha256"),
                ("qrels-test.jsonl", "qrels_test_sha256"),
            ]:
                payload = (output_dir / filename).read_bytes()
                self.assertTrue(payload.endswith(b"\n"))
                self.assertEqual(provenance[key], hashlib.sha256(payload).hexdigest())

            dev_threads = {
                (row["repo"], row["issue_number"])
                for row in corpus_rows
                if row["source_id"]
                in {
                    relevant["source_id"]
                    for qrel in dev_rows
                    for relevant in qrel["relevant"]
                }
            }
            test_threads = {
                (row["repo"], row["issue_number"])
                for row in corpus_rows
                if row["source_id"]
                in {
                    relevant["source_id"]
                    for qrel in test_rows
                    for relevant in qrel["relevant"]
                }
            }
            self.assertTrue(dev_threads.isdisjoint(test_threads))

    def test_rejects_required_secret_like_gold_without_disclosure(self) -> None:
        dev_rows, qgh_issues, qgh_comments = build_dev_rows()
        fake = FakeGitHub(qgh_issues, qgh_comments)
        secret = "gh" + "p_" + ("x" * 24)
        original_call = fake.__call__

        def secret_gold(url: str):
            parsed = urlparse(url)
            if parsed.path == "/repos/example/public-repo/issues":
                return [copy.deepcopy(fake.external_issues[100])]
            if parsed.path == "/repos/example/public-repo/issues/101":
                return issue("example/public-repo", 101, "EXT/101", body=secret)
            return original_call(url)

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spec_path = root / "spec.json"
            dev_path = root / "qrels-dev.jsonl"
            output_dir = root / "target" / "qgh-eval" / "fresh-blind"
            spec = build_spec()
            query_text = spec["qrels"][0]["query"]
            spec_path.write_text(json.dumps(spec), encoding="utf-8")
            dev_path.write_bytes(jsonl_bytes(dev_rows))
            stdout = io.StringIO()
            stderr = io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                with self.assertRaises(BUILDER.FixtureBuildError) as raised:
                    BUILDER.build_fixture(
                        spec_path=spec_path,
                        dev_qrels_path=dev_path,
                        output_dir=output_dir,
                        snapshot_at="2026-07-11T01:02:03Z",
                        fetch_json=secret_gold,
                    )
            rendered = str(raised.exception)
            self.assertNotIn(secret, rendered)
            self.assertNotIn(query_text, rendered)
            self.assertEqual(stdout.getvalue(), "")
            self.assertEqual(stderr.getvalue(), "")
            self.assertFalse(output_dir.exists())

    def test_rejects_dev_test_issue_thread_leakage(self) -> None:
        dev_rows, qgh_issues, qgh_comments = build_dev_rows()
        fake = FakeGitHub(qgh_issues, qgh_comments)
        spec = build_spec()
        spec["qrels"][0]["repo"] = "juicyjusung/qgh"
        spec["qrels"][0]["gold"] = [
            {"repo": "juicyjusung/qgh", "issue_number": 5, "grade": 3}
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spec_path = root / "spec.json"
            dev_path = root / "qrels-dev.jsonl"
            output_dir = root / "target" / "qgh-eval" / "fresh-blind"
            spec_path.write_text(json.dumps(spec), encoding="utf-8")
            dev_path.write_bytes(jsonl_bytes(dev_rows))
            with self.assertRaisesRegex(
                BUILDER.FixtureBuildError, "issue thread leaks"
            ):
                BUILDER.build_fixture(
                    spec_path=spec_path,
                    dev_qrels_path=dev_path,
                    output_dir=output_dir,
                    snapshot_at="2026-07-11T01:02:03Z",
                    fetch_json=fake,
                )
            self.assertFalse(output_dir.exists())

    def test_rejects_invalid_manual_judgment_pool(self) -> None:
        dev_rows, qgh_issues, qgh_comments = build_dev_rows()
        fake = FakeGitHub(qgh_issues, qgh_comments)
        spec = build_spec()
        spec["pooled_query_ids"] = ["test-001"] * 10
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spec_path = root / "spec.json"
            dev_path = root / "qrels-dev.jsonl"
            output_dir = root / "target" / "qgh-eval" / "fresh-blind"
            spec_path.write_text(json.dumps(spec), encoding="utf-8")
            dev_path.write_bytes(jsonl_bytes(dev_rows))
            with self.assertRaisesRegex(
                BUILDER.FixtureBuildError, "pooled_query_ids"
            ):
                BUILDER.build_fixture(
                    spec_path=spec_path,
                    dev_qrels_path=dev_path,
                    output_dir=output_dir,
                    snapshot_at="2026-07-11T01:02:03Z",
                    fetch_json=fake,
                )
            self.assertFalse(output_dir.exists())

    def test_public_rest_adapter_is_unauthenticated_and_identified(self) -> None:
        class Response:
            def __enter__(self):
                return self

            def __exit__(self, _kind, _value, _traceback):
                return False

            def geturl(self):
                return "https://api.github.com/repos/example/public-repo/issues/1"

            def read(self, _limit):
                return b'{"ok":true}'

        with mock.patch.object(
            BUILDER.urllib.request, "urlopen", return_value=Response()
        ) as urlopen:
            payload = BUILDER.GitHubRestClient().get_json(
                "https://api.github.com/repos/example/public-repo/issues/1"
            )
        self.assertEqual(payload, {"ok": True})
        request = urlopen.call_args.args[0]
        headers = {key.lower(): value for key, value in request.header_items()}
        self.assertNotIn("authorization", headers)
        self.assertEqual(headers["user-agent"], BUILDER.USER_AGENT)
        self.assertNotRegex(headers["user-agent"], r"gh[pousr]_|github_pat_|sk-")

    def test_paginates_full_dev_issue_and_comment_collections(self) -> None:
        dev_rows, qgh_issues, qgh_comments = build_dev_rows()
        qgh_issues.extend(
            issue("juicyjusung/qgh", number, f"QGH/{number}")
            for number in range(35, 102)
        )
        qgh_comments.extend(
            comment("juicyjusung/qgh", number, 8000 + number, f"QGHC/{number}")
            for number in range(5, 102)
        )
        fake = FakeGitHub(qgh_issues, qgh_comments)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spec_path = root / "spec.json"
            dev_path = root / "qrels-dev.jsonl"
            output_dir = root / "target" / "qgh-eval" / "fresh-blind"
            spec_path.write_text(json.dumps(build_spec()), encoding="utf-8")
            dev_path.write_bytes(jsonl_bytes(dev_rows))
            BUILDER.build_fixture(
                spec_path=spec_path,
                dev_qrels_path=dev_path,
                output_dir=output_dir,
                snapshot_at="2026-07-11T01:02:03Z",
                fetch_json=fake,
            )
            provenance = json.loads((output_dir / "provenance.json").read_text())
            counts = {
                row["repo"]: row["source_count"] for row in provenance["repositories"]
            }
            self.assertEqual(counts["juicyjusung/qgh"], 202)
            self.assertTrue(
                any(
                    "/repos/juicyjusung/qgh/issues?" in url and "page=2" in url
                    for url in fake.calls
                )
            )
            self.assertTrue(
                any(
                    "/repos/juicyjusung/qgh/issues/comments?" in url and "page=2" in url
                    for url in fake.calls
                )
            )


if __name__ == "__main__":
    unittest.main()
