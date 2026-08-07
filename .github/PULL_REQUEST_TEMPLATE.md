## What

<!-- What does this PR change? One or two sentences. -->

## Why

<!-- Why is the change needed? Link issues: Closes #123 -->

## Checklist

- [ ] Title follows Conventional Commits (`feat(scope): ...`)
- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes (new behavior has table-driven tests)
- [ ] Public items are documented (`missing_docs` is deny)
- [ ] Docs updated when behavior changed (`docs/`, `README.md`)
