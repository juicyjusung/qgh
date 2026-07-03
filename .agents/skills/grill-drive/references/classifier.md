# Critical-decision classifier

Classify every open choice *before* you act on it. The classification decides whether you take the
recommendation and move on, queue it for the human, or stop outright. This is the generalized form of
the D0–D3 decision classes from the precursor `issue-grill-with-docs` skill, stripped of any
GitHub-issue/qgh specifics and given concrete criteria you can apply to any plan.

The point of classifying first is that the failure mode of an autonomous grill is deciding things it had
no business deciding alone — quietly, coherently, and wrongly. A cheap misclassification the other way
(queueing something trivial) only costs the user a few seconds at the end. So when genuinely torn between
auto and critical, lean critical.

## Tier 1 — AUTO-DECIDE (take the recommendation, do not ask)

Take the recommended answer, record it in the ledger with tier `auto`, and keep grilling. A choice is
auto-decidable when **any** of these holds:

- **Naming / formatting / internal structure** — but grep for existing precedent *first* and follow the
  convention you find. A local helper name, test placement, file layout, wording. (Do not invent a
  "best practice" when the repo already has a pattern — matching the surrounding code beats an
  abstractly nicer choice.)
- **Cheap to reverse** — if changing your mind later is a small, local edit, decide now and move on.
- **A clear best-practice default exists** — one obviously-correct answer that a competent reviewer
  wouldn't argue with.
- **Answerable from the codebase or docs** — then it is not a question for the human at all. Go read the
  code, the docs, the ADRs. This inherits grilling's "explore the codebase instead" rule, and you should
  make it *stronger* in `batch`/`auto`: with no human to ask, exploration is your primary way to resolve
  a choice correctly instead of guessing.

Combines with **breadth-before-collapse**: even when a node's root looks auto-decidable, enumerate the
questions an interactive grill would ask *under* it before you collapse it. Auto-deciding a root must not
silently auto-decide a whole subtree you never looked at.

## Tier 2 — QUEUE-AS-CRITICAL (checkpoint: stop and ask; batch/auto: queue for the end)

Queue the decision (tier `critical`) with options, your recommendation, and the blast radius. A choice is
critical when **any** of these holds:

- **Hard or expensive to reverse** — schema migrations, public API shape, data model, wire formats,
  persisted contracts. The cost of being wrong is a migration, not an edit.
- **High blast radius** — cross-cutting, touches many modules, or sets a pattern others will copy.
- **Genuinely ambiguous** — multiple viable options, no clear winner, **and** the choice materially
  changes the outcome. All three parts matter: if there's a clear winner it's auto; if the choice doesn't
  change the outcome it's auto even when ambiguous.
- **Low-confidence recommendation** — you can produce a recommendation but you don't trust it. Say so and
  queue it rather than laundering a guess into a silent decision.
- **Needs judgment code can't supply** — product, UX, business, security posture, or cost trade-offs. The
  answer lives in someone's head or priorities, not in the repo.

In `batch`/`auto`, before queueing, check whether the decision is **structural** (see
`../SKILL.md` → Dependency handling). A structural critical decision can't just be parked — you take the
recommendation to keep momentum and record what it invalidates if overridden.

## Tier 3 — HARD-STOP (stop even in auto)

Stop when continuing would violate a safety, scope, or irreversibility guardrail — the kind of thing that
shouldn't proceed on a recommendation at all, regardless of mode. Examples of guardrails, adapt to the
project:

- broadening scope past what the plan/PRD authorizes
- an action that destroys or exfiltrates data, or persists secrets/tokens/private content
- contradicting an existing ADR without a new ADR to supersede it
- weakening a stated safety, privacy, or security contract
- anything the user has explicitly fenced off

On a hard-stop, emit a concise blocker note and the single smallest question needed to unblock, then
stop — even in `auto`. `auto` promises "no interactive stops for *decisions*," not "run through a
guardrail." A guardrail breach is the one thing that ends an `auto` run early.

## Quick reference

| Signal | Tier |
|--------|------|
| Naming / format / internal, precedent exists | auto |
| Cheap to reverse | auto |
| Clear best-practice default | auto |
| Answerable from code/docs | auto (go read) |
| Hard/expensive to reverse (schema, public API, data model) | critical |
| High blast radius | critical |
| Ambiguous **and** outcome-changing, no clear winner | critical |
| Low-confidence recommendation | critical |
| Needs product/UX/business/security/cost judgment | critical |
| Would breach a safety/scope/irreversibility guardrail | hard-stop |
