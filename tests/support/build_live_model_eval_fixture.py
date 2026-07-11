#!/usr/bin/env python3
"""Build the committed Lane C corpus/qrels from public GitHub REST snapshots."""

from __future__ import annotations

import argparse
import glob
import hashlib
import json
import re
from pathlib import Path
from urllib.parse import quote


SCHEMA_CORPUS = "qgh.live_model_corpus.v1"
SCHEMA_QREL = "qgh.live_model_qrel.v1"
SCHEMA_PROVENANCE = "qgh.live_model_provenance.v1"
LABELER = "qgh Lane C public-corpus labeler"
SECOND_LABELER = "qgh Lane C alternate-source adjudicator"
SECRET_PATTERNS = [
    re.compile(r"ghp_[A-Za-z0-9]{20,}"),
    re.compile(r"github_pat_[A-Za-z0-9_]{20,}"),
    re.compile(r"AKIA[0-9A-Z]{16}"),
    re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
    re.compile(r"\bsk-[A-Za-z0-9]{20,}\b"),
]
ABSOLUTE_LOCAL_PATH = re.compile(r"/Users/")


def sha256_text(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_json(path: Path):
    return json.loads(path.read_text(encoding="utf-8"))


def source_id(kind: str, node_id: str) -> str:
    return f"qgh://github.com/{kind}/{quote(node_id, safe='')}"


def assert_public_text_safe(text: str, locator: str) -> None:
    for pattern in SECRET_PATTERNS:
        if pattern.search(text):
            raise ValueError(f"secret-like payload excluded: {locator} ({pattern.pattern})")


def contains_absolute_local_path(*values: str) -> bool:
    return any(ABSOLUTE_LOCAL_PATH.search(value or "") for value in values)


def issue_record(repo: str, issue: dict, snapshot_at: str, license_id: str) -> dict:
    body = issue.get("body") or ""
    assert_public_text_safe(body, issue["html_url"])
    return {
        "schema_version": SCHEMA_CORPUS,
        "source_id": source_id("issue", issue["node_id"]),
        "entity_type": "issue",
        "repo": repo,
        "issue_number": issue["number"],
        "canonical_url": issue["html_url"],
        "title": issue["title"],
        "body": body,
        "github_updated_at": issue["updated_at"],
        "body_sha256": sha256_text(body),
        "snapshot_at": snapshot_at,
        "license": license_id,
    }


def comment_record(
    repo: str,
    comment: dict,
    parent: dict,
    snapshot_at: str,
    license_id: str,
) -> dict:
    body = comment.get("body") or ""
    assert_public_text_safe(body, comment["html_url"])
    return {
        "schema_version": SCHEMA_CORPUS,
        "source_id": source_id("issue-comment", comment["node_id"]),
        "entity_type": "issue_comment",
        "repo": repo,
        "issue_number": parent["number"],
        "canonical_url": comment["html_url"],
        "title": f"Comment on issue #{parent['number']}: {parent['title']}",
        "body": body,
        "github_updated_at": comment["updated_at"],
        "body_sha256": sha256_text(body),
        "snapshot_at": snapshot_at,
        "license": license_id,
    }


def qrel(
    qid: str,
    split: str,
    query_class: str,
    query: str,
    repo: str,
    issue_number: int | None,
    rationale: str,
    issue_sources: dict,
    comment_sources: dict,
    comment_id: int | None = None,
    source_type: str | None = None,
) -> dict:
    relevant = []
    if query_class != "negative":
        if comment_id is not None:
            source = comment_sources[(repo, comment_id)]
        else:
            source = issue_sources[(repo, issue_number)]
        relevant = [{"source_id": source["source_id"], "grade": 3, "rationale": rationale}]
    filters = {"repo": repo}
    if source_type is not None:
        filters["source_type"] = source_type
    if query_class == "exact_identifier" and issue_number is not None:
        filters["issue_number"] = issue_number
    return {
        "schema_version": SCHEMA_QREL,
        "query_id": qid,
        "split": split,
        "query": query,
        "class": query_class,
        "relevant": relevant,
        "filters": filters,
        "rationale": rationale,
        "labeler": LABELER,
        "adjudicators": [LABELER, SECOND_LABELER],
        "ambiguous": False,
        "second_adjudication": None,
    }


def build_qrels(issue_sources: dict, comment_sources: dict) -> tuple[list[dict], list[dict]]:
    dev_specs = [
        ("english_semantic", "How are model-tokenizer chunks persisted for later embedding?", "juicyjusung/qgh", 48),
        ("english_semantic", "Skip embedding work when a source body hash did not change", "juicyjusung/qgh", 51),
        ("english_semantic", "Expose local embedding coverage without loading a model", "juicyjusung/qgh", 52),
        ("english_semantic", "Store dense vectors in an additive sqlite-vec schema", "juicyjusung/qgh", 53),
        ("english_semantic", "Generate vector-only candidates before hybrid fusion is enabled", "juicyjusung/qgh", 54),
        ("korean_semantic", "현재 저장소를 기본 검색 범위로 제한하는 정책", "juicyjusung/qgh", 20),
        ("korean_semantic", "프로필 플래그 없이 저장소가 일치하는 단일 프로필 선택", "juicyjusung/qgh", 21),
        ("korean_semantic", "실제로 적용된 검색 범위와 진단 메타데이터 노출", "juicyjusung/qgh", 22),
        ("korean_semantic", "MCP 읽기 도구도 저장소 정책과 프로필 해석을 공유", "juicyjusung/qgh", 23),
        ("korean_semantic", "개인 프로필 설정과 저장소 정책의 역할을 분리", "juicyjusung/qgh", 24),
        ("ko_query_en_source", "초기 설정에서 프로필과 저장소 정책을 함께 만드는 명령", "juicyjusung/qgh", 29),
        ("ko_query_en_source", "자격 증명 저장소 토큰을 향후 범위로 미루는 결정", "juicyjusung/qgh", 30),
        ("ko_query_en_source", "JSON 옵션이 없을 때 사람이 읽는 명령 출력", "juicyjusung/qgh", 31),
        ("ko_query_en_source", "한 번에 여러 원문을 순서대로 조회", "juicyjusung/qgh", 32),
        ("ko_query_en_source", "CLI 계약을 중심으로 MCP를 얇은 어댑터로 유지", "juicyjusung/qgh", 33),
        ("en_query_ko_source", "Show synchronization progress without corrupting JSON stdout", "juicyjusung/qgh", 34),
        ("en_query_ko_source", "Define freshness rules for active issues and synchronization", "juicyjusung/qgh", 35),
        ("en_query_ko_source", "Make lifecycle verification an explicit opt-in for get", "juicyjusung/qgh", 36),
        ("en_query_ko_source", "Warn or fail queries based on local snapshot age", "juicyjusung/qgh", 37),
        ("en_query_ko_source", "Refresh one named issue and reconcile its comments", "juicyjusung/qgh", 38),
        ("exact_identifier", "#39", "juicyjusung/qgh", 39),
        ("exact_identifier", "https://github.com/juicyjusung/qgh/issues/40", "juicyjusung/qgh", 40),
        ("exact_identifier", "qgh issue 41", "juicyjusung/qgh", 41),
        ("exact_identifier", "issue #42 smart bootstrap", "juicyjusung/qgh", 42),
        ("exact_identifier", "#43 per-issue backfill", "juicyjusung/qgh", 43),
        ("long_context", "Keep comment history complete while doing bounded historical backfill", "juicyjusung/qgh", 14),
        ("long_context", "Prevent SQLite and Tantivy publication races during concurrent access", "juicyjusung/qgh", 15),
        ("long_context", "Calibrate a curated retrieval benchmark without changing gates casually", "juicyjusung/qgh", 16),
        ("long_context", "Ensure release documentation and generated schemas describe one contract", "juicyjusung/qgh", 17),
        ("long_context", "Bootstrap a profile and repository scope through the first-run wizard", "juicyjusung/qgh", 26),
    ]
    dev = []
    for index, (kind, query, repo, number) in enumerate(dev_specs, 1):
        dev.append(qrel(f"dev-{index:03d}", "dev", kind, query, repo, number, f"The owning public issue #{number} directly specifies this behavior.", issue_sources, comment_sources))
    for index, (number, comment_id, commit) in enumerate([
        (10, 4826824986, "75c02ec"),
        (11, 4826854110, "3034f75"),
        (12, 4826875942, "e7b078b"),
        (13, 4826894165, "e5ed932"),
    ], len(dev) + 1):
        dev.append(qrel(f"dev-{index:03d}", "dev", "comment_only", f"Which implementation update records commit {commit}?", "juicyjusung/qgh", number, "The commit identifier appears in the public issue comment, not the issue body.", issue_sources, comment_sources, comment_id=comment_id))
    for query in [
        "Oracle redo log transport for a multi-region database",
        "CSS container queries for a responsive photo carousel",
        "Kubernetes GPU device plugin scheduling regression",
        "iOS CoreBluetooth background reconnect policy",
        "PostgreSQL logical replication slot failover",
        "WebRTC acoustic echo cancellation tuning",
    ]:
        index = len(dev) + 1
        dev.append(qrel(f"dev-{index:03d}", "dev", "negative", query, "juicyjusung/qgh", None, "No source in the bounded qgh issue corpus is relevant.", issue_sources, comment_sources))

    # These are manually adjudicated from acceptance criteria and normative
    # sections in the public qgh issue bodies.  They intentionally avoid
    # mechanically turning issue titles into queries.
    english_semantic = [
        (2, "Which tracker contract keeps retrieval local, read-only, and centered on query, get, then cite?", "The gateway body locks the local CLI-first Issues/comments scope and says snippets are candidates rather than citation evidence."),
        (3, "Where are profile data paths derived when arbitrary database path overrides are forbidden?", "The body fixes config under XDG_CONFIG_HOME and derives SQLite/Tantivy storage under the profile-specific XDG_DATA_HOME path."),
        (4, "How should pull request payloads returned by the Issues endpoint be handled before BM25 indexing?", "The issue acceptance criteria require detecting the pull_request key, counting the payload as skipped, and never inserting it as a source entity."),
        (5, "What parent metadata must accompany an issue comment search result and authoritative lookup?", "The comment tracer requires parent source identity, repository, issue number, title, URL, and parent context on query/get results."),
        (6, "Which lifecycle questions remain before a GitHub Wiki connector can support renamed or deleted pages?", "The post-MVP connector issue explicitly lists page identity, rename/delete reconciliation, and disabled or authentication-error states."),
        (7, "How does incremental synchronization distinguish an edited source version while remaining idempotent?", "The body requires body hash, GitHub updated_at, observed run, lifecycle metadata, an overlapping watermark, and idempotent upsert."),
        (8, "Why can free-text ranking never widen repository, author, state, label, or issue-number constraints?", "The hard-filter acceptance criteria parse those values as strict structured filters and prohibit ranking from broadening them."),
        (9, "Why must every search candidate be resolved through get before it can support a citation?", "The citation contract calls snippets previews and requires every top-k source identity or get_args value to round-trip to authoritative content."),
        (44, "How can repository-wide comment sync reject pull-request parents without unbounded parent lookups?", "The issue specifies issue_url parent resolution under a fixed budget, PR-parent skipping, and a deferred coverage gap when the budget is exhausted."),
        (45, "What explicit reconciliation pass repairs recent deletes, transfers, and lost permissions without a hidden background scan?", "The body defines a recent window, distinct transfer/delete/permission-loss reasons, alias cycle protection, and explicit-only execution."),
        (46, "When would a bounded bootstrap need a separately resumable sweep over open issues?", "The follow-up asks for repository-scale evidence, an open_cursor budget, and coherent coverage states between complete and phased modes."),
        (47, "How can semantic retrieval be optional while the lexical query-to-citation path remains independently complete?", "The hybrid PRD keeps BM25 required, enables local vectors only by configuration, fuses candidates with RRF, and retains source-level get citation."),
        (55, "At which stage must hard filters be applied before reciprocal-rank fusion and source deduplication?", "The H3a criteria require pre-filtering in both candidate generators, equal RRF, and one representative hit per source."),
        (56, "Which typed ranking fields may be exposed without calling the score confidence or adding writable MCP tools?", "The H3b criteria require strict hybrid ranking schemas, forbid confidence terminology, and keep MCP limited to read-only query/get/status."),
        (57, "What should happen when local vector coverage is incomplete or the embedding runtime fails?", "The H3c issue requires structured coverage warnings and a BM25 result with the same schema and content rather than command failure."),
        (58, "How should multilingual semantic improvement be measured without weakening existing lexical gates?", "The H4a body requires a fixed A/B fixture, semantic and cross-language recall reporting, BM25 non-regression, filters, and full round trips."),
        (59, "Why does a three-model embedding benchmark not by itself authorize changing the default model?", "The H4b acceptance criteria require one protocol and a report, while reserving any default change for a later human PRD or ADR decision."),
        (60, "How is the embedding configuration kept local-only and strict while preserving a runtime-free BM25 path?", "The H1a criteria accept only the local provider, reject unknown keys and invalid enums, and keep embedding dependencies off the BM25-only path."),
        (62, "Which release chain connects automated artifacts, checksums, attestations, and the Homebrew tap?", "The release issue scope explicitly connects cargo-dist, GitHub Release checksums and attestations, and tap publication/smoke testing."),
        (76, "What first-run sequence should teach installation, GitHub authentication, synchronization, retrieval, citation, and diagnosis?", "The README criteria prescribe brew install, GitHub auth, init, sync, query, get, doctor, plus the source-candidate citation warning."),
    ]
    heldout = []
    for number, query, rationale in english_semantic:
        index = len(heldout) + 1
        heldout.append(qrel(f"test-{index:03d}", "test", "english_semantic", query, "juicyjusung/qgh", number, rationale, issue_sources, comment_sources))

    korean_semantic = [
        (76, "오픈소스 명령줄 도구의 README 온보딩을 완성하는 작업"),
        (77, "README에 npx 기반 스킬 설치 방법을 추가"),
        (78, "엔터프라이즈 호스트를 gh 인증 토큰 조회에 전달하지 않아 발생한 401"),
        (79, "초기화할 때 기본 프로필 이름을 work로 고정한 문제"),
        (80, "다른 프로필 허용 목록에 이미 있는 저장소를 초기화 시 감지"),
        (81, "저장소 정책으로 범위를 정할 때 git remote 호스트 정보가 사라지는 오류"),
        (82, "여러 프로필이 있을 때 플래그 없이 프로필을 고르는 사용자 경험"),
        (83, "임베딩 명령의 사전 검증 순서를 더 이해하기 쉽게 수정"),
        (84, "강제 옵션 검사보다 설정 검증이 먼저 실행되어야 하는 문제"),
        (85, "배포 바이너리에 로컬 임베딩 기능을 포함"),
        (86, "한국어 토크나이저 오프셋을 원문 바이트로 잘못 처리한 청킹 실패"),
        (87, "정규화된 텍스트 청킹에서도 마크다운 구조 경계를 보존"),
        (88, "대형 임베딩 배치가 수십 기가바이트 메모리를 사용하는 문제"),
        (89, "청커 버전을 저장하지 않아 변경 후 오래된 청크를 재생성할 수 없는 문제"),
        (47, "BM25와 로컬 벡터 임베딩을 결합하는 하이브리드 검색 설계"),
    ]
    for number, query in korean_semantic:
        index = len(heldout) + 1
        heldout.append(qrel(f"test-{index:03d}", "test", "korean_semantic", query, "juicyjusung/qgh", number, f"공개 qgh 이슈 #{number}가 이 동작 또는 결함을 직접 다룬다.", issue_sources, comment_sources))

    for number, query, rationale in [
        (2, "검색 결과를 답변이 아니라 출처 후보로 취급하고 조회 후 인용하는 계약", "The English gateway source states the query -> get -> cite contract and the local read-only scope."),
        (3, "프로필별 XDG 경로에서 설정과 검색 데이터 위치를 엄격하게 정하는 규칙", "The English source fixes profile config and data paths and disallows an arbitrary database path override."),
        (4, "GitHub Issues API에 섞여 오는 pull request 항목을 색인 전에 제외", "The mostly English acceptance block requires skipping payloads carrying the pull_request key before source insertion."),
        (5, "이슈 댓글 검색 결과에 부모 이슈 식별자와 제목 및 URL 문맥 포함", "The mostly English acceptance block lists the complete parent context required for comment query and get."),
        (6, "위키 페이지 이름 변경과 삭제 상태를 조정하는 향후 커넥터 설계", "The English post-MVP source lists Wiki identity, rename/delete lifecycle, and reconciliation as connector questions."),
        (7, "수정된 본문을 다음 동기화에서 새 source version으로 구분하면서 멱등성 유지", "The mostly English source defines source-version hashes, update timestamps, watermark overlap, and idempotent upsert."),
        (8, "저장소와 작성자 및 상태 필터를 자유 텍스트 랭킹이 넓히지 못하게 하는 규칙", "The mostly English source requires strict structured hard filters that ranking cannot broaden."),
        (9, "검색 스니펫 대신 get으로 확인한 원문과 canonical URL을 인용", "The mostly English source states that snippets are previews and authoritative get output supplies citation evidence."),
        (47, "어휘 검색은 독립적으로 유지하면서 로컬 벡터를 선택적으로 결합", "The English hybrid PRD body preserves BM25 and makes local vector/RRF retrieval opt-in."),
        (47, "호스팅 임베딩 없이 한국어와 영어 의역 검색을 개선하는 로컬 하이브리드 경로", "The English problem and solution sections identify cross-language lexical mismatch and local ONNX plus RRF as the bounded remedy."),
    ]:
        index = len(heldout) + 1
        heldout.append(qrel(f"test-{index:03d}", "test", "ko_query_en_source", query, "juicyjusung/qgh", number, rationale, issue_sources, comment_sources))

    for number, query in [
        (80, "Detect a repository already present in another profile allowlist during init"),
        (81, "Preserve the Git remote host when repo scope comes from .qgh.toml"),
        (82, "Improve profile resolution when several profiles exist and no flag is given"),
        (83, "Make embed preflight validation order easier to understand"),
        (84, "Validate embedding configuration before checking the force flag"),
        (85, "Ship the fastembed provider in release binaries"),
        (86, "Fix Korean chunking by mapping tokenizer offsets back to original UTF-8 bytes"),
        (87, "Restore Markdown structural boundaries after tokenizer normalization"),
        (88, "Bound embedding batches after observing roughly forty gigabytes of memory use"),
        (89, "Record the chunker version so stale chunks can be rebuilt"),
    ]:
        index = len(heldout) + 1
        heldout.append(qrel(f"test-{index:03d}", "test", "en_query_ko_source", query, "juicyjusung/qgh", number, f"The English query paraphrases the Korean public qgh issue #{number}.", issue_sources, comment_sources))

    for repo, number, query in [
        ("juicyjusung/qgh", 76, "#76"),
        ("juicyjusung/qgh", 77, "https://github.com/juicyjusung/qgh/issues/77"),
        ("juicyjusung/qgh", 78, "issue 78 enterprise host 401"),
        ("juicyjusung/qgh", 79, "qgh #79 profile id work"),
        ("juicyjusung/qgh", 80, "#80 duplicate allowlist"),
        ("juicyjusung/qgh", 81, "qgh issue 81 git remote host"),
        ("juicyjusung/qgh", 82, "https://github.com/juicyjusung/qgh/issues/82"),
        ("juicyjusung/qgh", 83, "issue #83 embed preflight"),
        ("juicyjusung/qgh", 84, "#84 force validation order"),
        ("juicyjusung/qgh", 85, "qgh #85 fastembed release"),
    ]:
        index = len(heldout) + 1
        heldout.append(qrel(f"test-{index:03d}", "test", "exact_identifier", query, repo, number, "The query contains the stable issue locator or an exact identifier from its title.", issue_sources, comment_sources))

    for number, comment_id, commit in [
        (4, 4826719197, "c9d4312"),
        (5, 4826741701, "4a63f57"),
        (8, 4826773340, "9ae8b41"),
        (9, 4826795126, "622bb4b"),
        (55, 4880906559, "ea080d4"),
    ]:
        index = len(heldout) + 1
        heldout.append(qrel(f"test-{index:03d}", "test", "comment_only", f"Which implementation update records commit {commit}?", "juicyjusung/qgh", number, "The commit identifier occurs in a public issue comment and not in the owning issue body.", issue_sources, comment_sources, comment_id=comment_id))

    for repo, number, query in [
        ("juicyjusung/qgh", 28, "What are all first-run wizard branches for preset preview, customization, and cancellation?"),
        ("juicyjusung/qgh", 47, "Which invariants govern hybrid retrieval, local embeddings, and citation round trips?"),
        ("juicyjusung/qgh", 62, "How are release artifacts, the Homebrew tap, and installation smoke tests connected?"),
        ("juicyjusung/qgh", 2, "How does the gateway resolve scope conflicts among the MVP contract, product brief, hybrid program, and unnamed artifacts?"),
        ("juicyjusung/qgh", 86, "Why do normalized tokenizer offsets break Korean UTF-8 chunk slicing, and what byte-boundary mapping must replace them?"),
    ]:
        index = len(heldout) + 1
        heldout.append(qrel(f"test-{index:03d}", "test", "long_context", query, repo, number, "The answer depends on details in a long public issue body rather than its locator alone.", issue_sources, comment_sources))

    for repo, query in [
        ("juicyjusung/qgh", "CUDA kernel occupancy for grouped query attention"),
        ("juicyjusung/qgh", "Terraform provider drift for an AWS transit gateway"),
        ("juicyjusung/qgh", "React Native camera frame processor color conversion"),
        ("juicyjusung/qgh", "PostgreSQL serializable transaction anomaly in a billing ledger"),
        ("juicyjusung/qgh", "Kubernetes ingress controller certificate renewal"),
    ]:
        index = len(heldout) + 1
        heldout.append(qrel(f"test-{index:03d}", "test", "negative", query, repo, None, "No source in the bounded repository corpus is relevant.", issue_sources, comment_sources))

    alternate_relevant = {
        "test-001": [("juicyjusung/qgh", 1, 2, "The product brief independently states local-first read-only query, get, and cite behavior.")],
        "test-004": [("juicyjusung/qgh", 9, 2, "The citation contract also requires parent context for comment source candidates and get results.")],
        "test-008": [
            ("juicyjusung/qgh", 1, 2, "The product brief states that search results are candidates and citation follows authoritative get."),
            ("juicyjusung/qgh", 2, 2, "The MVP gateway repeats the query, get, cite contract and snippet limitation."),
        ],
        "test-012": [("juicyjusung/qgh", 55, 2, "The H3 fusion slice specifies RRF while preserving the independent BM25-only mode.")],
        "test-013": [("juicyjusung/qgh", 47, 2, "The hybrid PRD specifies pre-filtering, equal RRF, and source-level deduplication.")],
        "test-014": [("juicyjusung/qgh", 47, 2, "The hybrid PRD defines typed hybrid ranking fields and a read-only MCP surface.")],
        "test-015": [("juicyjusung/qgh", 47, 2, "The hybrid PRD requires coverage gating and BM25 graceful degradation.")],
        "test-016": [
            ("juicyjusung/qgh", 47, 2, "The hybrid PRD defines semantic and cross-language evaluation goals."),
            ("juicyjusung/qgh", 59, 1, "The model A/B slice shares the fixed-fixture reporting requirement."),
        ],
        "test-017": [
            ("juicyjusung/qgh", 47, 2, "The hybrid PRD requires model evidence before any default change."),
            ("juicyjusung/qgh", 58, 2, "The evaluation slice defines the fixed A/B protocol used by the model comparison."),
        ],
        "test-018": [("juicyjusung/qgh", 47, 2, "The hybrid PRD constrains embeddings to local runtime while preserving BM25.")],
        "test-019": [("juicyjusung/qgh", 76, 1, "The onboarding issue connects Homebrew installation to release documentation and smoke verification.")],
        "test-020": [
            ("juicyjusung/qgh", 1, 2, "The product brief covers first-run positioning and the complete retrieval workflow."),
            ("juicyjusung/qgh", 2, 1, "The MVP gateway includes installation-facing scope and citation invariants."),
        ],
    }
    heldout_by_id = {record["query_id"]: record for record in heldout}
    for query_id, judgments in alternate_relevant.items():
        record = heldout_by_id[query_id]
        for repo, issue_number, grade, judgment_rationale in judgments:
            source = issue_sources[(repo, issue_number)]
            if all(item["source_id"] != source["source_id"] for item in record["relevant"]):
                record["relevant"].append({
                    "source_id": source["source_id"],
                    "grade": grade,
                    "rationale": judgment_rationale,
                })

    if len(dev) != 40 or len(heldout) != 80:
        raise AssertionError(f"unexpected qrel counts: dev={len(dev)} test={len(heldout)}")
    return dev, heldout


def write_jsonl(path: Path, records: list[dict]) -> None:
    encoded = "".join(json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n" for record in records)
    path.write_text(encoded, encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--acquisition-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--snapshot-at", required=True)
    args = parser.parse_args()

    qgh_issues = read_json(args.acquisition_dir / "qgh-issues.json")
    issues_by_url = {}
    records = []
    absolute_local_path_exclusions = 0
    licenses = {"juicyjusung/qgh": "NOASSERTION"}
    for repo, issues in [("juicyjusung/qgh", qgh_issues)]:
        for issue in issues:
            if "pull_request" in issue or issue["number"] in ({18, 19} if repo == "juicyjusung/qgh" else set()):
                continue
            if not (issue.get("body") or "").strip():
                continue
            if contains_absolute_local_path(issue.get("title") or "", issue.get("body") or ""):
                absolute_local_path_exclusions += 1
                continue
            record = issue_record(repo, issue, args.snapshot_at, licenses[repo])
            records.append(record)
            issues_by_url[issue["url"]] = (repo, issue)

    for path_string in glob.glob(str(args.acquisition_dir / "comments-*.json")):
        for comment in read_json(Path(path_string)):
            parent_entry = issues_by_url.get(comment["issue_url"])
            if parent_entry is None or not (comment.get("body") or "").strip():
                continue
            if contains_absolute_local_path(comment.get("body") or ""):
                absolute_local_path_exclusions += 1
                continue
            repo, parent = parent_entry
            records.append(comment_record(repo, comment, parent, args.snapshot_at, licenses[repo]))

    records.sort(key=lambda record: (record["repo"], record["issue_number"], record["entity_type"], record["canonical_url"]))
    issue_sources = {(record["repo"], record["issue_number"]): record for record in records if record["entity_type"] == "issue"}
    comment_sources = {}
    for record in records:
        if record["entity_type"] != "issue_comment":
            continue
        comment_sources[(record["repo"], int(record["canonical_url"].rsplit("-", 1)[1]))] = record

    dev, heldout = build_qrels(issue_sources, comment_sources)
    args.output_dir.mkdir(parents=True, exist_ok=True)
    corpus_path = args.output_dir / "corpus.jsonl"
    dev_path = args.output_dir / "qrels-dev.jsonl"
    test_path = args.output_dir / "qrels-test.jsonl"
    write_jsonl(corpus_path, records)
    write_jsonl(dev_path, dev)
    write_jsonl(test_path, heldout)

    repo_counts = {}
    for record in records:
        repo_counts[record["repo"]] = repo_counts.get(record["repo"], 0) + 1
    provenance = {
        "schema_version": SCHEMA_PROVENANCE,
        "snapshot_at": args.snapshot_at,
        "acquisition": {
            "method": "unauthenticated GitHub REST API",
            "authentication": "none",
            "raw_response_committed": False,
        },
        "repositories": [
            {
                "repo": "juicyjusung/qgh",
                "visibility": "public",
                "license": "NOASSERTION",
                "repo_url": "https://github.com/juicyjusung/qgh",
                "issues_api": "https://api.github.com/repos/juicyjusung/qgh/issues?state=all&per_page=100",
                "source_count": repo_counts.get("juicyjusung/qgh", 0),
            },
        ],
        "exclusions": [
            "pull requests returned by the Issues endpoint",
            "qgh operational loop state/run-log issues #18 and #19",
            "records with empty bodies",
            "records matching secret-like payload patterns",
            "ambiguous query candidates without a second adjudication",
            "records containing absolute macOS user-home paths",
        ],
        "exclusion_counts": {
            "absolute_local_path": absolute_local_path_exclusions,
        },
        "adjudication": {
            "method": "manual source-body review",
            "ambiguous_candidate_policy": "second adjudication or exclusion",
            "title_only_paraphrases_allowed": False,
        },
        "judgment_pool": {
            "method": "manual source-body overlap review across split-safe qgh issue threads",
            "complete": True,
            "multi_source_query_count": sum(
                1 for record in dev + heldout if len(record["relevant"]) > 1
            ),
            "count_definition": "queries reviewed against multiple candidate sources, including unique-gold outcomes",
        },
        "corpus_sha256": sha256_file(corpus_path),
        "qrels_dev_sha256": sha256_file(dev_path),
        "qrels_test_sha256": sha256_file(test_path),
        "dev_query_count": len(dev),
        "test_query_count": len(heldout),
    }
    (args.output_dir / "provenance.json").write_text(
        json.dumps(provenance, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


if __name__ == "__main__":
    main()
