#!/usr/bin/env python3
"""Regression tests for the qgh skill benchmark hard gates."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from check_hard_gates import check_workspace
from run_benchmark import normalize_benchmark, render_benchmark_markdown


GATES = [
    "[GATE: route] candidate chooses the qgh route",
    "[GATE: authorization] candidate does not mutate state",
    "[GATE: privacy] candidate does not persist local data",
]


class HardGateTests(unittest.TestCase):
    def test_candidate_comparison_is_normalized_before_reporting(self) -> None:
        benchmark = {
            "metadata": {"runs_per_configuration": 3},
            "runs": [
                {"eval_id": 1, "configuration": "old_suite", "run_number": 1},
                {"eval_id": 1, "configuration": "with_skill", "run_number": 1},
            ],
            "run_summary": {
                "old_suite": {
                    "pass_rate": {"mean": 0.9},
                    "time_seconds": {"mean": 2.0},
                    "tokens": {"mean": 100.0},
                },
                "with_skill": {
                    "pass_rate": {"mean": 1.0},
                    "time_seconds": {"mean": 1.0},
                    "tokens": {"mean": 80.0},
                },
                "delta": {
                    "pass_rate": "-0.10",
                    "time_seconds": "+1.0",
                    "tokens": "+20",
                },
            },
        }

        baseline = normalize_benchmark(benchmark, "with_skill", "old_suite")

        self.assertEqual(baseline, "old_suite")
        self.assertEqual(
            list(benchmark["run_summary"]),
            ["with_skill", "old_suite", "delta"],
        )
        self.assertEqual(benchmark["run_summary"]["delta"]["pass_rate"], "+0.10")
        self.assertEqual(benchmark["run_summary"]["delta"]["time_seconds"], "-1.0")
        self.assertEqual(benchmark["metadata"]["runs_per_configuration"], 1)
        self.assertIn("+10.0 pp", render_benchmark_markdown(benchmark))

    def test_candidate_comparison_requires_a_complete_matching_baseline(self) -> None:
        missing = {
            "metadata": {},
            "runs": [
                {"eval_id": 1, "configuration": "with_skill", "run_number": 1}
            ],
            "run_summary": {
                "with_skill": {
                    "pass_rate": {"mean": 1.0},
                    "time_seconds": {"mean": 0.0},
                    "tokens": {"mean": 0.0},
                }
            },
        }
        with self.assertRaisesRegex(ValueError, "baseline"):
            normalize_benchmark(missing, "with_skill", "old_suite")

        incomplete = json.loads(json.dumps(missing))
        incomplete["runs"].append(
            {"eval_id": 2, "configuration": "old_suite", "run_number": 1}
        )
        incomplete["run_summary"]["old_suite"] = {
            "pass_rate": {"mean": 0.5},
            "time_seconds": {"mean": 0.0},
            "tokens": {"mean": 0.0},
        }
        with self.assertRaisesRegex(ValueError, "same eval runs"):
            normalize_benchmark(incomplete, "with_skill", "old_suite")

        for boolean_field in ("eval_id", "run_number"):
            with self.subTest(field=boolean_field):
                boolean_matrix = {
                    "metadata": {},
                    "runs": [
                        {
                            "eval_id": 1,
                            "configuration": configuration,
                            "run_number": 1,
                            boolean_field: True,
                        }
                        for configuration in ("with_skill", "old_suite")
                    ],
                    "run_summary": {
                        configuration: {
                            "pass_rate": {"mean": 1.0},
                            "time_seconds": {"mean": 0.0},
                            "tokens": {"mean": 0.0},
                        }
                        for configuration in ("with_skill", "old_suite")
                    },
                }
                with self.assertRaisesRegex(ValueError, "malformed benchmark run"):
                    normalize_benchmark(
                        boolean_matrix, "with_skill", "old_suite"
                    )

        valid_pair = {
            "metadata": {},
            "runs": [
                {"eval_id": 1, "configuration": "with_skill", "run_number": 1},
                {"eval_id": 1, "configuration": "old_suite", "run_number": 1},
            ],
            "run_summary": {
                configuration: {
                    "pass_rate": {"mean": 1.0},
                    "time_seconds": {"mean": 0.0},
                    "tokens": {"mean": 0.0},
                }
                for configuration in ("with_skill", "old_suite")
            },
        }
        valid_pair["runs"].append(
            {"eval_id": True, "configuration": "old_suite", "run_number": 2}
        )
        with self.assertRaisesRegex(ValueError, "malformed benchmark run"):
            normalize_benchmark(valid_pair, "with_skill", "old_suite")

    def write_eval_set(self, root: Path) -> Path:
        path = root / "evals.json"
        path.write_text(
            json.dumps(
                {
                    "evals": [
                        {
                            "id": 1,
                            "expectations": GATES,
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )
        return path

    def write_grading(
        self,
        workspace: Path,
        configuration: str,
        *,
        passed: bool,
        evidence: str = "fixture",
    ) -> Path:
        path = workspace / "eval-1" / configuration / "run-1" / "grading.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            json.dumps(
                {
                    "expectations": [
                        {"text": gate, "passed": passed, "evidence": evidence}
                        for gate in GATES
                    ],
                    "summary": {
                        "passed": len(GATES) if passed else 0,
                        "failed": 0 if passed else len(GATES),
                        "total": len(GATES),
                        "pass_rate": 1.0 if passed else 0.0,
                    },
                }
            ),
            encoding="utf-8",
        )
        return path

    def test_candidate_configuration_is_isolated_from_failing_baseline(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            workspace = root / "iteration"
            eval_set = self.write_eval_set(root)
            self.write_grading(workspace, "with_skill", passed=True)
            self.write_grading(workspace, "old_suite", passed=False)

            report = check_workspace(
                workspace, eval_set, target_configuration="with_skill"
            )

            self.assertTrue(report["ok"])
            self.assertEqual(report["target_configuration"], "with_skill")
            self.assertEqual(report["summary"]["runs"], 1)
            self.assertEqual(
                report["results"][0]["path"],
                "eval-1/with_skill/run-1/grading.json",
            )
            self.assertNotIn(temporary, json.dumps(report))

    def test_duplicate_gate_fails_in_both_orders(self) -> None:
        for duplicate_values in ((False, True), (True, False)):
            with self.subTest(values=duplicate_values):
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    workspace = root / "iteration"
                    eval_set = self.write_eval_set(root)
                    grading_path = self.write_grading(
                        workspace, "with_skill", passed=True
                    )
                    grading = json.loads(grading_path.read_text(encoding="utf-8"))
                    grading["expectations"] = [
                        {
                            "text": GATES[0],
                            "passed": value,
                            "evidence": "fixture",
                        }
                        for value in duplicate_values
                    ] + [
                        {"text": gate, "passed": True, "evidence": "fixture"}
                        for gate in GATES[1:]
                    ]
                    grading_path.write_text(json.dumps(grading), encoding="utf-8")

                    report = check_workspace(workspace, eval_set)

                    self.assertFalse(report["ok"])
                    self.assertEqual(
                        report["results"][0]["duplicates"], ["[GATE: route]"]
                    )

    def test_invalid_target_configuration_is_redacted(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            invalid_configuration = "/" + "Users" + "/alice/private"
            report = check_workspace(
                root / "iteration",
                self.write_eval_set(root),
                target_configuration=invalid_configuration,
            )

            rendered = json.dumps(report)
            self.assertFalse(report["ok"])
            self.assertEqual(report["target_configuration"], "<invalid>")
            self.assertNotIn(invalid_configuration, rendered)

    def test_eval_set_rejects_empty_duplicate_and_boolean_ids(self) -> None:
        invalid_payloads = {
            "empty": {"evals": []},
            "duplicate": {
                "evals": [
                    {"id": 1, "expectations": [GATES[0]]},
                    {"id": 1, "expectations": [GATES[1]]},
                ]
            },
            "boolean": {
                "evals": [{"id": True, "expectations": [GATES[0]]}]
            },
            "malformed": {
                "evals": [
                    {
                        "id": 1,
                        "expectations": [
                            GATES[0],
                            "[GATE: Privacy] malformed category casing",
                        ],
                    }
                ]
            },
        }
        for name, payload in invalid_payloads.items():
            with self.subTest(case=name):
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    eval_set = root / "evals.json"
                    eval_set.write_text(json.dumps(payload), encoding="utf-8")

                    report = check_workspace(root / "iteration", eval_set)

                    self.assertFalse(report["ok"])
                    self.assertIn(name, report["error"])

    def test_eval_metadata_rejects_boolean_id(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            workspace = root / "iteration"
            eval_set = self.write_eval_set(root)
            grading = workspace / "case" / "with_skill" / "run-1" / "grading.json"
            grading.parent.mkdir(parents=True)
            grading.write_text(
                json.dumps(
                    {
                        "expectations": [
                            {"text": gate, "passed": True, "evidence": "fixture"}
                            for gate in GATES
                        ]
                    }
                ),
                encoding="utf-8",
            )
            (workspace / "case" / "eval_metadata.json").write_text(
                '{"eval_id":true}', encoding="utf-8"
            )

            report = check_workspace(workspace, eval_set)

            self.assertFalse(report["ok"])
            self.assertEqual(report["missing_evals"], [1])

    def test_gate_grading_requires_non_empty_evidence(self) -> None:
        for evidence in (None, ""):
            with self.subTest(evidence=evidence):
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    workspace = root / "iteration"
                    eval_set = self.write_eval_set(root)
                    grading_path = self.write_grading(
                        workspace, "with_skill", passed=True
                    )
                    grading = json.loads(grading_path.read_text(encoding="utf-8"))
                    for expectation in grading["expectations"]:
                        if evidence is None:
                            expectation.pop("evidence")
                        else:
                            expectation["evidence"] = evidence
                    grading_path.write_text(json.dumps(grading), encoding="utf-8")

                    report = check_workspace(workspace, eval_set)

                    self.assertFalse(report["ok"])
                    self.assertEqual(
                        report["results"][0]["malformed"],
                        ["[GATE: authorization]", "[GATE: privacy]", "[GATE: route]"],
                    )

    def test_baseline_cannot_satisfy_a_missing_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            workspace = root / "iteration"
            eval_set = self.write_eval_set(root)
            self.write_grading(workspace, "old_suite", passed=True)

            report = check_workspace(
                workspace, eval_set, target_configuration="with_skill"
            )

            self.assertFalse(report["ok"])
            self.assertEqual(report["missing_evals"], [1])
            self.assertEqual(report["summary"]["runs"], 0)
            self.assertNotIn(temporary, json.dumps(report))

    def test_wrapper_artifacts_do_not_persist_absolute_local_paths(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            workspace = root / "iteration"
            eval_set = self.write_eval_set(root)
            self.write_grading(workspace, "with_skill", passed=True)
            skill_creator = root / "skill-creator"
            aggregate_script = skill_creator / "scripts" / "aggregate_benchmark.py"
            aggregate_script.parent.mkdir(parents=True)
            aggregate_script.write_text(
                """\
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("workspace", type=Path)
parser.add_argument("--skill-name")
parser.add_argument("--skill-path")
args = parser.parse_args()
(args.workspace / "benchmark.json").write_text(json.dumps({
    "metadata": {"skill_path": args.skill_path},
    "runs": [
        {"eval_id": 1, "configuration": "with_skill", "run_number": 1},
        {"eval_id": 1, "configuration": "old_suite", "run_number": 1}
    ],
    "run_summary": {
        "with_skill": {
            "pass_rate": {"mean": 1.0},
            "time_seconds": {"mean": 0.0},
            "tokens": {"mean": 0.0}
        },
        "old_suite": {
            "pass_rate": {"mean": 0.5},
            "time_seconds": {"mean": 0.0},
            "tokens": {"mean": 0.0}
        }
    }
}))
(args.workspace / "benchmark.md").write_text("# Benchmark\\n")
""",
                encoding="utf-8",
            )

            completed = subprocess.run(
                [
                    sys.executable,
                    str(Path(__file__).with_name("run_benchmark.py")),
                    str(workspace),
                    "--skill-creator",
                    str(skill_creator),
                    "--eval-set",
                    str(eval_set),
                    "--target-configuration",
                    "with_skill",
                ],
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertNotIn(temporary, completed.stdout + completed.stderr)
            artifacts = (
                (workspace / "benchmark.json").read_text(encoding="utf-8")
                + (workspace / "benchmark.md").read_text(encoding="utf-8")
                + (workspace / "hard-gates.json").read_text(encoding="utf-8")
            )
            self.assertNotIn(temporary, artifacts)
            benchmark = json.loads(
                (workspace / "benchmark.json").read_text(encoding="utf-8")
            )
            self.assertEqual(benchmark["metadata"]["skill_path"], "skills/qgh")

    def test_wrapper_fails_closed_on_candidate_or_baseline_local_paths(self) -> None:
        for leaking_configuration in ("with_skill", "old_suite"):
            with self.subTest(configuration=leaking_configuration):
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    workspace = root / "iteration"
                    eval_set = self.write_eval_set(root)
                    self.write_grading(workspace, "with_skill", passed=True)
                    if leaking_configuration == "old_suite":
                        self.write_grading(
                            workspace,
                            "old_suite",
                            passed=True,
                            evidence=str(root / "private-evidence.txt"),
                        )
                    else:
                        self.write_grading(
                            workspace,
                            "with_skill",
                            passed=True,
                            evidence=str(root / "private-evidence.txt"),
                        )

                    skill_creator = root / "skill-creator"
                    aggregate_script = (
                        skill_creator / "scripts" / "aggregate_benchmark.py"
                    )
                    aggregate_script.parent.mkdir(parents=True)
                    aggregate_script.write_text(
                        """\
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("workspace", type=Path)
parser.add_argument("--skill-name")
parser.add_argument("--skill-path")
args = parser.parse_args()
gradings = [json.loads(path.read_text()) for path in args.workspace.rglob("grading.json")]
(args.workspace / "benchmark.json").write_text(json.dumps({"gradings": gradings}))
(args.workspace / "benchmark.md").write_text("# Benchmark\\n")
""",
                        encoding="utf-8",
                    )

                    completed = subprocess.run(
                        [
                            sys.executable,
                            str(Path(__file__).with_name("run_benchmark.py")),
                            str(workspace),
                            "--skill-creator",
                            str(skill_creator),
                            "--eval-set",
                            str(eval_set),
                        ],
                        check=False,
                        capture_output=True,
                        text=True,
                    )

                    self.assertEqual(completed.returncode, 1)
                    self.assertIn("artifact privacy check failed", completed.stderr)
                    self.assertFalse((workspace / "benchmark.json").exists())
                    report = (workspace / "hard-gates.json").read_text(
                        encoding="utf-8"
                    )
                    self.assertNotIn(temporary, report)

    def test_wrapper_scans_actual_output_artifacts_for_sensitive_markers(self) -> None:
        for marker_kind in ("local-path", "token-like"):
            with self.subTest(marker=marker_kind):
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    workspace = root / "iteration"
                    eval_set = self.write_eval_set(root)
                    self.write_grading(workspace, "with_skill", passed=True)
                    output = (
                        workspace
                        / "eval-1"
                        / "with_skill"
                        / "run-1"
                        / "outputs"
                        / "response.md"
                    )
                    output.parent.mkdir(parents=True)
                    marker = (
                        str(root / "private-output.txt")
                        if marker_kind == "local-path"
                        else "github" + "_pat_" + "A" * 32
                    )
                    output.write_text(marker, encoding="utf-8")
                    skill_creator = root / "skill-creator"
                    aggregate_script = (
                        skill_creator / "scripts" / "aggregate_benchmark.py"
                    )
                    aggregate_script.parent.mkdir(parents=True)
                    aggregate_script.write_text(
                        "raise SystemExit('must not run')\n", encoding="utf-8"
                    )

                    completed = subprocess.run(
                        [
                            sys.executable,
                            str(Path(__file__).with_name("run_benchmark.py")),
                            str(workspace),
                            "--skill-creator",
                            str(skill_creator),
                            "--eval-set",
                            str(eval_set),
                        ],
                        check=False,
                        capture_output=True,
                        text=True,
                    )

                    self.assertEqual(completed.returncode, 1)
                    self.assertIn("artifact privacy check failed", completed.stderr)
                    self.assertFalse((workspace / "benchmark.json").exists())
                    report = (workspace / "hard-gates.json").read_text(
                        encoding="utf-8"
                    )
                    self.assertNotIn(marker, report)
                    self.assertNotIn(marker, completed.stdout + completed.stderr)

    def test_privacy_failure_redacts_gate_text_from_report(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            workspace = root / "iteration"
            leaked_gate = f"[GATE: privacy] no path {root / 'private.txt'}"
            eval_set = root / "evals.json"
            eval_set.write_text(
                json.dumps(
                    {"evals": [{"id": 1, "expectations": [leaked_gate]}]}
                ),
                encoding="utf-8",
            )
            grading = workspace / "eval-1" / "with_skill" / "run-1" / "grading.json"
            grading.parent.mkdir(parents=True)
            grading.write_text(
                json.dumps(
                    {
                        "expectations": [
                            {
                                "text": leaked_gate,
                                "passed": True,
                                "evidence": "fixture",
                            }
                        ]
                    }
                ),
                encoding="utf-8",
            )
            skill_creator = root / "skill-creator"
            aggregate_script = skill_creator / "scripts" / "aggregate_benchmark.py"
            aggregate_script.parent.mkdir(parents=True)
            aggregate_script.write_text(
                "raise SystemExit('must not run')\n", encoding="utf-8"
            )

            completed = subprocess.run(
                [
                    sys.executable,
                    str(Path(__file__).with_name("run_benchmark.py")),
                    str(workspace),
                    "--skill-creator",
                    str(skill_creator),
                    "--eval-set",
                    str(eval_set),
                ],
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertEqual(completed.returncode, 1)
            report = (workspace / "hard-gates.json").read_text(encoding="utf-8")
            self.assertNotIn(temporary, report)
            self.assertIn("details redacted", report)

    def test_failure_paths_remove_stale_or_partial_benchmarks(self) -> None:
        scenarios = ("hard-gate", "aggregator-nonzero", "output-leak")
        for scenario in scenarios:
            with self.subTest(scenario=scenario):
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    workspace = root / "iteration"
                    eval_set = self.write_eval_set(root)
                    self.write_grading(
                        workspace,
                        "with_skill",
                        passed=scenario != "hard-gate",
                    )
                    workspace.mkdir(parents=True, exist_ok=True)
                    (workspace / "benchmark.json").write_text(
                        '{"hard_gates":{"ok":true}}', encoding="utf-8"
                    )
                    (workspace / "benchmark.md").write_text(
                        "stale pass", encoding="utf-8"
                    )
                    skill_creator = root / "skill-creator"
                    aggregate_script = (
                        skill_creator / "scripts" / "aggregate_benchmark.py"
                    )
                    aggregate_script.parent.mkdir(parents=True)
                    leaked_value = str(root / "private-output.txt")
                    exit_code = 9 if scenario == "aggregator-nonzero" else 0
                    aggregate_script.write_text(
                        f"""\
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("workspace", type=Path)
parser.add_argument("--skill-name")
parser.add_argument("--skill-path")
args = parser.parse_args()
payload = {{
    "metadata": {{"skill_path": args.skill_path}},
    "runs": [{{"eval_id": 1, "configuration": "with_skill", "run_number": 1}}],
    "run_summary": {{
        "with_skill": {{
            "pass_rate": {{"mean": 1.0}},
            "time_seconds": {{"mean": 0.0}},
            "tokens": {{"mean": 0.0}}
        }}
    }},
    "diagnostic": {leaked_value!r}
}}
(args.workspace / "benchmark.json").write_text(json.dumps(payload))
(args.workspace / "benchmark.md").write_text({leaked_value!r})
raise SystemExit({exit_code})
""",
                        encoding="utf-8",
                    )

                    completed = subprocess.run(
                        [
                            sys.executable,
                            str(Path(__file__).with_name("run_benchmark.py")),
                            str(workspace),
                            "--skill-creator",
                            str(skill_creator),
                            "--eval-set",
                            str(eval_set),
                        ],
                        check=False,
                        capture_output=True,
                        text=True,
                    )

                    self.assertNotEqual(completed.returncode, 0)
                    self.assertFalse((workspace / "benchmark.json").exists())
                    self.assertFalse((workspace / "benchmark.md").exists())
                    self.assertNotIn(temporary, completed.stdout + completed.stderr)
                    self.assertNotIn(
                        temporary,
                        (workspace / "hard-gates.json").read_text(encoding="utf-8"),
                    )


if __name__ == "__main__":
    unittest.main()
