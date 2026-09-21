# Contributing

Start with the [architecture](ARCHITECTURE.md) and [development setup](docs/development.md). AGENTS.md applies to maintainers and automated contributors alike; the README remains the entry point for users.

## Propose a focused change

For a bug, identify the observed behavior, affected transport, exact commit and a minimal reproduction. For performance, record source/negotiated resolution and FPS, encoder, network and receiver model/firmware. Separate sender pipeline timing from end-to-end latency. Do not compare different quality settings and call the result an equivalent-quality speedup.

Use a topic branch, preserve unrelated modifications and add a regression test that fails before the fix. Document external API decisions with an official reference and the dependency version actually tested. Community traces and issue reports are useful hypotheses, not universal protocol guarantees. Do not rewrite licensed vendored code or remove fixture data as generic cleanup.

Run targeted tests first, then the [required gates](docs/testing.md). Include the commands and their outcomes in the pull request. Record unavailable hardware, missing plugins and ignored tests explicitly. A sanitizer, Clippy or a lexical search is not proof that all dead code or security defects are absent.

## Translations and documentation

UI text uses English message IDs. Run `python3 po/extract.py`, update the appropriate PO catalogue and validate it with msgfmt. Explain new concepts in simple language and preserve placeholders and plurals. Review PT-BR as Brazilian Portuguese, not a copy of European Portuguese; do not equate simplified and traditional Chinese. Only claim review of the languages actually reviewed.

Update the closest maintained guide when behavior changes. Keep the root small: README, AGENTS, architecture, contribution/security/support policy and build manifests. Put reproducible tools under scripts and historical reports under docs/history or audit. Never check in personal paths, tokens, screenshots of private content, caches or built binaries.

## Pull request evidence

Describe the problem, minimal fix, risks and rollback, tests run, and any remaining hardware checks. Commit messages should identify the subsystem and reason. Do not create tags, publish packages or promise stable status as a side effect of a code review. Follow [releasing](docs/releasing.md) for maintainer approval.
