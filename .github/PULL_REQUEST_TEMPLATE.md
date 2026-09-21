## Change and reason

Describe the user-visible problem, the smallest fix and affected transport(s).
Link the upstream documentation or reproducible issue used for protocol changes.

## Validation

Record commands, versions, exit status and the exact commit tested. Separate
unit/integration tests from physical receiver tests. List missing dependencies,
ignored tests and untested combinations; attach sanitized logs only.

## Review checklist

- [ ] Regression test added or an explicit reason supplied.
- [ ] Other transports retain their protocol-specific invariants.
- [ ] Shutdown, cancellation and failure paths remain bounded.
- [ ] User text, translations, documentation and package metadata are consistent.
- [ ] No credentials, captured screen content, caches or unrelated changes.
