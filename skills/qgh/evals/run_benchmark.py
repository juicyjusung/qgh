#!/usr/bin/env python3
"""Run qgh hard gates, then the stock skill-creator benchmark aggregator."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

from check_hard_gates import CONFIGURATION_PATTERN, check_workspace, write_report


STANDARD_LOCAL_PATH = re.compile(
    r"(?:^|[\s`'\"(=])(?:~[/\\]|[A-Za-z]:\\\\|/"
    r"(?:Users|home|private|tmp|var/folders|Volumes|root|workspace|mnt)(?:/|\\))",
    re.MULTILINE,
)
TOKEN_PREFIXES = ["gh" + kind + "_" for kind in ("p", "o", "u", "s", "r")]
TOKEN_PREFIXES.append("github" + "_pat_")
TOKEN_LIKE_VALUE = re.compile(
    r"(?:" + "|".join(re.escape(prefix) for prefix in TOKEN_PREFIXES) + r")[A-Za-z0-9_]{8,}"
)
AUTHORIZATION_MARKER = "Authorization" + ": " + "Bearer"


def privacy_roots(workspace: Path, eval_set: Path, skill_creator: Path) -> list[str]:
    roots = {
        str(path.absolute())
        for path in (workspace.parent, eval_set.parent, skill_creator, Path.home())
        if str(path.absolute()) not in {"", "/"}
    }
    return sorted(roots, key=len, reverse=True)


def artifact_label(path: Path, workspace: Path) -> str:
    try:
        return path.relative_to(workspace).as_posix()
    except ValueError:
        return path.name


def find_artifact_privacy_leaks(
    paths: list[Path], workspace: Path, roots: list[str]
) -> list[str]:
    leaking_files: list[str] = []
    for path in paths:
        if not path.is_file():
            continue
        try:
            content = path.read_text(encoding="utf-8")
        except (OSError, UnicodeError):
            leaking_files.append(artifact_label(path, workspace))
            continue
        if (
            any(root in content for root in roots)
            or STANDARD_LOCAL_PATH.search(content)
            or TOKEN_LIKE_VALUE.search(content)
            or AUTHORIZATION_MARKER in content
        ):
            leaking_files.append(artifact_label(path, workspace))
    return sorted(set(leaking_files))


def remove_benchmark_artifacts(workspace: Path) -> None:
    for name in ("benchmark.json", "benchmark.md"):
        (workspace / name).unlink(missing_ok=True)


def evaluation_output_paths(workspace: Path) -> list[Path]:
    paths: list[Path] = []
    for path in workspace.rglob("*"):
        if not path.is_file():
            continue
        relative_parts = path.relative_to(workspace).parts
        if "outputs" in relative_parts or path.name in {
            "transcript.md",
            "user_notes.md",
        }:
            paths.append(path)
    return sorted(paths)


def mean_metric(summary: dict[str, Any], configuration: str, metric: str) -> float:
    value = summary.get(configuration, {}).get(metric, {}).get("mean", 0.0)
    return float(value) if isinstance(value, (int, float)) else 0.0


def normalize_benchmark(
    benchmark: dict[str, Any], target_configuration: str, baseline_configuration: str
) -> str:
    summary = benchmark.get("run_summary")
    if not isinstance(summary, dict) or target_configuration not in summary:
        raise ValueError("aggregated benchmark is missing the target configuration")
    if (
        baseline_configuration == target_configuration
        or baseline_configuration not in summary
    ):
        raise ValueError("aggregated benchmark is missing a distinct baseline configuration")

    ordered = {target_configuration: summary[target_configuration]}
    ordered[baseline_configuration] = summary[baseline_configuration]
    ordered["delta"] = {
        "pass_rate": (
            f"{mean_metric(summary, target_configuration, 'pass_rate') - mean_metric(summary, baseline_configuration, 'pass_rate'):+.2f}"
        ),
        "time_seconds": (
            f"{mean_metric(summary, target_configuration, 'time_seconds') - mean_metric(summary, baseline_configuration, 'time_seconds'):+.1f}"
        ),
        "tokens": (
            f"{mean_metric(summary, target_configuration, 'tokens') - mean_metric(summary, baseline_configuration, 'tokens'):+.0f}"
        ),
    }
    benchmark["run_summary"] = ordered

    run_matrix: dict[str, dict[int, set[int]]] = {
        target_configuration: {},
        baseline_configuration: {},
    }
    runs = benchmark.get("runs")
    if not isinstance(runs, list) or not runs:
        raise ValueError("aggregated benchmark has no comparable runs")
    for run in runs:
        if not isinstance(run, dict):
            raise ValueError("aggregated benchmark contains a malformed benchmark run")
        configuration = run.get("configuration")
        eval_id = run.get("eval_id")
        run_number = run.get("run_number")
        if (
            configuration not in run_matrix
            or type(eval_id) is not int
            or type(run_number) is not int
        ):
            raise ValueError("aggregated benchmark contains a malformed benchmark run")
        run_numbers = run_matrix[configuration].setdefault(eval_id, set())
        if run_number in run_numbers:
            raise ValueError("aggregated benchmark contains a duplicate benchmark run")
        run_numbers.add(run_number)
    target_runs = run_matrix[target_configuration]
    baseline_runs = run_matrix[baseline_configuration]
    if not target_runs or target_runs != baseline_runs:
        raise ValueError("target and baseline must contain the same eval runs")
    run_counts = {len(run_numbers) for run_numbers in target_runs.values()}
    if len(run_counts) != 1:
        raise ValueError("every eval must use one consistent run count")
    benchmark.setdefault("metadata", {})["runs_per_configuration"] = run_counts.pop()
    return baseline_configuration


def render_benchmark_markdown(benchmark: dict[str, Any]) -> str:
    metadata = benchmark.get("metadata", {})
    summary = benchmark.get("run_summary", {})
    configurations = [name for name in summary if name != "delta"]
    target = configurations[0] if configurations else "target"
    baseline = configurations[1] if len(configurations) > 1 else "baseline"
    target_summary = summary.get(target, {})
    baseline_summary = summary.get(baseline, {})
    delta = summary.get("delta", {})

    def stats(configuration_summary: dict[str, Any], metric: str) -> dict[str, Any]:
        value = configuration_summary.get(metric, {})
        return value if isinstance(value, dict) else {}

    target_pass = stats(target_summary, "pass_rate")
    baseline_pass = stats(baseline_summary, "pass_rate")
    target_time = stats(target_summary, "time_seconds")
    baseline_time = stats(baseline_summary, "time_seconds")
    target_tokens = stats(target_summary, "tokens")
    baseline_tokens = stats(baseline_summary, "tokens")
    pass_delta_points = (
        float(target_pass.get("mean", 0)) - float(baseline_pass.get("mean", 0))
    ) * 100
    evals = metadata.get("evals_run", [])
    lines = [
        f"# Skill Benchmark: {metadata.get('skill_name', 'qgh')}",
        "",
        f"**Model**: {metadata.get('executor_model', '<model-name>')}",
        f"**Date**: {metadata.get('timestamp', '<timestamp>')}",
        f"**Evals**: {', '.join(map(str, evals))} ({metadata.get('runs_per_configuration', 0)} run(s) each per configuration)",
        "",
        "## Summary",
        "",
        f"| Metric | {target.replace('_', ' ').title()} | {baseline.replace('_', ' ').title()} | Target delta |",
        "| --- | ---: | ---: | ---: |",
        f"| Pass Rate | {float(target_pass.get('mean', 0))*100:.1f}% | {float(baseline_pass.get('mean', 0))*100:.1f}% | {pass_delta_points:+.1f} pp |",
        f"| Time | {float(target_time.get('mean', 0)):.1f}s | {float(baseline_time.get('mean', 0)):.1f}s | {delta.get('time_seconds', 'n/a')}s |",
        f"| Tokens | {float(target_tokens.get('mean', 0)):.0f} | {float(baseline_tokens.get('mean', 0)):.0f} | {delta.get('tokens', 'n/a')} |",
    ]
    notes = benchmark.get("notes", [])
    if isinstance(notes, list) and notes:
        lines.extend(["", "## Notes", "", *[f"- {note}" for note in notes]])
    return "\n".join(lines) + "\n"


def main() -> int:
    if sys.version_info < (3, 9):
        print(
            "Python 3.9 or newer is required; run this wrapper with "
            "`uv run --python 3.13 python ...`.",
            file=sys.stderr,
        )
        return 1

    parser = argparse.ArgumentParser(
        description="Gate and aggregate a qgh skill evaluation workspace."
    )
    parser.add_argument("workspace", type=Path, help="skill-creator iteration directory")
    parser.add_argument(
        "--skill-creator",
        required=True,
        type=Path,
        help="skill-creator directory containing scripts/aggregate_benchmark.py",
    )
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
    parser.add_argument(
        "--baseline-configuration",
        default="old_suite",
        help="complete comparison configuration; its gates do not block the candidate",
    )
    args = parser.parse_args()

    remove_benchmark_artifacts(args.workspace)
    report_path = args.workspace / "hard-gates.json"
    report = check_workspace(
        args.workspace,
        args.eval_set,
        target_configuration=args.target_configuration,
    )
    baseline_is_valid = bool(
        CONFIGURATION_PATTERN.fullmatch(args.baseline_configuration)
    )
    report["baseline_configuration"] = (
        args.baseline_configuration if baseline_is_valid else "<invalid>"
    )
    if not baseline_is_valid:
        report["ok"] = False
        report["error"] = "baseline configuration must be one directory name"
    roots = privacy_roots(args.workspace, args.eval_set, args.skill_creator)
    input_paths = [
        args.eval_set,
        *sorted(args.workspace.rglob("grading.json")),
        *evaluation_output_paths(args.workspace),
    ]
    privacy_leaks = find_artifact_privacy_leaks(input_paths, args.workspace, roots)
    report["artifact_privacy"] = {
        "ok": not privacy_leaks,
        "files": privacy_leaks,
    }
    if privacy_leaks:
        report["ok"] = False
        report["error"] = "artifact privacy check failed; details redacted"
        report["results"] = []
    write_report(report, report_path)
    if not report["ok"]:
        if privacy_leaks:
            remove_benchmark_artifacts(args.workspace)
            print(
                "artifact privacy check failed; benchmark aggregation blocked",
                file=sys.stderr,
            )
            return 1
        print("hard gates failed; benchmark aggregation blocked", file=sys.stderr)
        return 1

    aggregate_script = args.skill_creator / "scripts/aggregate_benchmark.py"
    if not aggregate_script.is_file():
        print(
            "aggregate script not found under supplied skill-creator directory",
            file=sys.stderr,
        )
        return 1

    skill_path = "skills/qgh"
    command = [
        sys.executable,
        str(aggregate_script),
        str(args.workspace),
        "--skill-name",
        "qgh",
        "--skill-path",
        skill_path,
    ]
    try:
        completed = subprocess.run(command, check=False, capture_output=True, text=True)
    except OSError:
        remove_benchmark_artifacts(args.workspace)
        print("stock benchmark aggregation could not start", file=sys.stderr)
        return 1
    if completed.returncode != 0:
        remove_benchmark_artifacts(args.workspace)
        print("stock benchmark aggregation failed", file=sys.stderr)
        return completed.returncode

    benchmark_path = args.workspace / "benchmark.json"
    markdown_path = args.workspace / "benchmark.md"
    output_leaks = find_artifact_privacy_leaks(
        [benchmark_path, markdown_path], args.workspace, roots
    )
    if output_leaks:
        report["ok"] = False
        report["artifact_privacy"] = {"ok": False, "files": output_leaks}
        remove_benchmark_artifacts(args.workspace)
        write_report(report, report_path)
        print(
            "artifact privacy check failed after aggregation; outputs removed",
            file=sys.stderr,
        )
        return 1
    try:
        benchmark = json.loads(benchmark_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        remove_benchmark_artifacts(args.workspace)
        print("cannot read generated benchmark", file=sys.stderr)
        return 1

    try:
        baseline_configuration = normalize_benchmark(
            benchmark,
            args.target_configuration,
            args.baseline_configuration,
        )
    except ValueError as error:
        remove_benchmark_artifacts(args.workspace)
        print(str(error), file=sys.stderr)
        return 1

    benchmark["hard_gates"] = {
        "ok": True,
        "report": report_path.name,
        "target_configuration": args.target_configuration,
        "baseline_configuration": baseline_configuration,
        "summary": report["summary"],
    }
    benchmark_path.write_text(
        json.dumps(benchmark, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )

    markdown_path.write_text(render_benchmark_markdown(benchmark), encoding="utf-8")
    with markdown_path.open("a", encoding="utf-8") as markdown:
        markdown.write(
            "\n\n## Hard Gates\n\n"
            f"- Status: PASS\n"
            f"- Target configuration: `{args.target_configuration}`\n"
            f"- Runs checked: {report['summary']['runs']}\n"
            f"- Report: `{report_path.name}`\n"
        )
    target_rate = mean_metric(
        benchmark["run_summary"], args.target_configuration, "pass_rate"
    )
    baseline_rate = (
        mean_metric(benchmark["run_summary"], baseline_configuration, "pass_rate")
        if baseline_configuration is not None
        else 0.0
    )
    print("benchmark generated: benchmark.json, benchmark.md")
    print(
        f"candidate summary: {args.target_configuration}={target_rate * 100:.1f}% "
        f"baseline={baseline_rate * 100:.1f}% "
        f"delta={target_rate - baseline_rate:+.1%}"
    )
    print("hard-gate status embedded: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
