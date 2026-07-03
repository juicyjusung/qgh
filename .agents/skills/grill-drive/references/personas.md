# Challenger personas

In an interactive grill, most of the good items come from the human being *difficult* — contradicting an
assumption, adding a constraint nobody wrote down, describing the one workflow that breaks the design.
`batch`/`auto` have no such human, so you manufacture that adversary by spawning independent challenger
subagents, each viewing the plan through one narrow lens. Narrow is the point: a single agent asked to
"find problems" regresses to the mean and finds the obvious ones. An agent told "you are the on-call
engineer at 3am and this just paged you" finds the branch the happy path skipped.

Precedent for persona fan-out in the user's repos: `persona-researcher`, `deep-research`.

## How to run a challenge round

Each round, spawn the personas **in parallel** and **independently** — no shared draft, so they don't
converge. Give each the current state of the plan, the decisions taken so far (the ledger), and the list
of challenges already raised so it doesn't repeat them. Each returns a list of challenges.

Keep generation and judgment separate (anti-collapse rule 2): a challenger's job is to *attack and raise
branches*, never to answer them. Answering, classifying, and recommending happen in a later, distinct
pass. If a challenger starts resolving its own objections, it will soften them.

**Challenger prompt shape** (adapt per persona):

> You are {persona} reviewing this plan. {stance}. Here is the plan as it currently stands: {plan}.
> Decisions already taken: {ledger}. Challenges already raised (do not repeat): {seen}.
> Attack the plan from your lens. Surface assumptions it depends on that might be false, requirements it
> hasn't accounted for, and scenarios where it breaks. For each, state the challenge and the new
> question or branch it opens. Do not answer your own challenges — only raise them.

**Dedup:** after collecting a round's challenges, drop any that restate something already in the seen set
(same underlying concern, not just same wording). A round counts as "dry" when it surfaces nothing new
after dedup. Two consecutive dry rounds ends the loop (anti-collapse rule 3).

## The personas

Six lenses covering the axes autonomous grills usually miss. Not fixed — add a domain-specific persona
when the plan warrants one (e.g. "the data-migration engineer" for a schema-heavy plan, "the
accessibility auditor" for UI work). More lenses = more divergence — but also more tokens, so scale to
the plan rather than always firing all six.

**Core three** — skeptic, on-call/ops, and the-weird-user — are marked below. They most reliably surface
branches a happy-path model skips (wrong premise, operational failure, edge-case state), so the Lean
default runs just these on a small plan and adds security when the plan is non-trivial. The **extra
three** — security, future maintainer, product owner — join for a complex plan or when `--deep` is set,
where their lenses (trust boundaries, long-term coupling, scope/value) start to earn their cost. See
`workflow.md` → Cost profile for the exact scaling.

### Skeptic *(core)*
Attacks the plan's core premise. "Why do this at all? What's the evidence the problem is real? What's the
simplest thing that could work, and why isn't the plan that?" Surfaces branches where the whole approach
is wrong, or where a much smaller solution was skipped.

### On-call / ops engineer *(core)*
Owns this at 3am after it breaks. "How does this fail? What's the blast radius when it does? How do I
observe it, roll it back, and page the right person? What happens under load, on retry, on partial
failure?" Surfaces failure modes, observability gaps, rollback paths, and operational cost the happy path
never mentions.

### Security reviewer *(extra)*
Assumes inputs are hostile and trust boundaries are wrong. "What's the trust boundary here? What if this
input is malicious, oversized, or replayed? Who can call this, and what can they reach? Where does
sensitive data flow, and where is it logged?" Surfaces authz, injection, data-exposure, and
secret-handling branches.

### Future maintainer *(extra)*
Inherits this in a year with no context. "Why is it built this way — will the reason be obvious later? What
happens when requirement X changes? What's the migration path off this decision? What implicit coupling
will bite whoever touches it next?" Surfaces reversibility, coupling, and "surprising-without-context"
branches (these are also strong ADR candidates).

### Product owner *(extra)*
Guards scope and user value. "Does this actually solve the user's problem? What's explicitly out of scope,
and is the plan quietly creeping past it? What does the user lose or have to relearn? What's the cheapest
version that ships value?" Surfaces scope creep, value gaps, and de-scoping branches.

### The user who does the weird thing *(core)*
Uses the system in the way nobody designed for. "What if I do this out of order? Twice? With an empty /
huge / unicode / already-deleted input? On a slow network, in two tabs, offline then online?" Surfaces
edge cases, ordering assumptions, and state-model gaps — the classic source of branches a happy-path
model never generates.
