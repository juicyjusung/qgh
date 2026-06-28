# Issue tracker: GitHub

Issues and PRDs for this repo live as GitHub issues. Use the `gh` CLI for
all operations.

Infer the repo from `git remote -v`; `gh` does this automatically when
run inside this clone.

## Conventions

- **Create an issue**: `gh issue create --title "..." --body "..."`. Use
  a heredoc for multi-line bodies.
- **Read an issue**: `gh issue view <number> --comments`, filtering
  comments with `jq` when useful and also fetching labels.
- **List issues**: `gh issue list --state open --json number,title,body,labels,comments`
  with appropriate `--label` and `--state` filters.
- **Comment on an issue**: `gh issue comment <number> --body "..."`
- **Apply or remove labels**: `gh issue edit <number> --add-label "..."`
  or `gh issue edit <number> --remove-label "..."`
- **Close an issue**: `gh issue close <number> --comment "..."`

## Pull requests as a triage surface

**PRs as a request surface: no.**

External PRs are not pulled into the `/triage` queue for this repo.
Collaborator PRs should stay in the normal code-review workflow.

If this is later changed to `yes`, PRs run through the same labels and
states as issues using `gh pr` equivalents. GitHub shares one number
space across issues and PRs, so a bare `#42` may be either; resolve with
`gh pr view 42` and fall back to `gh issue view 42`.

## Local tracker files

`.scratch/` is not a tracker SSOT for this repo. Do not publish tracker
updates there unless the user explicitly asks for local markdown.

The old qgh MVP local tracker files were migrated to GitHub Issues on
2026-06-28 KST and removed from the workspace:

- PRD parent: #2
- MVP implementation slices: #3 through #17

## When a skill says "publish to the issue tracker"

Create a GitHub issue with `gh issue create`.

## When a skill says "fetch the relevant ticket"

Run `gh issue view <number> --comments`.
