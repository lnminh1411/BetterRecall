# Contributing to BetterRecall

Thanks for helping. BetterRecall is published by its contributors under AGPL-3.0-only (`LICENSING.md`); contributions are accepted under the same licence.

## Issues

- Bugs and feature ideas: use the issue forms. Search first; add to an existing issue rather than opening a duplicate.
- Security problems: a private advisory (`SECURITY.md`), never a public issue.
- Never paste captured text, screenshots of your history or keys.

## Code

1. Build and test as the README describes: `python Scripts/dev_build.py --test`, then `cargo clippy --workspace --all-targets -- -D warnings` in `rust\`.
2. Every source file starts with `SPDX-License-Identifier: AGPL-3.0-only` and `Copyright (c) BetterRecall contributors`.
3. Keep the privacy rules: no new network use without a setting that is off by default, nothing captured leaves the PC, tests never reach a real network endpoint.
4. One topic per pull request, with tests for the change. Describe what you tested by hand.
5. Commit messages: a short summary line (`area: what changed`), then why.

## Docs

User-facing changes update `README.md` or `docs\user-guide.md`. Design notes live in `docs\` and are listed in `docs\documentations_summary.md`.
