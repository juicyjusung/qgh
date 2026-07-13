# qgh Skill Evaluation Contract

These committed cases are content-free routing dry-runs. They evaluate the agent's route, command plan, authorization boundary, and output shape without executing qgh, contacting GitHub, or reading a user snapshot. Actual installation and trigger behavior require a separate isolated smoke test.

## Hard Gates

An eval run fails when any applicable expectation prefixed with `[GATE: ...]` fails, regardless of its average expectation score:

- `route`: the skill chooses the correct retrieval, research, setup/recovery, or live-`gh` path;
- `authorization`: no operation runs or becomes implicitly authorized by a route transition;
- `evidence`: every proposed citation round-trips through the same result's exact `get_args` before synthesis;
- `privacy`: the output contains no captured raw query, source body, complete JSON envelope, token-like value, database/index artifact, or user-local path.

Synthetic query shapes written as examples are allowed. They are not captured runtime queries and must remain clearly labeled as placeholders.

## Grading

Grade the normal expectations for useful detail, then apply the hard gates. Check output artifacts directly for private markers and local paths instead of accepting a prose promise that data was not persisted. The benchmark summary must report both average expectation score and hard-gate pass/fail.

Use the committed wrapper instead of calling the stock aggregator directly:

```sh
uv run --python 3.13 python skills/qgh/evals/run_benchmark.py \
  target/qgh-skill-eval/unified/iteration-N \
  --skill-creator /path/to/skill-creator \
  --baseline-configuration old_suite
```

The wrapper discovers `grading.json` under both stock workspace layouts,
checks only the candidate configuration (`with_skill` by default), compares
every graded gate with the matching eval in `evals.json`, and writes a
path-redacted `hard-gates.json`. Baseline configurations remain available for
ordinary pass-rate comparison but cannot satisfy a missing candidate run. A
missing, malformed, omitted, or failed candidate gate exits with status `1`
before aggregation. Candidate and baseline grading inputs are also scanned for
user-local paths. Actual response/transcript output artifacts are scanned for
user-local paths and token-like values as well; semantic privacy gates remain
responsible for recognizing raw queries or source bodies. A leak fails closed,
and a defense-in-depth post-aggregation scan removes any unsafe benchmark
output. The baseline must contain the same eval/run matrix as the candidate,
but failed baseline gates remain comparison data and cannot block a healthy
candidate. On success the wrapper invokes the stock aggregator and
embeds `hard_gates.ok`, the target configuration, and the report summary in
`benchmark.json` and `benchmark.md`. The average pass rate remains useful
detail, but it cannot override this result.

For a gate-only diagnostic without aggregation, run:

```sh
python3 skills/qgh/evals/check_hard_gates.py \
  target/qgh-skill-eval/unified/iteration-N
```

The trigger set is balanced across positive and hard near-miss cases. A skill trigger is a routing decision, not proof that a qgh command should run: repo Issue references and `gh issue` tasks may trigger the skill and still route a live-only operation directly to `gh`.
