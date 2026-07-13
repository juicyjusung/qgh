#!/usr/bin/env python3
"""Enforce qgh skill hard gates across a standard skill-creator workspace."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any


GATE_PREFIX = "[GATE:"
GATE_LABEL_PATTERN = re.compile(r"^\[GATE: [a-z][a-z-]*\](?=\s|$)")
EVAL_DIR_PATTERN = re.compile(r"^eval-(\d+)")
CONFIGURATION_PATTERN = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]*$")


def read_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def load_expected_gates(eval_set: Path) -> dict[int, list[str]]:
    payload = read_json(eval_set)
    cases = payload.get("evals") if isinstance(payload, dict) else None
    if not isinstance(cases, list):
        raise ValueError("eval set must contain an evals array")
    if not cases:
        raise ValueError("empty eval set is not allowed")

    expected: dict[int, list[str]] = {}
    for case in cases:
        if isinstance(case, dict) and isinstance(case.get("id"), bool):
            raise ValueError("boolean eval id is not allowed")
        if not isinstance(case, dict) or type(case.get("id")) is not int:
            raise ValueError("every eval must have an integer id")
        if case["id"] in expected:
            raise ValueError(f"duplicate eval id: {case['id']}")
        expectations = case.get("expectations")
        if not isinstance(expectations, list):
            raise ValueError(f"eval {case['id']} expectations must be an array")
        gate_candidates = [
            value
            for value in expectations
            if isinstance(value, str) and value.startswith(GATE_PREFIX)
        ]
        malformed_gates = [
            value for value in gate_candidates if not GATE_LABEL_PATTERN.match(value)
        ]
        if malformed_gates:
            raise ValueError(f"eval {case['id']} has a malformed hard gate")
        gates = gate_candidates
        if not gates:
            raise ValueError(f"eval {case['id']} defines no hard gates")
        expected[case["id"]] = gates
    return expected


def gate_label(text: str) -> str:
    match = GATE_LABEL_PATTERN.match(text)
    return match.group(0) if match else "[GATE: malformed]"


def find_eval_context(grading_path: Path) -> tuple[int, Path]:
    for directory in grading_path.parents:
        metadata_path = directory / "eval_metadata.json"
        if metadata_path.is_file():
            metadata = read_json(metadata_path)
            eval_id = metadata.get("eval_id") if isinstance(metadata, dict) else None
            if type(eval_id) is not int:
                raise ValueError(f"invalid eval_id in {metadata_path.name}")
            return eval_id, directory

        match = EVAL_DIR_PATTERN.match(directory.name)
        if match:
            return int(match.group(1)), directory
    raise ValueError(f"cannot resolve eval id for {grading_path.name}")


def report_path(path: Path, workspace: Path) -> str:
    try:
        return path.relative_to(workspace).as_posix()
    except ValueError:
        return path.name


def redact_error(error: Exception, *roots: Path) -> str:
    message = str(error)
    for root in roots:
        for value in {str(root), str(root.absolute())}:
            if value:
                message = message.replace(value, f"<{root.name or 'path'}>")
    return message


def check_grading(
    grading_path: Path,
    expected_by_eval: dict[int, list[str]],
    workspace: Path,
) -> dict[str, Any]:
    display_path = report_path(grading_path, workspace)
    try:
        eval_id, _ = find_eval_context(grading_path)
        expected = expected_by_eval[eval_id]
        payload = read_json(grading_path)
    except (KeyError, OSError, ValueError, json.JSONDecodeError) as error:
        return {
            "path": display_path,
            "eval_id": None,
            "ok": False,
            "error": redact_error(error, workspace),
            "missing": [],
            "failed": [],
            "duplicates": [],
            "malformed": [],
        }

    expectations = payload.get("expectations") if isinstance(payload, dict) else None
    if not isinstance(expectations, list):
        return {
            "path": display_path,
            "eval_id": eval_id,
            "ok": False,
            "error": "grading expectations must be an array",
            "missing": [gate_label(text) for text in expected],
            "failed": [],
            "duplicates": [],
            "malformed": [],
        }

    graded: dict[str, bool] = {}
    malformed: list[str] = []
    duplicates: list[str] = []
    for expectation in expectations:
        if not isinstance(expectation, dict):
            continue
        text = expectation.get("text")
        if not isinstance(text, str) or not text.startswith(GATE_PREFIX):
            continue
        passed = expectation.get("passed")
        evidence = expectation.get("evidence")
        if not isinstance(passed, bool):
            malformed.append(text)
            passed = False
        if not isinstance(evidence, str) or not evidence.strip():
            malformed.append(text)
            passed = False
        if text in graded:
            duplicates.append(text)
            graded[text] = graded[text] and passed
        else:
            graded[text] = passed

    missing = [text for text in expected if text not in graded]
    failed = [text for text in expected if graded.get(text) is False]
    failed.extend(text for text in malformed if text not in failed)
    failed.extend(text for text in duplicates if text not in failed)
    return {
        "path": display_path,
        "eval_id": eval_id,
        "ok": not missing and not failed,
        "error": None,
        "expected": [gate_label(text) for text in expected],
        "missing": [gate_label(text) for text in missing],
        "failed": [gate_label(text) for text in failed],
        "duplicates": sorted({gate_label(text) for text in duplicates}),
        "malformed": sorted({gate_label(text) for text in malformed}),
    }


def check_workspace(
    workspace: Path,
    eval_set: Path,
    *,
    target_configuration: str = "with_skill",
) -> dict[str, Any]:
    if not CONFIGURATION_PATTERN.fullmatch(target_configuration):
        return {
            "ok": False,
            "error": "target configuration must be one directory name",
            "target_configuration": "<invalid>",
            "results": [],
        }
    try:
        expected_by_eval = load_expected_gates(eval_set)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        return {
            "ok": False,
            "error": redact_error(error, workspace, eval_set.parent),
            "target_configuration": target_configuration,
            "results": [],
        }

    grading_paths = sorted(
        path
        for path in workspace.rglob("grading.json")
        if target_configuration in path.relative_to(workspace).parts
    )

    results = [
        check_grading(path, expected_by_eval, workspace) for path in grading_paths
    ]
    seen_eval_ids = {
        result["eval_id"] for result in results if isinstance(result["eval_id"], int)
    }
    missing_evals = sorted(set(expected_by_eval) - seen_eval_ids)
    ok = all(result["ok"] for result in results) and not missing_evals
    return {
        "ok": ok,
        "error": (
            None
            if grading_paths
            else f"no grading.json files found for target configuration {target_configuration!r}"
        ),
        "eval_set": eval_set.name,
        "workspace": ".",
        "target_configuration": target_configuration,
        "missing_evals": missing_evals,
        "summary": {
            "runs": len(results),
            "passed": sum(1 for result in results if result["ok"]),
            "failed": sum(1 for result in results if not result["ok"]),
        },
        "results": results,
    }


def write_report(report: dict[str, Any], output: Path | None) -> None:
    rendered = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if output is None:
        print(rendered, end="")
        return
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(rendered, encoding="utf-8")
    print(f"hard-gate report: {output.name}")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Enforce qgh hard gates before skill benchmark aggregation."
    )
    parser.add_argument("workspace", type=Path, help="skill-creator iteration directory")
    parser.add_argument(
        "--eval-set",
        type=Path,
        default=Path(__file__).with_name("evals.json"),
        help="qgh evals.json path",
    )
    parser.add_argument(
        "--target-configuration",
        default="with_skill",
        help="candidate configuration whose hard gates must pass",
    )
    parser.add_argument("--output", type=Path, help="optional JSON report path")
    args = parser.parse_args()

    report = check_workspace(
        args.workspace,
        args.eval_set,
        target_configuration=args.target_configuration,
    )
    write_report(report, args.output)
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
