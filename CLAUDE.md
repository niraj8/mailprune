# mailprune

## Agent skills

### Issue tracker

Issues live as GitHub issues in `niraj8/mailprune`, driven by the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage roles, each label string equal to its name (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context — one `CONTEXT.md` and one `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## Releasing

Scripted, never by hand: `make release VERSION=X.Y.Z` (bump, test, tag, push), then
`make publish-tap` once the workflow has uploaded the tarballs. Version lives only in
`Cargo.toml`; the scripts derive the tag, the lock entry and the formula from it.
