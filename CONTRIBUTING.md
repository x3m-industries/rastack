# Contributing to rastack

Thanks for helping build the stack. This document is short on purpose —
the conventions below are enforced by tooling, so once your setup is done
the repo keeps you honest.

## One-time setup

```sh
git clone git@github.com:x3m-industries/rastack.git
cd rastack
cargo test --workspace

# optional: use the commit message template
git config commit.template .gitmessage
```

That's it. The Rust toolchain is pinned in `rust-toolchain.toml` (rustup
installs it automatically), and the first `cargo test` also installs the
repo git hooks into `.git/hooks` via [cargo-husky](https://github.com/rhysd/cargo-husky):
fmt + clippy on commit, the full test suite on push, and a
conventional-commit title check. Hook sources live in `.cargo-husky/hooks/`
— edit there, and they reinstall on the next `cargo test`.

## Commit messages: Conventional Commits

Every commit title follows [Conventional Commits](https://www.conventionalcommits.org):

```
<type>(<scope>): <summary>      # <= 72 chars

feat(axum): deliver router via add_routes
fix(stack): abort timed-out shutdown hooks
docs: explain the strict provider order
refactor(config)!: rename layer precedence   # ! = breaking change
```

Types: `feat` `fix` `docs` `style` `refactor` `perf` `test` `build` `ci`
`chore` `revert`. Scopes are lowercase (`stack`, `provider`, `config`,
`app`, `axum`, `tracing`, `docs`, `ci`). The `commit-msg` hook rejects
titles that don't match.

## Quality gates

The same gates run in the hooks and in CI — green locally means green in CI:

| Gate | Command |
|---|---|
| Formatting | `cargo fmt --all --check` |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` (pedantic; `unsafe_code` and `missing_docs` are **deny**) |
| Tests | `cargo test --workspace` |

`--no-verify` skips hooks in an emergency; CI will still catch it.

## Code conventions

- **The strict order** in every app and example: create the stack →
  per provider (create, add) in dependency order → use handles → run.
  See [docs/STACK.md](docs/STACK.md).
- **Providers**: ctors return `Arc<Self>`; configs are serde structs with
  `Default` (layered loading comes free via `LoadConfig`); every config
  property is documented with its env/file/cli keys.
  See [docs/PROVIDER.md](docs/PROVIDER.md).
- **Tests are table-driven**: `cases: &[(name, input..., expected)]` loops
  with the case name in every assertion. Critical paths get adversarial
  coverage (see `packages/rastack/tests/abuse.rs`).
- **Errors are typed** (`thiserror`), never matched by message string.

## Pull requests

- Keep PRs focused; one concern per PR.
- New behavior ships with tests; changed behavior updates docs.
- CI must be green; a code owner reviews every PR (see
  `.github/CODEOWNERS`).

## Reporting security issues

Please do not open public issues for vulnerabilities — see
[SECURITY.md](SECURITY.md).
