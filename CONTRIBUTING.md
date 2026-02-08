# Contributing to XEarthLayer

Thanks for your interest in contributing to XEarthLayer! This guide covers everything you need to get started.

## Getting Started

### Prerequisites

- **Rust** (stable, latest) via [rustup](https://rustup.rs/)
- **Linux** with FUSE3 support (`libfuse3-dev` on Debian/Ubuntu, `fuse3` on Arch)
- **X-Plane 12** (for integration testing)

### Setup

```bash
git clone https://github.com/samsoir/xearthlayer.git
cd xearthlayer
make init    # Installs rustfmt + clippy components
make build   # Debug build
make test    # Run all tests
```

## Development Workflow

### Branching

- `main` is the stable branch. All changes go through pull requests.
- Branch naming: `feature/<name>`, `bugfix/<issue>-<description>`, `chore/<description>`

### Before Submitting a PR

**Always run pre-commit checks:**

```bash
make pre-commit   # fmt + clippy + tests (required)
```

This runs:
1. `cargo fmt` — code formatting
2. `cargo clippy -- -D warnings` — lint with warnings as errors
3. `cargo test` — full test suite with strict mode

CI will reject PRs that haven't passed these checks.

### Writing Code

XEarthLayer follows **SOLID principles** and **Test-Driven Development (TDD)**:

- **Write tests first** — new features and bug fixes should have tests before implementation
- **Use traits for abstraction** — dependency injection over concrete types
- **Keep it testable** — every component should work in isolation with mocks
- **Target 80%+ test coverage** (90%+ preferred)

Refer to the [developer documentation](docs/dev/) for architecture details and design decisions.

### Commit Messages

Follow the conventional commit format:

```
type(scope): description (#issue)
```

**Types:** `feat`, `fix`, `chore`, `docs`, `refactor`, `test`, `perf`

**Examples:**
```
feat(prefetch): add cruise strategy with track-based bands (#32)
fix(cache): restructure disk cache into region subdirectories (#46)
chore(logs): demote per-request log messages from INFO to DEBUG
```

### Pull Requests

- Keep PRs focused — one logical change per PR
- Include a clear description of what changed and why
- Reference related issues with `Fixes #N` or `Related: #N`
- Add a test plan section describing how to verify the change
- Respond to review feedback constructively

## What to Work On

- Check [open issues](https://github.com/samsoir/xearthlayer/issues) for bugs and feature requests
- Issues labeled `good first issue` are suitable for newcomers
- If you want to work on something, comment on the issue first to avoid duplicate effort

## Reporting Bugs

Use the [bug report template](https://github.com/samsoir/xearthlayer/issues/new?template=bug_report.yml). Include:

- Output of `xearthlayer diagnostics`
- Steps to reproduce
- Expected vs actual behavior
- Relevant log output

## Code of Conduct

All contributors are expected to follow our [Code of Conduct](CODE_OF_CONDUCT.md). Be respectful, constructive, and welcoming.

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
