# Preflight Check Design

**Status**: Implemented
**Created**: 2026-09-07
**Issue**: #265

## Overview

Preflight is the ordered set of prerequisites XEarthLayer verifies before a
command does any work: the configuration exists and parses, packages are
installed, X-Plane's Custom Scenery directory can be found, macFUSE is present,
no other instance is already running.

It is a registry, not a sequence of conditionals. Each prerequisite is a named
value implementing one trait, registered in order, executed before command
dispatch. What it produces is not merely a pass or fail: the checks
progressively fill a **bootstrap context** holding the validated inputs the
command then launches from.

### Problem Statement

Before this framework, seven prerequisites were written inline at the top of
`commands/run.rs`. Three problems followed from that.

**The order was load-bearing and nothing recorded it.** The configuration
upgrade warning has to run after the first-run check, because it reads the
configuration file. The packages check has to run after configuration load,
because it consumes a configuration value. Neither constraint was written down
and no test protected either. The only way to discover one was to break it.

**Placement was wrong, and invisibly so.** `commands/run.rs` and
`commands/setup/wizard.rs` each answered the question "does this installation
exist" independently, both by testing whether the configuration file is
present. A prerequisite living inside `run` therefore governed only `run`. The
setup wizard, reached by a user whose installation was in a state `run`
rejected, would see the same state and conclude the user was new, then offer to
overwrite a working configuration.

**Every new prerequisite was another conditional in the same function.** Three
distinct outcome shapes already existed there, none of them named: act
silently, warn and continue, stop with an error. Two prerequisites that plainly
should have existed did not, because adding one meant growing a function that
was already doing too much. A missing macFUSE kernel extension surfaced as an
opaque mount failure, and nothing at all prevented a second instance from
mounting the same paths.

### Goals

- Prerequisites are declared, named and individually testable.
- Execution order is explicit and asserted, not implicit in the shape of a
  function.
- One place answers "does this installation exist", for every command.
- The whole set can be inspected without side effects, so a diagnostic can
  report what would fail and why.
- Adding a prerequisite is a registration, not a new branch in `run`.

### Non-Goals

- **Not a dependency graph.** No topological sort, no reordering, no declared
  edges between checks. See [Operating Principles](#operating-principles).
- **Not service construction.** Preflight produces validated inputs.
  `service::builder`, `service::runtime_builder` and `ServiceOrchestrator`
  still own construction and consume those inputs.
- **Not a plugin system.** The extension points exist (see
  [Extension Points](#extension-points)) but nothing discovers or loads
  third-party checks today.

## Architecture

### Module Structure

The framework is generic and lives in the library. The concrete checks live in
the CLI, because what counts as a prerequisite is a property of the command
line tool rather than of the engine.

```
xearthlayer/src/preflight/
    check.rs      Preflight<C> trait
    types.rs      Status, Remedy<C>, PreflightError, RunOutcome
    runner.rs     Runner<C>, the execution engine
    context.rs    BootstrapContext

xearthlayer-cli/src/preflight/
    mod.rs        build_registry, enforce, check names, error mapping
    config.rs     FirstRun, LoadConfig, ConfigUpgradeWarning
    paths.rs      PackagesInstalled, ResolveCustomScenery, CustomSceneryExists
    system.rs     LogStartup, FdLimit, AirportIcao, MacFuseAvailable,
                  NoRunningInstance
```

The trait is public and unsealed in the library crate. A trait declared in a
binary crate cannot be implemented from anywhere else, and for the same reason
the framework owns its error type rather than naming `CliError`.

### The Trait

```rust
pub trait Preflight<C>: Send + Sync {
    fn name(&self) -> Cow<'static, str>;
    fn applies(&self, ctx: &C) -> bool { true }
    fn inspect(&self, ctx: &C) -> Status;
    fn remediate(&self, ctx: &mut C) -> Result<Remedy<C>, PreflightError> { ... }
}
```

Generic over the context it operates on. Check families working on different
contexts get their own registries rather than sharing one object every check
must fit. `Cow<'static, str>` for the name rather than `&'static str`, so a
check constructed at runtime can name itself.

### Outcome Types

```rust
pub enum Status {
    Satisfied,
    Warning(String),
    Unsatisfied { reason: String, remediable: bool, hint: Option<String> },
}
```

The three shapes that already existed in `run.rs`, now named. `Warning` reports
and continues. `Unsatisfied` with `remediable: false` stops the run.
`Unsatisfied` with `remediable: true` invites `remediate`.

`Remedy<C>` carries an optional message and a list of further checks to
register. `PreflightError` reports a remediation failure and can carry the
underlying error, so a caller can downcast and rebuild a richer error than a
message string.

### The Bootstrap Context

```rust
pub struct BootstrapContext {
    command: Cow<'static, str>,
    airport: Option<String>,
    config: Option<ConfigFile>,
    install_location: Option<PathBuf>,
    custom_scenery_path: Option<PathBuf>,
}
```

A concrete struct with accessors, not a type-keyed store. That choice is
deliberate: a concrete struct states the pipeline's whole contract in one
definition, the compiler enumerates every reader, and an unfilled field is
greppable. A dynamic bag hides all three.

Every contributed field is `Option` until its check has run. It holds library
types only: a `clap` dependency here would make the context unusable outside
the binary, which is exactly the coupling the generic trait exists to avoid.

## Operating Principles

These are the rules the design rests on. Breaking one does not merely make the
code less tidy, it removes a property something else depends on.

### 1. Registration order is execution order

There is no dependency graph, no reordering and no declared edges. The registry
is a `Vec` walked front to back.

Most checks only contribute, so their order is genuinely free. A few must read
a value an earlier check contributed, and that is a real constraint the
framework does not remove. What it does is make the failure legible: see
principle 4.

The order is asserted by test (`registration_order_reproduces_what_run_did_inline`).
It was load-bearing and undocumented before; now it is load-bearing and stated.

### 2. Inspection is pure

`inspect` must have no side effects. This is the contract the whole design
rests on, because `Runner::report` calls `inspect` and nothing else. A check
that mutates during inspection would change a user's installation as a side
effect of their asking for a diagnostic.

Logging is permitted, since it changes nothing the program depends on.
Anything else is not.

A test registers a check that panics if remediated and runs the registry in
report mode, so the contract is covered rather than merely documented.

### 3. Remediation is verified

After `remediate` returns, the runner calls `inspect` again. Since inspection
is pure, that second read costs nothing.

Two things follow. A remediation that claims success without fixing anything is
caught rather than believed. And a single check can *resolve* a value during
remediation and then *judge* it, which is how `PackagesInstalled` resolves the
install location and then rejects it when the directory does not exist.

One attempt, then a verdict. There is no retry loop, so a check that cannot fix
itself cannot spin.

### 4. Checks never call one another; absence is named

A check reads what it needs from the context and contributes what it owns. It
never invokes another check and declares nothing about one.

When a check reads a value that is absent, it must return
`Status::Unsatisfied` naming that value, never unwrap it:

```rust
fn inspect(&self, ctx: &BootstrapContext) -> Status {
    if ctx.config().is_none() {
        return Status::unsatisfied("configuration has not been loaded");
    }
    ...
}
```

This is what makes principle 1 safe without a dependency graph. A misordered
registry produces `packages-installed: configuration has not been loaded`
rather than a panic three steps downstream, and the message names both the
check and the thing it lacked.

An earlier draft of this design added `requires()` and `provides()`
declarations validated at registry construction. They were dropped: they were a
dependency graph wearing a lighter coat, and they reintroduced exactly the
inter-check coupling the trait exists to avoid. The residual risk, that a
misordering is caught on first execution rather than at construction, is closed
by the completeness test.

### 5. The context carries inputs, not services

Resolved paths, the loaded configuration, detected hardware. Not constructed
services. Preflight establishes preconditions; it does not start XEarthLayer.

The line matters because `service::builder`, `service::runtime_builder` and
`ServiceOrchestrator` already own construction. A context that carried
constructed services would make preflight responsible for startup, and the two
would drift into doing the same job.

### 6. Platform exclusion is runtime, not `#[cfg]`

`MacFuseAvailable` compiles on every platform and is excluded off macOS through
`applies()`:

```rust
fn applies(&self, ctx: &BootstrapContext) -> bool {
    cfg!(target_os = "macos") && ctx.command() == "run"
}
```

`cfg!` is an expression evaluating to a constant. `#[cfg]` is an item attribute
that removes code from compilation entirely, and code removed from compilation
is never type checked on the other platform, which is how a macOS path rots
unnoticed until someone builds on a Mac. Here Linux compiles the check and runs
its tests, and the platform assertion follows the target so the test is
meaningful on both.

Prefer this wherever the cost is a branch rather than a genuinely absent API.

### 7. Prefer injection over ambient state

Every check that touches the environment takes its source as a constructor
parameter: `FirstRun::with_paths`, `LoadConfig::with_path`,
`ResolveCustomScenery::with_detector`, `FdLimit::with_limits`,
`MacFuseAvailable::with_probe`, `NoRunningInstance::with_liveness`.

No test reads a real `$HOME`, probes for a real X-Plane installation, or
depends on which PIDs happen to exist on the machine running it. Two of these
were added specifically to remove races: `FdLimit` reading the real process
limit would race every other test that raises it, and the liveness predicate
would otherwise depend on PID reuse.

## The Registry

`build_registry()` in `xearthlayer-cli/src/preflight/mod.rs`. Order reproduces
what `run.rs` did inline, and the first four entries carry the ordering
constraints described above.

| # | Check | Purpose |
|---|-------|---------|
| 1 | `first-run` | No configuration and no packages means a fresh install |
| 2 | `load-config` | Loads the configuration, contributes it |
| 3 | `log-startup` | Writes the version banner to the log |
| 4 | `config-upgrade` | Warns when settings are missing or deprecated |
| 5 | `packages-installed` | Resolves the install location, requires it exists |
| 6 | `resolve-custom-scenery` | Resolves X-Plane's Custom Scenery path |
| 7 | `custom-scenery-exists` | Requires that path to exist |
| 8 | `airport-icao` | Validates `--airport`, only when one was given |
| 9 | `macfuse-available` | Requires macFUSE, macOS only |
| 10 | `no-running-instance` | Refuses a second concurrent instance |

Two positions are themselves load-bearing.

**`log-startup` is third.** It is an action rather than a test, and it is in the
registry because its position is what matters: after the configuration loads,
and before anything that can reject the run. A failed startup is exactly when a
support log needs to record which build produced it. Moving it behind the
checks that can fail loses the banner from every failed run, which is what
happened during development and was caught only by diffing against a binary
built from the previous commit.

**`no-running-instance` is last.** The lock is claimed only for a run that will
actually proceed. Claiming it earlier would leave a lock file behind after a run
that a later prerequisite rejected.

### Execution

`preflight::enforce` runs before command dispatch in `main()`, so `run` and
`setup` receive the same answer about whether the installation exists.

`configure_allocator()` is the documented exception. It must precede all
allocation and thread creation, so it cannot be a registry entry executed after
argument parsing. It stays as the first statement of `main()`.

## Adding a Check

### 1. Name it

Add a constant to `preflight::names`. Constants rather than literals because
`to_cli_error` dispatches on them, and a duplicate name would silently reroute
one check's error to another. A test asserts they are unique.

### 2. Write the check

```rust
pub struct SomethingRequired {
    probe: PathBuf,
}

impl Preflight<BootstrapContext> for SomethingRequired {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(names::SOMETHING_REQUIRED)
    }

    fn applies(&self, ctx: &BootstrapContext) -> bool {
        ctx.command() == "run"
    }

    fn inspect(&self, ctx: &BootstrapContext) -> Status {
        let Some(config) = ctx.config() else {
            return Status::unsatisfied("configuration has not been loaded");
        };
        if self.probe.exists() {
            Status::Satisfied
        } else {
            Status::unsatisfied(format!("{} is missing", self.probe.display()))
                .with_hint("Run 'xearthlayer init' to create it.")
        }
    }
}
```

Take any environment source as a constructor parameter, per principle 7. If the
check is test-only in its injected form, gate that constructor with
`#[cfg(test)]`: the project lints with warnings denied, so an unused
constructor fails the build.

### 3. Decide whether it remediates

Implement `remediate` only if the check can fix the problem or contribute a
value. Return `Status::unsatisfied(...).remediable()` from `inspect` to invite
it. Remember principle 3: after remediation the check is inspected again and
must then be satisfied, or the run stops.

Return `PreflightError` from `remediate` only for a genuine failure to act. If
the failure has a richer error type the CLI should render, attach it with
`.with_source(e)` and add an arm to `remediation_to_cli_error`.

### 4. Register it

Add it to `build_registry_with`, in the position its ordering constraints
require, and update the expected order in
`registration_order_reproduces_what_run_did_inline`. That test failing is the
intended prompt to think about position.

If the check reads the environment, add its source to `RegistryEnv` rather than
taking it from a default. The struct exists so that a test fabricating an
environment is told by the compiler everything it has to fabricate. A check
excluded on the test machine's platform is the case to watch: it will not run
through the registry there, so assert its satisfied path directly as well.
`the_fabricated_environment_satisfies_the_macos_only_check_too` is that
assertion for macFUSE.

### 5. Map its failure

By default an unsatisfied check becomes `CliError::Config` with the reason and
hint. Add an arm to `to_cli_error` only if it needs a different variant.

### 6. Test it

Against a fabricated context, never a real environment:

- satisfied and unsatisfied paths
- `applies` for the commands it should and should not govern
- if it reads a contributed value, that it **names** the value when absent
  rather than panicking
- if it remediates, that inspection is satisfied afterwards

## Extension Points

### Registrations from a remedy

`Remedy<C>` carries `register: Vec<Box<dyn Preflight<C>>>`. The runner walks the
registry by index rather than by iterator, so entries appended during execution
are picked up at the tail.

Nothing uses this yet. It exists so that a future plugin discovery check can
contribute further checks without the registry knowing plugins exist. Appending
at the tail is what keeps execution a straight line: nothing is ever spliced in
ahead of a check that has not run, and because system checks register first,
anything a discovery check contributes necessarily runs after all of them.

### New context types

`Preflight<C>` is generic and `Runner<C>` is one registry per context type.
A different family of startup work, operating on a different accumulating
context, gets its own `Runner` rather than being forced into
`BootstrapContext`.

### What is deliberately absent

Plugin discovery, dynamic loading, ABI stability, trait versioning, priority
overrides and cross-stage dependencies. Deregistration exists only as removal by
name, for tests.

## Error Mapping

`to_cli_error` dispatches on the check name. This is a deliberate temporary
seam and the one piece of the design that is not principled.

Two errors are not plain messages. `CliError::NeedsSetup` prints a welcome and
**exits 0**, because a fresh installation is not a failure.
`CliError::NoPackages` renders installation guidance built from the resolved
install location. Neither survives a generic "reason plus optional hint"
rendering, and both had to be preserved exactly while the framework was
introduced.

It disappears once those two messages are free to change, at which point every
check maps uniformly.

## Testing Strategy

Three tests carry more weight than the rest.

**Completeness.** `the_bootstrap_pipeline_fills_every_context_field` runs the
whole registry over a fabricated environment and asserts no `BootstrapContext`
field is still `None`. This proves the pipeline actually produces a launchable
context, and it fails if a field is added with no check to fill it. It is what
replaced the `requires`/`provides` declarations described in principle 4.

**Order.** `registration_order_reproduces_what_run_did_inline` pins the
sequence. Adding a check makes it fail, which is the point.

**Applicability.** `setup_does_not_inherit_runs_prerequisites` asserts that no
`run` prerequisite applies to `setup`, since `setup` exists to create the state
those checks require.

### What tests cannot see

The suite has no visibility into terminal output. When changing anything a user
reads, build a binary from the base commit and diff the scenarios directly.
That is what caught the `log-startup` regression, which 66 passing tests did
not.

## Related

- `docs/dev/design-principles.md`: SOLID and TDD expectations
- `docs/dev/job-executor-design.md`: the job and task traits, a comparable
  trait-plus-registry design at a different layer
- `docs/macos.md`: the macFUSE installation and approval flow that
  `macfuse-available` points users at
