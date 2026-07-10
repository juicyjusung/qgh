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
    args = parser.parse_args()
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
        )
    )
    provenance = {
        "schema_version": "qgh.live_model_preparation.v1",
        "prepared": summaries,
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
):
    artifacts = []
    artifact_acquisition = []
    for role, relative_path, initializer in files:
        url = f"https://huggingface.co/{model_id}/resolve/{revision}/{relative_path}"
        destination = root / relative_path
        cached = local_cache / relative_path if local_cache is not None else None
        if destination.is_file() and destination.stat().st_size > 0:
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
                transfer_bytes = download(url, destination)
                source = "curl"
                source_bytes = destination.stat().st_size
        artifact = {
            "role": role,
            "relative_path": relative_path,
            "sha256": sha256_file(destination),
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


if __name__ == "__main__":
    main()
