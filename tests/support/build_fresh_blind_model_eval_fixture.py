#!/usr/bin/env python3
"""Build a machine-only fresh blind live-model fixture from public GitHub data.

The external interface is deliberately small: a strict JSON specification, the
existing dev qrels JSONL, an output directory under ``target/qgh-eval``, and a
UTC snapshot timestamp.  Raw GitHub responses are held in memory only.
"""

from __future__ import annotations

import argparse
import hashlib
import ipaddress
import json
import re
import urllib.error
import urllib.request
from collections import Counter
from pathlib import Path
from typing import Callable
from urllib.parse import quote, urlparse


SPEC_SCHEMA = "qgh.fresh_blind_model_eval_spec.v2"
CORPUS_SCHEMA = "qgh.live_model_corpus.v1"
QREL_SCHEMA = "qgh.live_model_qrel.v1"
PROVENANCE_SCHEMA = "qgh.live_model_provenance.v1"
USER_AGENT = "qgh-public-blind-eval-builder/1.0 (+https://github.com/juicyjusung/qgh)"
PRIMARY_LABELER = "qgh fresh blind public-corpus labeler"
SECONDARY_LABELER = "qgh fresh blind public-corpus adjudicator"
MAX_RESPONSE_BYTES = 32 * 1024 * 1024

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
QREL_KEYS = {
    "schema_version",
    "query_id",
    "split",
    "query",
    "class",
    "relevant",
    "filters",
    "rationale",
    "labeler",
    "adjudicators",
    "ambiguous",
    "second_adjudication",
}
RELEVANT_KEYS = {"source_id", "grade", "rationale"}
FILTER_KEYS = {"repo", "source_type", "issue_number"}
REPO_PATTERN = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
QUERY_ID_PATTERN = re.compile(r"^test-[0-9]{3}$")
SOURCE_ID_PATTERN = re.compile(r"^qgh://github\.com/(issue|issue-comment)/[^/]+$")
UTC_TIMESTAMP_PATTERN = re.compile(
    r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$"
)
ABSOLUTE_USER_PATH = re.compile(r"/Users/")
SECRET_PATTERNS = (
    re.compile(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b"),
    re.compile(r"\bgithub_pat_[A-Za-z0-9_]{20,}\b"),
    re.compile(r"\bAKIA[0-9A-Z]{16}\b"),
    re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
    re.compile(r"\bsk-[A-Za-z0-9]{20,}\b"),
)


class FixtureBuildError(ValueError):
    """A safe-to-display fixture contract failure."""


class UnsafeSource(ValueError):
    """An internal source exclusion that never carries source content."""

    def __init__(self, reason: str):
        super().__init__(reason)
        self.reason = reason


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def source_id(kind: str, node_id: str) -> str:
    return f"qgh://github.com/{kind}/{quote(node_id, safe='')}"


def _require_exact_keys(value: dict, keys: set[str], context: str) -> None:
    if set(value) != keys:
        raise FixtureBuildError(f"{context} has an invalid field set")


def _require_string(value: object, context: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise FixtureBuildError(f"{context} must be a non-empty string")
    return value


def _assert_safe_text(value: str, context: str) -> None:
    if ABSOLUTE_USER_PATH.search(value):
        raise FixtureBuildError(f"{context} contains a forbidden local path")
    if any(pattern.search(value) for pattern in SECRET_PATTERNS):
        raise FixtureBuildError(f"{context} contains secret-like text")


def _assert_source_text(value: object, reason_prefix: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise UnsafeSource(f"empty_{reason_prefix}")
    if ABSOLUTE_USER_PATH.search(value):
        raise UnsafeSource("absolute_local_path")
    if any(pattern.search(value) for pattern in SECRET_PATTERNS):
        raise UnsafeSource("secret_like")
    return value


def _validate_repo(value: object, context: str) -> str:
    repo = _require_string(value, context)
    if not REPO_PATTERN.fullmatch(repo):
        raise FixtureBuildError(f"{context} must be an owner/repository slug")
    return repo


def _validate_public_https_url(value: object, context: str) -> str:
    url = _require_string(value, context)
    _assert_safe_text(url, context)
    parsed = urlparse(url)
    if (
        parsed.scheme != "https"
        or not parsed.hostname
        or parsed.username
        or parsed.password
    ):
        raise FixtureBuildError(f"{context} must be a public HTTPS URL")
    host = parsed.hostname.lower()
    if host == "localhost" or host.endswith(".local"):
        raise FixtureBuildError(f"{context} must be a public HTTPS URL")
    try:
        address = ipaddress.ip_address(host)
    except ValueError:
        address = None
    if address is not None and not address.is_global:
        raise FixtureBuildError(f"{context} must be a public HTTPS URL")
    return url


def _expected_issue_html_url(repo: str, number: int) -> str:
    return f"https://github.com/{repo}/issues/{number}"


def _expected_comment_html_url(repo: str, number: int, comment_id: int) -> str:
    return f"{_expected_issue_html_url(repo, number)}#issuecomment-{comment_id}"


def _api_url(repo: str, suffix: str, query_string: str | None = None) -> str:
    base = f"https://api.github.com/repos/{repo}/{suffix}"
    return f"{base}?{query_string}" if query_string else base


class GitHubRestClient:
    """Unauthenticated public GitHub REST adapter."""

    def get_json(self, url: str):
        parsed = urlparse(url)
        if parsed.scheme != "https" or parsed.hostname != "api.github.com":
            raise FixtureBuildError("GitHub request target is not the public REST API")
        request = urllib.request.Request(
            url,
            headers={
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
                "User-Agent": USER_AGENT,
            },
            method="GET",
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                final = urlparse(response.geturl())
                if final.scheme != "https" or final.hostname != "api.github.com":
                    raise FixtureBuildError(
                        "GitHub REST redirect left the public API host"
                    )
                payload = response.read(MAX_RESPONSE_BYTES + 1)
        except (urllib.error.URLError, TimeoutError) as error:
            raise FixtureBuildError("public GitHub REST request failed") from error
        if len(payload) > MAX_RESPONSE_BYTES:
            raise FixtureBuildError(
                "public GitHub REST response exceeded the size limit"
            )
        try:
            return json.loads(payload)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise FixtureBuildError(
                "public GitHub REST response was not valid JSON"
            ) from error


def _read_json(path: Path, context: str):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise FixtureBuildError(f"{context} is not readable strict JSON") from error


def _parse_dev_qrels(path: Path, dev_repo: str) -> tuple[bytes, list[dict]]:
    try:
        raw = path.read_bytes()
    except OSError as error:
        raise FixtureBuildError("dev qrels are not readable") from error
    if not raw.endswith(b"\n"):
        raise FixtureBuildError("dev qrels must end with one JSONL newline")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise FixtureBuildError("dev qrels are not UTF-8") from error
    rows = []
    for line_number, line in enumerate(text.splitlines(), 1):
        if not line:
            raise FixtureBuildError("dev qrels contain an empty JSONL record")
        try:
            row = json.loads(line)
        except json.JSONDecodeError as error:
            raise FixtureBuildError(
                f"dev qrel line {line_number} is invalid JSON"
            ) from error
        _validate_qrel(row, "dev", dev_repo, f"dev qrel line {line_number}")
        rows.append(row)
    if len(rows) != 40:
        raise FixtureBuildError("dev qrels must contain exactly 40 records")
    query_ids = [row["query_id"] for row in rows]
    if len(query_ids) != len(set(query_ids)):
        raise FixtureBuildError("dev qrels contain duplicate query IDs")
    return raw, rows


def _validate_qrel(row: object, split: str, repo: str, context: str) -> None:
    if not isinstance(row, dict):
        raise FixtureBuildError(f"{context} must be an object")
    _require_exact_keys(row, QREL_KEYS, context)
    if row["schema_version"] != QREL_SCHEMA or row["split"] != split:
        raise FixtureBuildError(f"{context} has an invalid schema or split")
    query_id = _require_string(row["query_id"], f"{context} query_id")
    _assert_safe_text(query_id, f"{context} query_id")
    query = _require_string(row["query"], f"{context} query")
    _assert_safe_text(query, f"{context} query")
    query_class = _require_string(row["class"], f"{context} class")
    if query_class not in CLASS_COUNTS:
        raise FixtureBuildError(f"{context} has an unsupported class")
    rationale = _require_string(row["rationale"], f"{context} rationale")
    _assert_safe_text(rationale, f"{context} rationale")
    labeler = _require_string(row["labeler"], f"{context} labeler")
    _assert_safe_text(labeler, f"{context} labeler")
    if not isinstance(row["adjudicators"], list) or len(row["adjudicators"]) < 2:
        raise FixtureBuildError(f"{context} adjudicators must contain two reviewers")
    for adjudicator in row["adjudicators"]:
        name = _require_string(adjudicator, f"{context} adjudicator")
        _assert_safe_text(name, f"{context} adjudicator")
    if row["ambiguous"] is not False or row["second_adjudication"] is not None:
        raise FixtureBuildError(f"{context} must be unambiguous")
    filters = row["filters"]
    if not isinstance(filters, dict) or not set(filters).issubset(FILTER_KEYS):
        raise FixtureBuildError(f"{context} filters are invalid")
    if filters.get("repo") != repo:
        raise FixtureBuildError(f"{context} repo filter does not match its repository")
    if "source_type" in filters and filters["source_type"] not in {
        "issue",
        "issue_comment",
    }:
        raise FixtureBuildError(f"{context} source_type filter is invalid")
    if "issue_number" in filters and (
        not isinstance(filters["issue_number"], int)
        or isinstance(filters["issue_number"], bool)
        or filters["issue_number"] <= 0
    ):
        raise FixtureBuildError(f"{context} issue_number filter is invalid")
    relevant = row["relevant"]
    if not isinstance(relevant, list):
        raise FixtureBuildError(f"{context} relevant must be a list")
    if (query_class == "negative") != (len(relevant) == 0):
        raise FixtureBuildError(f"{context} negative/relevant contract is invalid")
    for judgment in relevant:
        if not isinstance(judgment, dict):
            raise FixtureBuildError(f"{context} relevant judgment must be an object")
        _require_exact_keys(judgment, RELEVANT_KEYS, f"{context} relevant judgment")
        if not isinstance(judgment["grade"], int) or not 1 <= judgment["grade"] <= 3:
            raise FixtureBuildError(f"{context} relevance grade is invalid")
        relevant_rationale = _require_string(
            judgment["rationale"], f"{context} relevant rationale"
        )
        _assert_safe_text(relevant_rationale, f"{context} relevant rationale")
        source = _require_string(judgment["source_id"], f"{context} source_id")
        if not SOURCE_ID_PATTERN.fullmatch(source):
            raise FixtureBuildError(f"{context} source_id is invalid")


def _parse_spec(path: Path) -> dict:
    spec = _read_json(path, "fresh blind specification")
    if not isinstance(spec, dict):
        raise FixtureBuildError("fresh blind specification must be an object")
    _require_exact_keys(
        spec,
        {
            "schema_version",
            "dev_repo",
            "distractor_limit",
            "pooled_query_ids",
            "repositories",
            "qrels",
        },
        "fresh blind specification",
    )
    if spec["schema_version"] != SPEC_SCHEMA:
        raise FixtureBuildError("fresh blind specification schema is unsupported")
    dev_repo = _validate_repo(spec["dev_repo"], "dev_repo")
    limit = spec["distractor_limit"]
    if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 100:
        raise FixtureBuildError(
            "distractor_limit must be an integer from 1 through 100"
        )
    repositories = spec["repositories"]
    if not isinstance(repositories, list) or not repositories:
        raise FixtureBuildError("repositories must be a non-empty list")
    repo_metadata = {}
    for entry in repositories:
        if not isinstance(entry, dict):
            raise FixtureBuildError("repository specification must be an object")
        _require_exact_keys(
            entry, {"repo", "license", "license_url"}, "repository specification"
        )
        repo = _validate_repo(entry["repo"], "repository repo")
        if repo in repo_metadata:
            raise FixtureBuildError(
                "repository specification contains a duplicate repo"
            )
        license_id = _require_string(entry["license"], "repository license")
        _assert_safe_text(license_id, "repository license")
        license_url = _validate_public_https_url(
            entry["license_url"], "repository license_url"
        )
        repo_metadata[repo] = {
            "repo": repo,
            "license": license_id,
            "license_url": license_url,
        }
    if dev_repo not in repo_metadata:
        raise FixtureBuildError("dev_repo must be declared in repositories")
    qrels = spec["qrels"]
    if not isinstance(qrels, list) or len(qrels) != 80:
        raise FixtureBuildError(
            "fresh blind specification must contain exactly 80 qrels"
        )
    seen_ids = set()
    class_counts = Counter()
    for index, entry in enumerate(qrels, 1):
        context = f"test qrel specification {index}"
        if not isinstance(entry, dict):
            raise FixtureBuildError(f"{context} must be an object")
        _require_exact_keys(
            entry, {"query_id", "query", "class", "repo", "rationale", "gold"}, context
        )
        query_id = _require_string(entry["query_id"], f"{context} query_id")
        if not QUERY_ID_PATTERN.fullmatch(query_id) or query_id in seen_ids:
            raise FixtureBuildError(f"{context} query_id is invalid or duplicated")
        seen_ids.add(query_id)
        query = _require_string(entry["query"], f"{context} query")
        _assert_safe_text(query, f"{context} query")
        query_class = _require_string(entry["class"], f"{context} class")
        if query_class not in CLASS_COUNTS:
            raise FixtureBuildError(f"{context} class is unsupported")
        class_counts[query_class] += 1
        repo = _validate_repo(entry["repo"], f"{context} repo")
        if repo not in repo_metadata:
            raise FixtureBuildError(f"{context} repo is not declared")
        rationale = _require_string(entry["rationale"], f"{context} rationale")
        _assert_safe_text(rationale, f"{context} rationale")
        gold = entry["gold"]
        if not isinstance(gold, list):
            raise FixtureBuildError(f"{context} gold must be a list")
        if (query_class == "negative") != (len(gold) == 0):
            raise FixtureBuildError(f"{context} negative/gold contract is invalid")
        locator_identities = set()
        for locator in gold:
            _validate_gold_locator(locator, repo, query_class, context)
            identity = (
                locator["repo"],
                locator["issue_number"],
                locator.get("comment_id"),
            )
            if identity in locator_identities:
                raise FixtureBuildError(f"{context} contains a duplicate gold locator")
            locator_identities.add(identity)
    if dict(class_counts) != CLASS_COUNTS:
        raise FixtureBuildError("fresh blind qrel class balance is invalid")
    expected_ids = [f"test-{index:03d}" for index in range(1, 81)]
    if [entry["query_id"] for entry in qrels] != expected_ids:
        raise FixtureBuildError(
            "fresh blind query IDs must be ordered test-001 through test-080"
        )
    pooled_query_ids = spec["pooled_query_ids"]
    if (
        not isinstance(pooled_query_ids, list)
        or len(pooled_query_ids) < 10
        or any(not isinstance(query_id, str) for query_id in pooled_query_ids)
        or len(set(pooled_query_ids)) != len(pooled_query_ids)
    ):
        raise FixtureBuildError(
            "pooled_query_ids must contain at least 10 distinct query IDs"
        )
    qrel_by_id = {entry["query_id"]: entry for entry in qrels}
    if any(
        query_id not in qrel_by_id
        or qrel_by_id[query_id]["class"] in {"exact_identifier", "negative"}
        for query_id in pooled_query_ids
    ):
        raise FixtureBuildError(
            "pooled_query_ids must name semantic test queries"
        )
    return {
        "dev_repo": dev_repo,
        "distractor_limit": limit,
        "pooled_query_ids": pooled_query_ids,
        "repo_metadata": repo_metadata,
        "qrels": qrels,
    }


def _validate_gold_locator(
    locator: object, repo: str, query_class: str, context: str
) -> None:
    if not isinstance(locator, dict):
        raise FixtureBuildError(f"{context} gold locator must be an object")
    required = {"repo", "issue_number", "grade"}
    optional = {"comment_id", "rationale"}
    if not required.issubset(locator) or not set(locator).issubset(required | optional):
        raise FixtureBuildError(f"{context} gold locator has an invalid field set")
    if _validate_repo(locator["repo"], f"{context} gold repo") != repo:
        raise FixtureBuildError(f"{context} gold repo does not match its filter repo")
    number = locator["issue_number"]
    if not isinstance(number, int) or isinstance(number, bool) or number <= 0:
        raise FixtureBuildError(f"{context} gold issue_number is invalid")
    grade = locator["grade"]
    if not isinstance(grade, int) or isinstance(grade, bool) or not 1 <= grade <= 3:
        raise FixtureBuildError(f"{context} gold grade is invalid")
    comment_id = locator.get("comment_id")
    if comment_id is not None and (
        not isinstance(comment_id, int)
        or isinstance(comment_id, bool)
        or comment_id <= 0
    ):
        raise FixtureBuildError(f"{context} gold comment_id is invalid")
    if query_class == "comment_only" and comment_id is None:
        raise FixtureBuildError(f"{context} comment_only gold must locate a comment")
    if query_class == "exact_identifier" and comment_id is not None:
        raise FixtureBuildError(f"{context} exact_identifier gold must locate an issue")
    if "rationale" in locator:
        rationale = _require_string(locator["rationale"], f"{context} gold rationale")
        _assert_safe_text(rationale, f"{context} gold rationale")


class CorpusCollector:
    def __init__(self, snapshot_at: str, repo_metadata: dict[str, dict]):
        self.snapshot_at = snapshot_at
        self.repo_metadata = repo_metadata
        self.records_by_source: dict[str, dict] = {}
        self.source_by_canonical: dict[str, str] = {}
        self.issues: dict[tuple[str, int], dict] = {}
        self.comments: dict[tuple[str, int], dict] = {}
        self.exclusions = Counter()

    def add_issue(
        self, repo: str, payload: object, required: bool = False
    ) -> dict | None:
        try:
            record = self._issue_record(repo, payload)
        except UnsafeSource as error:
            self.exclusions[error.reason] += 1
            if required:
                number = payload.get("number") if isinstance(payload, dict) else None
                raise FixtureBuildError(
                    f"required public gold/dev issue was rejected: {repo}#{number} ({error.reason})"
                ) from None
            return None
        return self._insert(record, (repo, record["issue_number"]), None)

    def add_comment(
        self, repo: str, payload: object, required: bool = False
    ) -> dict | None:
        try:
            record, comment_id = self._comment_record(repo, payload)
        except UnsafeSource as error:
            self.exclusions[error.reason] += 1
            if required:
                comment_id = payload.get("id") if isinstance(payload, dict) else None
                raise FixtureBuildError(
                    f"required public gold/dev comment was rejected: {repo} comment {comment_id} ({error.reason})"
                ) from None
            return None
        return self._insert(record, None, (repo, comment_id))

    def _insert(
        self,
        record: dict,
        issue_key: tuple[str, int] | None,
        comment_key: tuple[str, int] | None,
    ) -> dict:
        existing = self.records_by_source.get(record["source_id"])
        if existing is not None and existing != record:
            raise FixtureBuildError(
                "GitHub node identity mapped to conflicting source records"
            )
        canonical_source = self.source_by_canonical.get(record["canonical_url"])
        if canonical_source is not None and canonical_source != record["source_id"]:
            raise FixtureBuildError(
                "GitHub canonical URL mapped to conflicting source records"
            )
        self.records_by_source[record["source_id"]] = record
        self.source_by_canonical[record["canonical_url"]] = record["source_id"]
        if issue_key is not None:
            prior = self.issues.get(issue_key)
            if prior is not None and prior["source_id"] != record["source_id"]:
                raise FixtureBuildError(
                    "GitHub issue locator mapped to conflicting node identities"
                )
            self.issues[issue_key] = record
        if comment_key is not None:
            prior = self.comments.get(comment_key)
            if prior is not None and prior["source_id"] != record["source_id"]:
                raise FixtureBuildError(
                    "GitHub comment locator mapped to conflicting node identities"
                )
            self.comments[comment_key] = record
        return record

    def _issue_record(self, repo: str, payload: object) -> dict:
        if not isinstance(payload, dict):
            raise UnsafeSource("malformed")
        if "pull_request" in payload:
            raise UnsafeSource("pull_request")
        number = payload.get("number")
        if not isinstance(number, int) or isinstance(number, bool) or number <= 0:
            raise UnsafeSource("malformed")
        canonical_url = payload.get("html_url")
        if canonical_url != _expected_issue_html_url(repo, number):
            raise UnsafeSource("non_public_canonical_url")
        node_id = payload.get("node_id")
        if (
            not isinstance(node_id, str)
            or not node_id
            or any(char.isspace() for char in node_id)
        ):
            raise UnsafeSource("malformed")
        title = _assert_source_text(payload.get("title"), "title")
        body = _assert_source_text(payload.get("body"), "body")
        updated_at = payload.get("updated_at")
        if not isinstance(updated_at, str) or not UTC_TIMESTAMP_PATTERN.fullmatch(
            updated_at
        ):
            raise UnsafeSource("malformed")
        return {
            "schema_version": CORPUS_SCHEMA,
            "source_id": source_id("issue", node_id),
            "entity_type": "issue",
            "repo": repo,
            "issue_number": number,
            "canonical_url": canonical_url,
            "title": title,
            "body": body,
            "github_updated_at": updated_at,
            "body_sha256": sha256_bytes(body.encode("utf-8")),
            "snapshot_at": self.snapshot_at,
            "license": self.repo_metadata[repo]["license"],
        }

    def _comment_record(self, repo: str, payload: object) -> tuple[dict, int]:
        if not isinstance(payload, dict):
            raise UnsafeSource("malformed")
        comment_id = payload.get("id")
        if (
            not isinstance(comment_id, int)
            or isinstance(comment_id, bool)
            or comment_id <= 0
        ):
            raise UnsafeSource("malformed")
        issue_url = payload.get("issue_url")
        prefix = f"https://api.github.com/repos/{repo}/issues/"
        if not isinstance(issue_url, str) or not issue_url.startswith(prefix):
            raise UnsafeSource("non_public_canonical_url")
        suffix = issue_url[len(prefix) :]
        if not suffix.isdigit() or int(suffix) <= 0:
            raise UnsafeSource("non_public_canonical_url")
        number = int(suffix)
        parent = self.issues.get((repo, number))
        if parent is None:
            raise UnsafeSource("missing_parent")
        canonical_url = payload.get("html_url")
        if canonical_url != _expected_comment_html_url(repo, number, comment_id):
            raise UnsafeSource("non_public_canonical_url")
        node_id = payload.get("node_id")
        if (
            not isinstance(node_id, str)
            or not node_id
            or any(char.isspace() for char in node_id)
        ):
            raise UnsafeSource("malformed")
        body = _assert_source_text(payload.get("body"), "body")
        updated_at = payload.get("updated_at")
        if not isinstance(updated_at, str) or not UTC_TIMESTAMP_PATTERN.fullmatch(
            updated_at
        ):
            raise UnsafeSource("malformed")
        return (
            {
                "schema_version": CORPUS_SCHEMA,
                "source_id": source_id("issue-comment", node_id),
                "entity_type": "issue_comment",
                "repo": repo,
                "issue_number": number,
                "canonical_url": canonical_url,
                "title": f"Comment on issue #{number}: {parent['title']}",
                "body": body,
                "github_updated_at": updated_at,
                "body_sha256": sha256_bytes(body.encode("utf-8")),
                "snapshot_at": self.snapshot_at,
                "license": self.repo_metadata[repo]["license"],
            },
            comment_id,
        )

    def sorted_records(self) -> list[dict]:
        return sorted(
            self.records_by_source.values(),
            key=lambda record: (
                record["repo"],
                record["issue_number"],
                record["entity_type"],
                record["canonical_url"],
            ),
        )


def _fetch_list(fetch_json: Callable[[str], object], url: str) -> list[dict]:
    payload = fetch_json(url)
    if not isinstance(payload, list):
        raise FixtureBuildError("public GitHub REST collection response was not a list")
    return payload


def _fetch_object(fetch_json: Callable[[str], object], url: str) -> dict:
    payload = fetch_json(url)
    if not isinstance(payload, dict):
        raise FixtureBuildError("public GitHub REST object response was not an object")
    return payload


def _fetch_dev_repo(
    repo: str, collector: CorpusCollector, fetch_json: Callable[[str], object]
) -> None:
    page = 1
    while True:
        payloads = _fetch_list(
            fetch_json,
            _api_url(
                repo,
                "issues",
                f"state=all&sort=updated&direction=desc&per_page=100&page={page}",
            ),
        )
        for payload in payloads:
            collector.add_issue(repo, payload)
        if len(payloads) < 100:
            break
        page += 1
    page = 1
    while True:
        payloads = _fetch_list(
            fetch_json,
            _api_url(
                repo,
                "issues/comments",
                f"sort=updated&direction=desc&per_page=100&page={page}",
            ),
        )
        for payload in payloads:
            collector.add_comment(repo, payload)
        if len(payloads) < 100:
            break
        page += 1


def _fetch_external_distractors(
    repo: str,
    limit: int,
    collector: CorpusCollector,
    fetch_json: Callable[[str], object],
) -> None:
    payloads = _fetch_list(
        fetch_json,
        _api_url(
            repo,
            "issues",
            f"state=all&sort=updated&direction=desc&per_page={limit}&page=1",
        ),
    )
    for payload in payloads[:limit]:
        collector.add_issue(repo, payload)


def _collect_gold(
    qrel_specs: list[dict],
    collector: CorpusCollector,
    fetch_json: Callable[[str], object],
) -> None:
    issue_locators = {
        (gold["repo"], gold["issue_number"])
        for qrel in qrel_specs
        for gold in qrel["gold"]
    }
    for repo, number in sorted(issue_locators):
        if (repo, number) not in collector.issues:
            payload = _fetch_object(fetch_json, _api_url(repo, f"issues/{number}"))
            record = collector.add_issue(repo, payload, required=True)
            if record is None or record["issue_number"] != number:
                raise FixtureBuildError("gold issue response did not match its locator")
    comment_locators = {
        (gold["repo"], gold["issue_number"], gold["comment_id"])
        for qrel in qrel_specs
        for gold in qrel["gold"]
        if gold.get("comment_id") is not None
    }
    for repo, number, comment_id in sorted(comment_locators):
        payload = _fetch_object(
            fetch_json, _api_url(repo, f"issues/comments/{comment_id}")
        )
        record = collector.add_comment(repo, payload, required=True)
        if record is None or record["issue_number"] != number:
            raise FixtureBuildError(
                "gold comment response did not match its parent locator"
            )


def _build_test_qrels(qrel_specs: list[dict], collector: CorpusCollector) -> list[dict]:
    rows = []
    for entry in qrel_specs:
        relevant = []
        for gold in entry["gold"]:
            if gold.get("comment_id") is None:
                source = collector.issues.get((gold["repo"], gold["issue_number"]))
            else:
                source = collector.comments.get((gold["repo"], gold["comment_id"]))
            if source is None:
                raise FixtureBuildError(
                    "gold locator did not resolve to a corpus source"
                )
            relevant.append(
                {
                    "source_id": source["source_id"],
                    "grade": gold["grade"],
                    "rationale": gold.get("rationale", entry["rationale"]),
                }
            )
        filters = {"repo": entry["repo"]}
        if entry["class"] == "comment_only":
            filters["source_type"] = "issue_comment"
        if entry["class"] == "exact_identifier":
            issue_numbers = {gold["issue_number"] for gold in entry["gold"]}
            if len(issue_numbers) != 1:
                raise FixtureBuildError(
                    "exact_identifier qrel must target one issue number"
                )
            filters["issue_number"] = next(iter(issue_numbers))
        row = {
            "schema_version": QREL_SCHEMA,
            "query_id": entry["query_id"],
            "split": "test",
            "query": entry["query"],
            "class": entry["class"],
            "relevant": relevant,
            "filters": filters,
            "rationale": entry["rationale"],
            "labeler": PRIMARY_LABELER,
            "adjudicators": [PRIMARY_LABELER, SECONDARY_LABELER],
            "ambiguous": False,
            "second_adjudication": None,
        }
        _validate_qrel(row, "test", entry["repo"], "generated test qrel")
        rows.append(row)
    return rows


def _validate_split_and_sources(
    dev_qrels: list[dict], test_qrels: list[dict], collector: CorpusCollector
) -> None:
    missing_dev = {
        judgment["source_id"]
        for qrel in dev_qrels
        for judgment in qrel["relevant"]
        if judgment["source_id"] not in collector.records_by_source
    }
    if missing_dev:
        raise FixtureBuildError(
            "the public dev acquisition did not preserve every dev source"
        )

    def threads(qrels: list[dict]) -> set[tuple[str, int]]:
        result = set()
        for qrel in qrels:
            for judgment in qrel["relevant"]:
                source = collector.records_by_source.get(judgment["source_id"])
                if source is None:
                    raise FixtureBuildError(
                        "a qrel references a source absent from the corpus"
                    )
                result.add((source["repo"], source["issue_number"]))
        return result

    if not threads(dev_qrels).isdisjoint(threads(test_qrels)):
        raise FixtureBuildError("an issue thread leaks across the dev and test splits")
    issue_threads = {
        (record["repo"], record["issue_number"])
        for record in collector.records_by_source.values()
        if record["entity_type"] == "issue"
    }
    for record in collector.records_by_source.values():
        if (
            record["entity_type"] == "issue_comment"
            and (record["repo"], record["issue_number"]) not in issue_threads
        ):
            raise FixtureBuildError("corpus comment is missing its parent issue")


def _jsonl_bytes(records: list[dict]) -> bytes:
    return "".join(
        json.dumps(record, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
        + "\n"
        for record in records
    ).encode("utf-8")


def _assert_machine_output_path(path: Path) -> None:
    parts = path.resolve().parts
    if not any(
        parts[index] == "target" and parts[index + 1] == "qgh-eval"
        for index in range(len(parts) - 1)
    ):
        raise FixtureBuildError("output_dir must be under target/qgh-eval")


def build_fixture(
    *,
    spec_path: Path,
    dev_qrels_path: Path,
    output_dir: Path,
    snapshot_at: str,
    fetch_json: Callable[[str], object] | None = None,
) -> None:
    """Build and validate corpus/qrels/provenance without logging source content."""

    if not UTC_TIMESTAMP_PATTERN.fullmatch(snapshot_at):
        raise FixtureBuildError("snapshot_at must be a whole-second UTC timestamp")
    _assert_machine_output_path(output_dir)
    spec = _parse_spec(spec_path)
    dev_raw, dev_qrels = _parse_dev_qrels(dev_qrels_path, spec["dev_repo"])
    fetch = fetch_json or GitHubRestClient().get_json
    collector = CorpusCollector(snapshot_at, spec["repo_metadata"])

    _fetch_dev_repo(spec["dev_repo"], collector, fetch)
    for repo in sorted(spec["repo_metadata"]):
        if repo != spec["dev_repo"]:
            _fetch_external_distractors(
                repo, spec["distractor_limit"], collector, fetch
            )
    _collect_gold(spec["qrels"], collector, fetch)
    test_qrels = _build_test_qrels(spec["qrels"], collector)
    _validate_split_and_sources(dev_qrels, test_qrels, collector)

    records = collector.sorted_records()
    corpus_raw = _jsonl_bytes(records)
    test_raw = _jsonl_bytes(test_qrels)
    repository_counts = Counter(record["repo"] for record in records)
    if set(repository_counts) != set(spec["repo_metadata"]):
        raise FixtureBuildError(
            "every declared public repository must yield a safe source"
        )
    provenance = {
        "schema_version": PROVENANCE_SCHEMA,
        "snapshot_at": snapshot_at,
        "acquisition": {
            "method": "unauthenticated GitHub REST API",
            "authentication": "none",
            "raw_response_committed": False,
        },
        "repositories": [
            {
                "repo": repo,
                "visibility": "public",
                "license": metadata["license"],
                "repo_url": f"https://github.com/{repo}",
                "issues_api": _api_url(
                    repo,
                    "issues",
                    "state=all&sort=updated&direction=desc&per_page="
                    + str(100 if repo == spec["dev_repo"] else spec["distractor_limit"])
                    + "&page=1",
                ),
                "source_count": repository_counts.get(repo, 0),
            }
            for repo, metadata in sorted(spec["repo_metadata"].items())
        ],
        "exclusions": [
            "pull requests returned by the Issues endpoint",
            "records with empty bodies or titles",
            "records matching secret-like payload patterns",
            "records containing absolute macOS user-home paths",
            "records with non-public or mismatched canonical URLs",
            "ambiguous query candidates without adjudication",
        ],
        "exclusion_counts": {
            "absolute_local_path": collector.exclusions.get("absolute_local_path", 0)
        },
        "adjudication": {
            "method": "manual fresh blind source-body review",
            "ambiguous_candidate_policy": "second adjudication or exclusion",
            "title_only_paraphrases_allowed": False,
        },
        "judgment_pool": {
            "method": "manual source-body overlap review across split-safe public issue threads",
            "complete": True,
            "multi_source_query_count": len(spec["pooled_query_ids"]),
            "count_definition": "queries reviewed against multiple candidate sources, including unique-gold outcomes",
        },
        "corpus_sha256": sha256_bytes(corpus_raw),
        "qrels_dev_sha256": sha256_bytes(dev_raw),
        "qrels_test_sha256": sha256_bytes(test_raw),
        "dev_query_count": len(dev_qrels),
        "test_query_count": len(test_qrels),
    }

    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "corpus.jsonl").write_bytes(corpus_raw)
    (output_dir / "qrels-dev.jsonl").write_bytes(dev_raw)
    (output_dir / "qrels-test.jsonl").write_bytes(test_raw)
    (output_dir / "provenance.json").write_text(
        json.dumps(provenance, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Build a fresh blind public GitHub live-model fixture."
    )
    parser.add_argument("--spec", type=Path, required=True)
    parser.add_argument("--dev-qrels", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--snapshot-at", required=True)
    args = parser.parse_args()
    try:
        build_fixture(
            spec_path=args.spec,
            dev_qrels_path=args.dev_qrels,
            output_dir=args.output_dir,
            snapshot_at=args.snapshot_at,
        )
    except FixtureBuildError as error:
        parser.exit(2, f"error: {error}\n")


if __name__ == "__main__":
    main()
