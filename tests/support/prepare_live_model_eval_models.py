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


def download(url: str, destination: Path) -> None:
    if destination.is_file() and destination.stat().st_size > 0:
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_suffix(destination.suffix + ".partial")
    subprocess.run(
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
            url,
        ],
        check=True,
    )
    os.replace(temporary, destination)


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
    print(json.dumps({"prepared": summaries}, sort_keys=True))


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
    for role, relative_path, initializer in files:
        url = f"https://huggingface.co/{model_id}/resolve/{revision}/{relative_path}"
        destination = root / relative_path
        cached = local_cache / relative_path if local_cache is not None else None
        if not destination.is_file() or destination.stat().st_size == 0:
            if cached is not None and cached.exists():
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(cached.resolve(), destination)
            else:
                download(url, destination)
        artifact = {
            "role": role,
            "relative_path": relative_path,
            "sha256": sha256_file(destination),
            "byte_size": destination.stat().st_size,
        }
        if initializer is not None:
            artifact["external_initializer_name"] = initializer
        artifacts.append(artifact)
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
        "context_template_version": "qgh.context.none.v1",
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
        "snapshot_bytes": sum(artifact["byte_size"] for artifact in artifacts)
        + manifest_path.stat().st_size,
    }
    return summary


if __name__ == "__main__":
    main()
