# Final report structure

`batch` and `auto` end with this report. `batch` makes section 3 interactive (you present the queued
decisions and take overrides); `auto` presents the same section read-only and ends. Use this exact
skeleton so the report is scannable and the "what breaks if I override this" traceability is never
dropped — that traceability is what makes autonomous grilling safe to act on.

```markdown
# Grill-drive report: <plan title>  (mode: batch|auto)

## 1. The plan, grilled into shape
<The plan as it now stands after the auto-decisions were folded in. Tight, current, and readable on its
own — someone who never saw the original should understand what is being built.>

## 2. Decision ledger
| # | Decision | Options considered | Chosen (=recommended) | Rationale | Tier | Structural? | Depends on # | Invalidates if overridden | Status |
|---|----------|--------------------|-----------------------|-----------|------|-------------|--------------|---------------------------|--------|
| 1 | ... | ... | ... | ... | auto | no | — | — | applied |
| 2 | ... | ... | ... | ... | critical | yes | 1 | 4, 5 | queued |
<Status: applied (auto) / queued (critical) / confirmed / overridden.>

## 3. Needs your call
<Every queued critical decision. For each:>
### C1. <decision>
- **Options:** <A / B / C>
- **My recommendation:** <X> — <confidence: low|medium|high>
- **Why:** <short rationale>
- **If you override this:** <what downstream breaks — the exact auto-taken ledger rows invalidated, from
  the decision's `invalidates` list. If structural, say so: overriding re-opens branches N, N+1, ….>

<batch: pause here and take answers; re-grill only the branches under any decision the user overrides.
auto: leave read-only; do not wait.>

## 4. Open questions
<Things you genuinely could not resolve from code, docs, or best practice — not the same as queued
criticals (those have a recommendation). These are the ones where you don't even have a confident
recommendation. If empty, say "none".>

## 5. Coverage note (completeness-critic)
<From the completeness pass: what an interactive grill might have probed that you collapsed, and exactly
why collapsing it was safe. This is the honest "here's what I chose not to chase, and my reasoning" —
it's what lets the user spot a subtree you dismissed too fast.>

## 6. Artifacts written
- ADRs: <paths under docs/adr/, or "none">
- Glossary: <CONTEXT.md terms added/changed, or "none">
- <Rounds run: N. Challenges raised: M.>
```

## Notes on filling it in

- **Section 2 is the spine.** Sections 3–5 are views onto it: section 3 is the `critical` rows, section 4
  is the choices with no confident recommendation, section 5 is what never made it into a row and why.
- **The `Invalidates if overridden` column is not optional.** A `batch` report where overriding a
  decision has unknown downstream effects is false autonomy — the user can't override anything safely.
  Build it from each critical decision's dependency metadata; if a critical decision is structural, its
  invalidation set is "the branches that only exist because of the chosen answer."
- **Keep the plan (section 1) honest about placeholders.** Critical decisions that were auto-taken to keep
  momentum (structural ones) should read as "provisionally X — see C<n>", not as settled fact.
- **ADRs stay sparing.** Only for decisions that are hard to reverse *and* surprising-without-context
  *and* a real trade-off, per `/domain-modeling`. Most ledger rows do not become ADRs.
