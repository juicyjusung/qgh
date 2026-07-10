#!/usr/bin/env python3
"""Prepare explicit local ModelManifestV1 snapshots for Lane C live evaluation."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
from pathlib import Path


GTE_MODEL_ID = "Alibaba-NLP/gte-modernbert-base"
GTE_REVISION = "e7f32e3c00f91d699e8c43b53106206bcc72bb22"
GTE_FILES = [
    ("onnx_model", "onnx/model.onnx", None),
    ("tokenizer", "tokenizer.json", None),
    ("config", "config.json", None),
    ("special_tokens_map", "special_tokens_map.json", None),
    ("tokenizer_config", "tokenizer_config.json", None),
]
ARCTIC_MODEL_ID = "Snowflake/snowflake-arctic-embed-l-v2.0"
ARCTIC_REVISION = "ac6544c8a46e00af67e330e85a9028c66b8cfd9a"
ARCTIC_FILES = [
    ("onnx_model", "onnx/model.onnx", None),
    ("onnx_external_data", "onnx/model.onnx_data", "model.onnx_data"),
    ("tokenizer", "tokenizer.json", None),
    ("config", "config.json", None),
    ("special_tokens_map", "special_tokens_map.json", None),
    ("tokenizer_config", "tokenizer_config.json", None),
]
DRAGONKUE_UNAVAILABLE = {
    "candidate": "dragonkue-ko",
    "model_id": "dragonkue/snowflake-arctic-embed-l-v2.0-ko",
    "resolved_revision": "55ec6e9358a56d56af759bc8372e970caf8c305f",
    "required_artifact": "onnx/model.onnx",
    "availability": "missing_at_immutable_revision",
    "checked_at": "2026-07-10T17:45:46Z",
    "authentication": "none",
    "evidence": {
        "revision_http_status": 200,
        "tree_http_status": 200,
        "tree_entry_count": 12,
        "required_artifact_matches": 0,
        "tree_sha256": "3440d1cf94a3c8664310e4b0b03cb57da5a7e132fea5fa6087618a580aee6219",
        "path_sha256": "9e4c07c5352f95ac48d195ab5be417240ab20f1f773da95836b5c69ec7337dc0",
        "resolve_http_status": 404,
        "resolve_error": "EntryNotFound",
        "resolve_revision": "55ec6e9358a56d56af759bc8372e970caf8c305f",
    },
}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def download(url: str, destination: Path) -> int:
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_suffix(destination.suffix + ".partial")
    result = subprocess.run(
        [
            "curl",
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--user-agent",
            "qgh-live-model-eval/1",
            "--output",
            str(temporary),
            "--write-out",
            "%{size_download}",
            url,
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    os.replace(temporary, destination)
    return int(float(result.stdout.strip()))


def snapshot_sha256(root: Path) -> str:
    files = []
    for path in sorted(path for path in root.rglob("*") if path.is_file()):
        files.append(
            {
                "relative_path": path.relative_to(root).as_posix(),
                "byte_size": path.stat().st_size,
                "sha256": sha256_file(path),
            }
        )
    canonical = json.dumps(files, ensure_ascii=False, separators=(",", ":"))
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output-root", type=Path, default=Path("target/qgh-eval/models"))
    parser.add_argument(
        "--offline",
        action="store_true",
        help="fail instead of downloading a missing artifact",
    )
    args = parser.parse_args()
    prior_records = load_prior_prepared_records(args.output_root)
    summaries = []
    summaries.append(
        prepare_manifest(
            args.output_root / "gte-modernbert-base",
            "gte-modernbert-base",
            GTE_MODEL_ID,
            GTE_REVISION,
            GTE_FILES,
            pooling="cls",
            query_prefix="",
            document_prefix="",
            native_dimension=768,
            max_length=8192,
            offline=args.offline,
            prior_record=prior_records.get("gte-modernbert-base"),
        )
    )
    arctic_cache = (
        Path.home()
        / ".cache/qgh/hf/models--Snowflake--snowflake-arctic-embed-l-v2.0/snapshots"
        / ARCTIC_REVISION
    )
    summaries.append(
        prepare_manifest(
            args.output_root / "arctic-embed-l-v2.0",
            "arctic-embed-l-v2.0",
            ARCTIC_MODEL_ID,
            ARCTIC_REVISION,
            ARCTIC_FILES,
            pooling="cls",
            query_prefix="query: ",
            document_prefix="",
            native_dimension=1024,
            max_length=8192,
            local_cache=arctic_cache,
            offline=args.offline,
            prior_record=prior_records.get("arctic-embed-l-v2.0"),
        )
    )
    provenance = {
        "schema_version": "qgh.live_model_preparation.v1",
        "prepared": summaries,
        "unavailable": [DRAGONKUE_UNAVAILABLE],
    }
    args.output_root.mkdir(parents=True, exist_ok=True)
    (args.output_root / "preparation-provenance.json").write_text(
        json.dumps(provenance, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(json.dumps(provenance, sort_keys=True))


def prepare_manifest(
    root: Path,
    candidate: str,
    model_id: str,
    revision: str,
    files,
    *,
    pooling: str,
    query_prefix: str,
    document_prefix: str,
    native_dimension: int,
    max_length: int,
    local_cache: Path = None,
    offline: bool = False,
    prior_record=None,
):
    if (
        not isinstance(prior_record, dict)
        or prior_record.get("candidate") != candidate
        or prior_record.get("model_id") != model_id
        or prior_record.get("resolved_revision") != revision
    ):
        prior_record = None
    prior_artifacts = load_prior_manifest_artifacts(
        root, candidate, model_id, revision, prior_record
    )
    artifacts = []
    artifact_acquisition = []
    for role, relative_path, initializer in files:
        url = f"https://huggingface.co/{model_id}/resolve/{revision}/{relative_path}"
        destination = root / relative_path
        cached = local_cache / relative_path if local_cache is not None else None
        artifact_sha256 = None
        if destination.is_file() and destination.stat().st_size > 0:
            artifact_sha256 = sha256_file(destination)
            preserved = preserved_acquisition(
                prior_record,
                prior_artifacts,
                relative_path,
                destination.stat().st_size,
                artifact_sha256,
            )
            if preserved is not None:
                source = preserved["source"]
                source_bytes = preserved["source_bytes"]
                transfer_bytes = preserved["download_transfer_bytes"]
            else:
                source = "existing_snapshot"
                source_bytes = destination.stat().st_size
                transfer_bytes = 0
        else:
            if cached is not None and cached.exists():
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(cached.resolve(), destination)
                source = "local_cache"
                source_bytes = cached.stat().st_size
                transfer_bytes = 0
            else:
                if offline:
                    raise RuntimeError(
                        f"offline preparation is missing required artifact: {candidate}/{relative_path}"
                    )
                transfer_bytes = download(url, destination)
                source = "curl"
                source_bytes = destination.stat().st_size
        artifact = {
            "role": role,
            "relative_path": relative_path,
            "sha256": artifact_sha256 or sha256_file(destination),
            "byte_size": destination.stat().st_size,
        }
        if initializer is not None:
            artifact["external_initializer_name"] = initializer
        artifacts.append(artifact)
        artifact_acquisition.append(
            {
                "relative_path": relative_path,
                "source": source,
                "source_bytes": source_bytes,
                "download_transfer_bytes": transfer_bytes,
            }
        )
    manifest = {
        "schema_version": "qgh.model_manifest.v1",
        "preset_id": None,
        "provider": "fastembed",
        "model_source": {
            "type": "hf",
            "model_id": model_id,
            "resolved_revision": revision,
        },
        "artifacts": artifacts,
        "tokenizer": "hf_tokenizer_json",
        "query_prefix": query_prefix,
        "document_prefix": document_prefix,
        "pooling": pooling,
        "normalization": "l2",
        "native_dimension": native_dimension,
        "output_dimension": native_dimension,
        "max_length": max_length,
        "quantization": "none",
        "context_template_version": "qgh.context.v1",
    }
    manifest_path = root / "manifest.json"
    manifest_path.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    summary = {
        "candidate": candidate,
        "model_id": model_id,
        "resolved_revision": revision,
        "manifest_file": f"{candidate}/manifest.json",
        "manifest_sha256": sha256_file(manifest_path),
        "prepared_snapshot_sha256": snapshot_sha256(root),
        "snapshot_bytes": sum(artifact["byte_size"] for artifact in artifacts)
        + manifest_path.stat().st_size,
        "download_transfer_bytes": sum(
            artifact["download_transfer_bytes"] for artifact in artifact_acquisition
        ),
        "cache_source_bytes": sum(
            artifact["source_bytes"]
            for artifact in artifact_acquisition
            if artifact["source"] == "local_cache"
        ),
        "existing_snapshot_bytes": sum(
            artifact["source_bytes"]
            for artifact in artifact_acquisition
            if artifact["source"] == "existing_snapshot"
        ),
        "artifact_acquisition": artifact_acquisition,
    }
    return summary


def load_prior_prepared_records(output_root: Path):
    path = output_root / "preparation-provenance.json"
    if not path.is_file():
        return {}
    try:
        provenance = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    if provenance.get("schema_version") != "qgh.live_model_preparation.v1":
        return {}
    return {
        record.get("candidate"): record
        for record in provenance.get("prepared", [])
        if isinstance(record, dict) and isinstance(record.get("candidate"), str)
    }


def load_prior_manifest_artifacts(
    root: Path, candidate: str, model_id: str, revision: str, prior_record
):
    if not isinstance(prior_record, dict):
        return {}
    manifest_path = root / "manifest.json"
    if (
        not manifest_path.is_file()
        or prior_record.get("manifest_file") != f"{candidate}/manifest.json"
        or prior_record.get("manifest_sha256") != sha256_file(manifest_path)
    ):
        return {}
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    source = manifest.get("model_source", {})
    if (
        manifest.get("schema_version") != "qgh.model_manifest.v1"
        or source.get("type") != "hf"
        or source.get("model_id") != model_id
        or source.get("resolved_revision") != revision
    ):
        return {}
    artifacts = manifest.get("artifacts")
    if not isinstance(artifacts, list):
        return {}
    artifact_map = {
        artifact.get("relative_path"): artifact
        for artifact in artifacts
        if isinstance(artifact, dict)
        and isinstance(artifact.get("relative_path"), str)
        and isinstance(artifact.get("byte_size"), int)
        and isinstance(artifact.get("sha256"), str)
    }
    return artifact_map if len(artifact_map) == len(artifacts) else {}


def preserved_acquisition(
    prior_record,
    prior_artifacts,
    relative_path: str,
    byte_size: int,
    artifact_sha256: str,
):
    if not isinstance(prior_record, dict):
        return None
    prior_artifact = prior_artifacts.get(relative_path)
    if (
        not isinstance(prior_artifact, dict)
        or prior_artifact.get("byte_size") != byte_size
        or prior_artifact.get("sha256") != artifact_sha256
    ):
        return None
    matches = [
        artifact
        for artifact in prior_record.get("artifact_acquisition", [])
        if isinstance(artifact, dict)
        and artifact.get("relative_path") == relative_path
    ]
    if len(matches) != 1:
        return None
    artifact = matches[0]
    source = artifact.get("source")
    source_bytes = artifact.get("source_bytes")
    transfer_bytes = artifact.get("download_transfer_bytes")
    if source not in {"curl", "local_cache", "existing_snapshot"}:
        return None
    if source_bytes != byte_size or not isinstance(transfer_bytes, int):
        return None
    if source == "curl" and transfer_bytes != byte_size:
        return None
    if source != "curl" and transfer_bytes != 0:
        return None
    return artifact


if __name__ == "__main__":
    main()
