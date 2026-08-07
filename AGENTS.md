# rastack — agent instructions

rastack is a provider-based application stack for Rust services: a `Stack`
owns providers (axum server, tracing, app identity, ...), drives their
lifecycle (`ctor → init → run → shutdown → close`), and tears down in
reverse order on signals, completion, or failure.

## Commands

```sh
cargo test --workspace                          # all tests (unit + integration)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all                                 # format (check in CI/hooks)
cargo run --example app                         # run the demo app
```

Toolchain is pinned in `rust-toolchain.toml`. CI runs the same gates
(`.github/workflows/ci.yml`); git hooks in `.githooks/` mirror them.

## Hard rules (enforced by lints — builds fail otherwise)

- clippy `pedantic` is on; fix warnings, don't `#[allow]` them away unless
  there is a genuine reason stated in a comment.
- `unsafe_code = "deny"` — never introduce `unsafe`.
- `missing_docs = "deny"` — every `pub` item gets a doc comment. Config
  struct fields document their purpose AND their layer keys (env / file /
  cli).

## Code conventions

- **The strict order** in every app/example `main`:
  1. create the stack (`Stack::new()` embeds the tracing + app core providers)
  2. per optional provider, in dependency order: create it, `stack.add(...)`
  3. use provider handles (deliver routes, inject into dependent ctors)
  4. `stack.run().await`
  Never chain `add` calls; bind each provider to an explicit variable.
- **Providers**: ctors return `Arc<Self>`; all lifecycle hooks take `&self`
  (interior mutability for runtime state); configs are plain serde structs
  with `Default` — `LoadConfig` gives layered loading (defaults < ENV <
  config file < cli) for free. Field name = flat config key everywhere
  (`app_name` → `APP_NAME` / `app_name:` / `--app-name`).
- **Errors are typed** (`thiserror` enums); tests match error values, never
  message strings.
- Config validation is the stack's job via `Provider::config()` — never
  call `validate()` in a provider.

## Testing conventions

- **Table-driven tests everywhere**: `cases: &[(name, input..., expected)]`
  with the case name in every assertion message.
- Critical paths get adversarial coverage — see
  `packages/rastack/tests/abuse.rs` (panicking hooks, duplicate smuggling,
  state-machine double calls, 1000-provider teardown).
- Process-level behavior is tested with self-exec integration tests
  (`tests/signal.rs`, `tests/config.rs`): `harness = false` binaries that
  re-run themselves as children.

## Git

- Conventional Commits, enforced by `.githooks/commit-msg`:
  `<type>(<scope>): <summary>` — types feat/fix/docs/style/refactor/perf/
  test/build/ci/chore/revert; scopes stack/provider/config/app/axum/
  tracing/docs/ci.
- No AI co-author trailers in commit messages.
- Docs live in `docs/STACK.md` (lifecycle internals) and
  `docs/PROVIDER.md` (provider contract) — update them when behavior
  changes, plus `README.md` for user-facing changes.
