# The batch / auto Workflow

`batch` and `auto` are backed by a Workflow so the anti-collapse protocol is enforced by code instead of
by a model's discipline. The value a workflow adds here is structural: independent agents can't soften
their own attacks, can't grade their own done-ness, and can't bail out early — the loop counter decides
when it's done, not a model's feeling that the plan "looks ready."

The script below is a **sketch to adapt**, not a fixed artifact to run verbatim. Tune the personas,
schemas, and scaling to the plan in front of you. What must survive any adaptation are the five
invariants, each of which maps to an anti-collapse rule:

1. Persona challengers run **in `parallel()` and independently** — no shared draft (rule 1).
2. The **generator** agents (challengers) are distinct from the **judge** pass (resolver). A challenger
   never answers its own challenge (rule 2).
3. The loop runs **until the dry counter is hit** (default 2 consecutive dry rounds), counted in code
   after dedup — not until a model says stop (rule 3).
4. Dedup happens **in plain JS** against the running `seen` set, so "dry" is objective (rule 3).
5. A **completeness-critic** pass runs before returning; anything fresh it finds gets resolved too
   (rule 5).

Breadth-before-collapse (rule 4) lives inside the resolver's prompt: before it marks a node auto, it
enumerates the sub-questions an interactive grill would ask under it.

## Cost profile (Lean default)

A naive version of this loop burns tokens badly: six personas every round, a fresh resolver agent per
challenge, and a full plan-rewrite every round. None of that buys anti-collapse depth — it just
multiplies cost. The Lean default keeps the breadth (independent adversaries + loop-until-dry) but slashes
the per-unit cost:

- **Scale personas to the plan.** Start with 3 core lenses (skeptic, ops, weird-user) — the ones that
  most reliably surface branches a happy-path model skips. Add the other three (security, maintainer,
  product) only for a complex plan or when `--deep` is set. A three-line plan does not need six agents.
- **One batched resolver call per round**, not one agent per challenge. Generation is still separate from
  judgment (the resolver is a distinct pass over the challengers' output) — it just judges the whole
  round's fresh challenges in a single call instead of fanning out N agents. This is the single biggest
  saving.
- **Synthesize the plan once, at the very end** — not every round. Between rounds, challengers attack the
  original plan plus the running decision ledger, which carries the same information without paying for a
  full rewrite each round.
- **Mechanical stages run at low effort.** Challenge-generation and classification are not deep reasoning;
  run them at `effort: 'low'`. Reserve normal/high effort for the completeness-critic and final synthesis.
- **Cap the rounds and honor the budget.** Round cap of 3 by default (loop-until-dry exits earlier when it
  goes quiet). If a token budget is set, scale persona count and the cap to what's left and stop when it
  runs low.

`--deep` (see SKILL.md invocation) restores the thorough profile: all six personas, a higher round cap,
and normal effort throughout. Reach for it when the plan is high-stakes enough to justify the spend.

## What the workflow does and does not do

- It does the **divergent + classify** work: expand the tree adversarially, classify each open choice,
  recommend answers for auto choices, queue criticals, and record dependency/structural metadata.
- It does **not** write artifacts in parallel. ADRs and `CONTEXT.md` edits (via `/domain-modeling`) and
  the final consolidated session happen in the **driving session** after the workflow returns its
  structured result — writing files from parallel agents invites conflicts, and the human-facing
  stop/no-stop behavior isn't the workflow's job.
- `batch` vs `auto` is **not** a difference in the script. The script runs to completion either way. The
  difference is what the driving model does with the returned report: `batch` opens the consolidated
  "Needs your call" session; `auto` prints it read-only and ends.

## Script sketch (Lean)

```javascript
export const meta = {
  name: 'grill-drive-batch',
  description: 'Autonomous anti-collapse grill: adversarial persona rounds until dry, classify and resolve, queue criticals',
  phases: [
    { title: 'Challenge' },     // parallel persona challengers (generation only), low effort
    { title: 'Resolve' },       // ONE batched judge pass per round: classify + recommend, low effort
    { title: 'Completeness' },  // last-chance divergence before returning
    { title: 'Synthesize' },    // fold decisions into the final plan, ONCE
  ],
}

// args: { plan, ledger?, complexity? ('small'|'medium'|'complex'), deep? }
let plan = args.plan
let ledger = args.ledger ?? []
const queued = []
const seen = []          // challenge fingerprints already raised
let dry = 0
let round = 0

// --- persona scaling ---------------------------------------------------------
const CORE = [
  { key: 'skeptic',    stance: 'Attack the core premise; propose the smallest thing that could work.' },
  { key: 'ops',        stance: 'You own this at 3am. Probe failure modes, blast radius, rollback, observability.' },
  { key: 'weird-user', stance: 'Use it as nobody designed for: out of order, twice, empty/huge/unicode/deleted, offline.' },
]
const EXTRA = [
  { key: 'security',   stance: 'Assume hostile inputs and wrong trust boundaries. Probe authz, injection, data exposure, secrets.' },
  { key: 'maintainer', stance: 'You inherit this in a year with no context. Probe reversibility, coupling, migration paths.' },
  { key: 'product',    stance: 'Guard scope and user value. Probe scope creep, value gaps, cheapest shippable version.' },
]
const deep = args.deep === true
const complexity = args.complexity ?? 'medium'
let personas = CORE
if (deep || complexity === 'complex') personas = [...CORE, ...EXTRA]
else if (complexity === 'medium')     personas = [...CORE, EXTRA[0]]   // + security
if (budget.total && budget.remaining() < 120_000) personas = CORE      // trim under budget pressure

const ROUND_CAP = deep ? 8 : 3
const DRY_TARGET = 2
// -----------------------------------------------------------------------------

const CHALLENGE_ITEM = { type: 'object', required: ['concern', 'branch'], properties: {
  concern: { type: 'string' },   // the assumption/gap/scenario attacked
  branch:  { type: 'string' },   // the new question/branch it opens
} }
const CHALLENGES = { type: 'object', required: ['challenges'],
  properties: { challenges: { type: 'array', items: CHALLENGE_ITEM } } }

const RESOLUTION = { type: 'object', required: ['decision', 'tier'], properties: {
  decision:   { type: 'string' },
  options:    { type: 'array', items: { type: 'string' } },
  chosen:     { type: 'string' },             // = recommended answer (for auto)
  rationale:  { type: 'string' },
  tier:       { type: 'string', enum: ['auto', 'critical', 'hard-stop'] },
  structural: { type: 'boolean' },            // does its answer change which branches exist?
  dependsOn:  { type: 'array', items: { type: 'integer' } },
  invalidates:{ type: 'array', items: { type: 'integer' } },  // ledger rows voided if overridden
  confidence: { type: 'string', enum: ['low', 'medium', 'high'] },
} }
const RESOLUTION_BATCH = { type: 'object', required: ['resolutions'],
  properties: { resolutions: { type: 'array', items: RESOLUTION } } }

const CRITIQUE = { type: 'object', required: ['gaps'], properties: { gaps: { type: 'array', items: {
  type: 'object', required: ['concern', 'branch', 'whySkippedWasUnsafe'], properties: {
    concern: { type: 'string' }, branch: { type: 'string' }, whySkippedWasUnsafe: { type: 'string' },
} } } } }

const fp = c => (c.concern + '|' + c.branch).toLowerCase().replace(/\s+/g, ' ').trim()

// one batched judge pass over a list of fresh challenges (generation stays separate from judgment)
const resolveBatch = async (items, label, phaseName) => {
  const out = await agent(
    `Resolve each open choice below for the plan. Classify each with references/classifier.md ` +
    `(auto / critical / hard-stop). Before marking one auto, enumerate the sub-questions an interactive ` +
    `grill would ask under it and confirm none is itself critical (breadth-before-collapse). If the ` +
    `codebase or docs can answer it, say so and answer from them — don't guess. Mark structural=true when ` +
    `an answer changes which branches exist.\n\nPLAN:\n${plan}\n\nDECISIONS SO FAR:\n${JSON.stringify(ledger)}` +
    `\n\nOPEN CHOICES:\n${items.map((c, i) => `${i + 1}. ${c.concern} -> ${c.branch}`).join('\n')}`,
    { label, phase: phaseName, schema: RESOLUTION_BATCH, effort: 'low' }
  )
  for (const r of (out?.resolutions ?? [])) {
    ledger.push(r)
    if (r.tier === 'critical') queued.push(r)
    // hard-stop: flagged in the ledger; the driving session surfaces the blocker.
  }
}

while (round < ROUND_CAP) {
  round++
  if (budget.total && budget.remaining() < 40_000) { log(`budget low, stopping after round ${round - 1}`); break }

  phase('Challenge')
  // Rule 1: independent parallel challengers. Rule 2: they ONLY raise, never answer. Low effort.
  const raised = (await parallel(personas.map(p => () =>
    agent(
      `You are the ${p.key} reviewing this plan. ${p.stance}\n\n` +
      `PLAN:\n${plan}\n\nDECISIONS SO FAR:\n${JSON.stringify(ledger)}\n\n` +
      `ALREADY RAISED (do not repeat):\n${seen.join('\n')}\n\n` +
      `Attack from your lens. Surface false assumptions, unaccounted requirements, and breaking scenarios. ` +
      `For each, give the concern and the new branch/question it opens. Do NOT answer your own challenges.`,
      { label: `challenge:${p.key}`, phase: 'Challenge', schema: CHALLENGES, effort: 'low' }
    )
  ))).filter(Boolean).flatMap(r => r.challenges)

  // Rule 4 (dedup): objective "dry" via the seen set, in plain code.
  const fresh = raised.filter(c => !seen.includes(fp(c)))
  if (fresh.length === 0) {
    dry++
    log(`round ${round}: dry (${dry}/${DRY_TARGET})`)
    if (dry >= DRY_TARGET) break
    continue
  }
  dry = 0
  fresh.forEach(c => seen.push(fp(c)))

  phase('Resolve')
  await resolveBatch(fresh, `resolve:r${round}`, 'Resolve')   // ONE call, not per-challenge
  // No per-round synthesis: next round's challengers attack `plan` + the updated `ledger`.
}

// Rule 5: completeness-critic once, at higher effort (inherits session effort; don't force 'low' here).
phase('Completeness')
const critique = await agent(
  `Would an interactive grill have asked more about this plan than the decisions below capture? ` +
  `What was skipped, and exactly why was skipping it safe (or not)?\n\nPLAN:\n${plan}\n\n` +
  `DECISIONS:\n${JSON.stringify(ledger)}\n\nRAISED:\n${seen.join('\n')}`,
  { label: 'completeness-critic', phase: 'Completeness', schema: CRITIQUE }
)
const gaps = (critique?.gaps ?? []).filter(c => !seen.includes(fp(c)))
if (gaps.length) { gaps.forEach(g => seen.push(fp(g))); await resolveBatch(gaps, 'resolve:completeness', 'Completeness') }

// Synthesize the final plan ONCE, incorporating auto-decisions; leave criticals as open placeholders.
phase('Synthesize')
plan = await agent(
  `Rewrite the plan to incorporate the auto-tier decisions in the ledger. Leave critical decisions as ` +
  `open placeholders (do not silently pick one). Keep it tight.\n\nPLAN:\n${plan}\n\nLEDGER:\n${JSON.stringify(ledger)}`,
  { label: 'synthesize', phase: 'Synthesize' }
)

return { plan, ledger, queued, seenCount: seen.length, rounds: round }
```

## After the workflow returns

The driving session takes `{ plan, ledger, queued }` and:

1. Writes ADRs and glossary entries via `/domain-modeling` for the decisions that warrant them (hard to
   reverse + surprising-without-context + a real trade-off). Serialized, in the driving session.
2. Assembles the final report per `report-template.md`, including the invalidation map built from each
   critical decision's `invalidates` list.
3. `batch`: opens the consolidated "Needs your call" session over `queued`; on any override, re-runs the
   workflow scoped to just the affected branches (pass the reduced plan + surviving ledger as `args`).
   `auto`: prints the report read-only and ends.

## If you are not backing it with a workflow

If a run can't use the Workflow tool, preserve the same invariants with the Agent tool: spawn the persona
challengers as parallel independent subagents each round, dedup their output yourself, resolve them in a
single separate reasoning pass, and hold the dry-rounds counter explicitly in your notes. It's a softer
guarantee — you are now the loop counter — so lean on the counter and resist the urge to stop early. Keep
the same Lean cost discipline: 3 core personas unless the plan is complex, one resolve pass per round, and
synthesize once at the end.
