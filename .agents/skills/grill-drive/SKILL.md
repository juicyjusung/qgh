---
name: grill-drive
description: >-
  Drive a grill-with-docs design session at a chosen autonomy level so you are not stuck answering
  one question at a time. Three modes. Reach for `checkpoint` when you want grill-with-docs but faster,
  with yourself still in the loop — it auto-answers low-stakes questions with the recommended answer and
  stops only on critical decisions. Reach for `batch` when you want to walk away: it grinds the whole
  design tree autonomously, queues every critical decision, then brings them all back in one
  consolidated session at the end. Reach for `auto` when you want an unattended first pass that never
  stops and ends with a written report. Invoke as `/grill-drive [checkpoint|batch|auto] <plan or topic>`.
  Default mode when none is given is `batch`.
disable-model-invocation: true
---

# Grill Drive

You run the exact same thing `grill-with-docs` does — a relentless walk down the design tree,
resolving decision dependencies one by one, giving a recommended answer for every question, and
writing ADRs and a glossary via `/domain-modeling` as you go. Do not reimplement any of that here.

**Delegate:** run a `/grilling` session using the `/domain-modeling` skill, but **replace grilling's
interaction protocol** — "ask one question at a time, wait for feedback" — with the protocol of the
mode the user picked, below. This override is intentional and is the entire point of this skill; it is
not a contradiction of grilling. The only things grill-drive changes about grilling are *when it stops
to ask the user*, and *how it keeps the design tree from collapsing once the user is no longer the one
answering.*

Everything else — the recommend-an-answer habit, "if a question can be answered by exploring the
codebase, explore instead," the ADR/glossary discipline — carries over unchanged and is strengthened,
not softened, by the modes.

## Invocation

`/grill-drive [checkpoint|batch|auto] [--deep] <plan or topic>`

Parse the first token as the mode. If it is not one of the three, treat the whole argument as the
plan/topic and use the default mode, **`batch`**. If no plan/topic is given, ask what to grill and stop.

`--deep` (optional, `batch`/`auto` only) trades tokens for thoroughness: all six personas, a higher
round cap, normal effort throughout. Without it, `batch`/`auto` run the **Lean** cost profile described
below — enough breadth for most plans at a fraction of the spend. Reach for `--deep` only when the plan
is high-stakes enough to justify it.

## The three modes

| Mode | Stops to ask? | Human in loop? | Backed by |
|------|---------------|----------------|-----------|
| `checkpoint` | On every **critical** decision; resumes after each answer | Yes, at criticals | Plain interactive skill (this file) |
| `batch` | Never mid-flight; **one** consolidated session at the very end | Yes, only at the end | A Workflow (see `references/workflow.md`) |
| `auto` | Never — produces the final report and ends | No | Same Workflow, stopping disabled |

`auto` is `batch` with stopping disabled — a `--no-stop` flavor of the same code path, not a separate
design. Build `batch` well and `auto` falls out of it.

Before touching any mode, read `references/classifier.md` — you classify every open choice with it.

## checkpoint

A drop-in, faster `grill-with-docs`, with you still holding the wheel on the decisions that matter.

Walk the design tree exactly as grilling does. For each open choice:

- **Auto-decide** (per the classifier): take the recommended answer silently and keep moving. Record it
  in the decision ledger. Do not stop.
- **Critical**: stop, present the decision — options, your recommendation, and why — and wait. Resume
  from the answer.
- **Hard-stop**: stop with a concise blocker note and the single smallest question needed to unblock.

Because you are still answering the critical decisions, you remain the divergence source at every node
that matters, so checkpoint does not need the heavy adversarial machinery that `batch`/`auto` do. But
two habits from the anti-collapse protocol still apply, because between criticals *you* are now
auto-answering and can silently swallow whole subtrees:

- **Breadth-before-collapse** (see below): before you mark a node auto and move on, enumerate the
  questions an interactive grill would have asked under it. Only then collapse.
- **Explore, don't guess**: if the codebase or docs can answer it, go read them.

## batch / auto — do not let the tree collapse

This is the whole reason grill-drive exists, so design around it, not as an afterthought.

**The failure mode:** when a model auto-answers its own questions, it takes the coherent happy path,
seeks consistency with its own stated hypothesis, and grades its own done-ness. It explores a shallower,
narrower tree than an interactive grill and surfaces *fewer* items. In a real grill, most items come
from the *human* acting as a divergence source — contradicting assumptions, adding constraints, revealing
hidden requirements — each spawning new branches. Remove the human and you must manufacture that
adversary, or this mode is strictly worse than grilling by hand.

`batch`/`auto` are backed by a Workflow precisely so the anti-collapse protocol is enforced by code, not
by a model's willpower — independent agents structurally cannot bail early or grade their own done-ness.
The full script sketch is in `references/workflow.md`. Author and run it as described there.

Keep it economical: the anti-collapse guarantee comes from *independent adversaries + loop-until-dry*,
not from brute fan-out. Preserve breadth but cut per-unit cost, which is the **Lean** default — scale
personas to the plan (3 core lenses, all 6 only for complex plans or `--deep`), resolve each round's
challenges in **one** batched judge pass rather than an agent per challenge, synthesize the plan **once**
at the end rather than every round, run the mechanical challenge/classify stages at low effort, and cap
the rounds while honoring any token budget. `references/workflow.md` spells out the profile and why each
lever is safe to pull. The protocol you must preserve in any adaptation:

1. **Adversarial multi-persona challenge rounds.** Never let one model answer its own questions. Each
   round, spawn *independent* challenger subagents, each attacking the current plan from a distinct lens
   — skeptic, on-call/ops, security, future maintainer, product owner, "the user who does the weird
   thing." Each surfaces branches the happy-path model won't. Collect the challenges and dedup against
   everything already raised. Personas and their prompts live in `references/personas.md`.

2. **Separate generation from judgment.** The pass that *generates* questions and attacks must be a
   different agent from the pass that *answers* one with a recommendation. Fused in one breath they
   collapse straight into the happy path — the generator softens its own attacks so the answerer can win.

3. **Loop-until-dry termination, not model-satisfied.** Keep running challenge rounds until **N
   consecutive rounds (default 2) surface nothing new** after dedup. This objective counter is far more
   robust than a vague sense of "this is ready." Treat any urge to stop and hand back early as the signal
   to run *one more* adversarial round instead.

4. **Breadth-before-collapse.** Before you mark any node low-stakes/auto, first enumerate every question
   an interactive grill would ask under it; only then collapse. This stops you from silently swallowing
   an entire subtree by declaring its root "cheap."

5. **Completeness-critic pass at the end.** A final agent asks: "Would an interactive grill have asked
   more here? What did I skip, and exactly why was it safe to?" Anything it finds becomes another round.
   Fold the findings back in before writing the report.

### Persistence

Once `batch` or `auto` starts, do not yield control back until the loop-until-dry condition is met (or a
hard-stop guardrail trips). The stop condition is the concrete N-dry-rounds counter — not "I feel this
is ready." Prefer the counter to any open-ended goal lock; it is what keeps the run honest when no human
is watching.

### Bringing decisions back (batch only)

`batch` never stops mid-flight, but it is not fire-and-forget: at the very end it opens **one**
consolidated decision session over the queued critical decisions (see the report's "Needs your call"
section). If the user overrides any decision, re-grill **only** the branches that depended on it — not
the whole tree. `auto` presents the same material read-only and ends.

## Dependency handling (batch / auto)

This is where naive deferral lies to you. A critical decision may be deferred to the end **only** if
downstream exploration does not depend on its answer — a leaf or otherwise independent decision.

If a critical decision is **structural** — its answer changes *which branches even exist* — you cannot
defer it, because there is nothing to explore underneath it until it is answered. Take the recommended
answer to keep momentum, but flag it clearly: *"auto-taken to proceed; overriding this in the final
session invalidates the downstream decisions below it."* The final report must show, for each deferred
critical decision, exactly which auto-taken decisions get invalidated if the user overrides it. Without
that traceability, `batch` is false autonomy — the user can't safely override anything.

## Decision ledger

Maintain a running ledger throughout, one row per decision. It feeds the ADRs and the final report.

| # | Decision | Options considered | Chosen (=recommended) | Rationale | Tier (auto/critical) | Structural? | Depends on # | Invalidates if overridden | Status |

`Tier` is the classifier result. `Structural?` and `Depends on #`/`Invalidates if overridden` drive the
dependency handling above. `Status` is one of: applied (auto) / queued (critical) / confirmed / overridden.

## Final report

`batch` and `auto` end with the report structured in `references/report-template.md`. In short: the
grilled plan, the ledger table, "Needs your call" (queued criticals with recommendations and blast
radius — interactive in `batch`, read-only in `auto`), open questions you genuinely couldn't resolve
from code/docs/best-practice, the completeness-critic coverage note, and the artifacts written (ADR
paths + glossary entries from `/domain-modeling`).

## Reference files

- `references/classifier.md` — how to sort every open choice into auto-decide / queue-as-critical /
  hard-stop, with concrete criteria and examples. Read before running any mode.
- `references/personas.md` — the challenger personas, their lenses, and the prompts that spawn them.
- `references/workflow.md` — the Workflow script sketch that backs `batch`/`auto` and enforces the
  anti-collapse protocol. Adapt and run it.
- `references/report-template.md` — the exact structure of the final report.
