## Agent skills

### Issue tracker

Issues live in GitHub Issues (PRs are not a triage surface). See `docs/agents/issue-tracker.md`.

### Triage labels

Default label vocabulary (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context layout — `CONTEXT.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## Formatter

### Rustfmt

You should run `cargo fmt -- --check` before `git commit`.
Our CI (GitHub Actions) run `cargo fmt --check`.
