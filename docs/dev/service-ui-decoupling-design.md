# Service-UI Decoupling Design

## Status

**Draft** — Target release: 0.5.0

Related: #111 (FUSE hook pipeline), #112 (plugin architecture), #113 (release channels)

### Tracking Issues

| Issue | Domain |
|-------|--------|
| #139 | Remove `gpu-encode` feature flag |
| #140 | `xearthlayer-proto` crate |
| #141 | `xearthlayer-api` server adapter |
| #142 | `xearthlayer-daemon` binary |
| #143 | `DaemonClient` wrapper |
| #144 | `xearthlayer-tui` extraction |
| #145 | CLI service commands + cleanup |
| #146 | Multi-package release pipeline |
| #147 | `ServiceManager` trait |

## Problem Statement

XEarthLayer's service layer (FUSE, cache, executor, prefetch) and terminal UI run in the same process with a shared lifecycle. Quitting the TUI stops the service. There is no way to run the service headless, connect a GUI, or interact with a running service from multiple clients.

As we introduce a graphical interface in 0.5.0, we need a clean separation between the service and any UI that controls or observes it.

## Goals

1. The service runs as a standalone daemon process, independent of any UI
2. The daemon is managed by the OS service infrastructure (systemd on Linux, launchd on macOS)
3. A terminal UI and graphical UI can connect to the running daemon simultaneously
4. Both UIs interact with the daemon through a common typed API
5. The daemon can run fully headless with no UI attached
6. Quitting a UI does not stop the service — stopping is an explicit action

## Non-Goals

- macOS launchd integration (deferred to 0.6.x)
- Full runtime reconfiguration via the API (the API accommodates it later, but 0.5.0 scope is operational commands only)
- Windows service support
- GUI packaging (separate design, likely Flatpak/AppImage with bundled dependencies)

## Design Decisions

### Process Topology

The daemon runs as its own OS process. UIs are separate processes that connect over IPC. There is no shared memory, no in-process coupling.

When multiple clients are connected, each operates independently with last-write-wins semantics for commands. No locking or lease mechanism is needed — conflicting commands from two UIs is an edge case that users can self-regulate.

### IPC Transport: gRPC

Communication between daemon and clients uses gRPC via the `tonic` and `prost` crates.

**Why gRPC**:

- Strongly typed contracts via Protocol Buffers eliminate ad-hoc error checking on both sides
- Bidirectional streaming maps naturally to live telemetry delivery
- Code generation keeps server and client in sync at compile time
- Standard tooling (grpcurl, Postman, BloomRPC) can inspect traffic for debugging

**Alternatives considered**:

| Transport | Why Not |
|-----------|---------|
| REST + WebSocket | Loses compile-time type safety, requires manual serialization |
| Unix socket + custom protocol | Requires building framing, routing, and codegen from scratch |
| D-Bus | Poor macOS support, awkward Rust bindings |

### Daemon Lifecycle via OS Service Manager

The daemon is designed to be supervised by systemd (Linux) or launchd (macOS). This provides automatic restart on crash, logging via journald, dependency ordering, and a familiar management interface (`systemctl --user start xearthlayer`).

Clients can also start and stop the daemon programmatically. A `ServiceManager` trait abstracts the platform mechanism:

| Platform | Start | Stop |
|----------|-------|------|
| Linux (systemd) | `systemctl --user start xearthlayer` | Shutdown RPC + `systemctl --user stop` |
| Linux (no systemd) | Spawn `xearthlayer-daemon` detached | Shutdown RPC |
| macOS (launchd) | `launchctl load ...` | Shutdown RPC + `launchctl unload` |
| Fallback | Spawn `xearthlayer-daemon` detached | Shutdown RPC |

Client-initiated lifecycle control is important for the GUI, particularly on macOS where users expect to manage background services from the application rather than from a terminal.

### FUSE Mount Ownership

The daemon process owns FUSE mounts directly. When the daemon shuts down, it unmounts as part of its shutdown sequence. UIs never interact with FUSE.

### Endpoint Discovery

The daemon listens on a Unix domain socket by default:

```
$XDG_RUNTIME_DIR/xearthlayer/daemon.sock
```

Fallback: `~/.xearthlayer/daemon.sock`

A `--listen` flag allows binding to a TCP address for remote access or containerized environments.

On startup, the daemon writes a state file:

```json
{
  "pid": 12345,
  "endpoint": "unix:///run/user/1000/xearthlayer/daemon.sock",
  "started_at": "2026-04-06T14:30:00Z",
  "version": "0.5.0"
}
```

**Location**: `$XDG_RUNTIME_DIR/xearthlayer/daemon.state` (fallback: `~/.xearthlayer/daemon.state`)

Clients discover the daemon by reading this file, validating the PID is alive, and connecting to the endpoint. An environment variable `$XEARTHLAYER_ENDPOINT` overrides discovery for advanced use cases.

### Singleton Enforcement

Only one daemon instance may run at a time. On startup, the daemon checks for an existing state file with a live PID and refuses to start if one is found. The Unix domain socket bind also fails naturally if another process holds it, providing a second line of defense.

## Design

### Crate Topology

The workspace grows from two crates to six (plus `xearthlayer-plugin-sdk` from #112), with strict layered dependencies. No crate depends sideways on a peer.

```
xearthlayer-proto              xearthlayer-plugin-sdk
(gRPC contract)                (plugin ABI contract, from #112)
  ^      ^      ^                    ^
  |      |      |                    |
  |  xearthlayer-api           xearthlayer
  |  (server + client          (service library)
  |   adapters)                     ^
  |      ^      ^                   |
  |      |      +-------------------+
  |      |      |
  |      |      |
  |      |  xearthlayer-daemon
  |      |  (service + gRPC server + plugin host)
  |      |
  +------+-------------+
  |      |             |
xearthlayer-tui  xearthlayer-cli
(TUI client)     (offline CLI +
                  daemon commands)
```

The two SDK crates (`proto` and `plugin-sdk`) are both pure abstractions at the bottom of the graph. Neither depends on the other — they serve different boundaries (IPC and dynamic loading respectively).

#### xearthlayer-proto

Owns `.proto` files and generated Rust code. No business logic, no dependencies on other XEarthLayer crates. This is the pure abstraction at the bottom of the dependency graph — the interface that all parties depend inward on.

#### xearthlayer-api

Two responsibilities, both adapter-shaped:

- **Server side**: Implements the tonic service traits by delegating to `ServiceOrchestrator` methods. Translates between domain types (`TelemetrySnapshot`, `SharedAircraftPosition`) and proto types.
- **Client side**: Provides `DaemonClient`, a typed Rust wrapper around the generated tonic stubs. Handles endpoint discovery, connection management, and error translation. UIs call `DaemonClient` — they never use raw gRPC stubs.

Depends on `xearthlayer-proto` (for generated types) and `xearthlayer` (for domain types used in the server adapter). The client-side wrapper only uses proto types, but Cargo dependencies are crate-wide — the `xearthlayer` dependency is acceptable here because UI crates depend on `xearthlayer-api`, not on `xearthlayer` directly, so the service library's internals remain hidden behind the adapter.

#### xearthlayer (service library)

Unchanged. FUSE, cache, executor, prefetch, providers. Does not know about gRPC or any transport concern. The existing `ServiceOrchestrator` API is already UI-agnostic — the server adapter wraps it without modification.

#### xearthlayer-daemon

Binary crate. Creates the tokio runtime, boots `XEarthLayerService`, loads plugins (#112), starts the gRPC server, writes the state file, and signals readiness to systemd via `sd_notify`. Awaits shutdown from either SIGTERM or the Shutdown RPC.

#### xearthlayer-tui

Binary crate. Connects to the daemon via `DaemonClient`, subscribes to telemetry and position streams, renders the terminal dashboard. Quitting the TUI disconnects from the daemon without stopping it.

#### xearthlayer-cli

Retains all offline commands (`config`, `packages`, `cache`, `publish`, `diagnostics`, `setup`, `scenery-index`, `download`). Gains a `service` subcommand group and daemon-targeted commands.

A `run` convenience command preserves backward compatibility: it starts the daemon via `ServiceManager` (if not already running), then execs the TUI client process. On TUI exit, the daemon continues running. This keeps the familiar `xearthlayer run` experience while using the new architecture underneath.

### SOLID Analysis

The crate topology was chosen to satisfy SOLID principles at the module boundary:

- **Single Responsibility**: Each crate has one reason to change. Proto changes when the API contract evolves. The API crate changes when bridging logic changes. The service library never changes for transport reasons.
- **Open/Closed**: New transports or API versions can be new crates depending on the same abstractions. Nothing existing is modified.
- **Interface Segregation**: UIs depend only on `xearthlayer-proto` and `xearthlayer-api` — they never see FUSE, DDS encoding, or cache internals.
- **Dependency Inversion**: The proto crate is pure abstraction with zero internal dependencies. Both daemon and client sides depend inward on it. Concrete service types are only referenced in the adapter layer, which is the single point where concrete meets concrete.

This is Ports and Adapters (Hexagonal Architecture) at the crate level: `xearthlayer-proto` defines the port, `xearthlayer-api` provides the adapters.

### gRPC Service Definition

The API is split into three gRPC services, separated by concern. Clients import only the services they need.

#### XEarthLayerDaemon — Lifecycle and Discovery

```protobuf
service XEarthLayerDaemon {
  rpc Ping(PingRequest) returns (PingResponse);
  rpc GetInfo(GetInfoRequest) returns (DaemonInfo);
  rpc Shutdown(ShutdownRequest) returns (ShutdownResponse);
}
```

#### XEarthLayerTelemetry — Observability

```protobuf
service XEarthLayerTelemetry {
  rpc StreamTelemetry(StreamTelemetryRequest) returns (stream TelemetrySnapshot);
  rpc StreamTileProgress(StreamTileProgressRequest) returns (stream TileProgressUpdate);
  rpc StreamAircraftPosition(StreamAircraftPositionRequest) returns (stream AircraftPositionUpdate);
  rpc GetCacheStats(GetCacheStatsRequest) returns (CacheStats);
}
```

`StreamTelemetryRequest` includes an `interval_ms` field (100-5000, default 200) so clients control their update cadence. The daemon maintains a per-client timer that calls the existing `telemetry_snapshot()` method and pushes the result.

#### XEarthLayerControl — Operational Commands

```protobuf
service XEarthLayerControl {
  rpc PausePrefetch(PausePrefetchRequest) returns (PrefetchStatusResponse);
  rpc ResumePrefetch(ResumePrefetchRequest) returns (PrefetchStatusResponse);
  rpc GetPrefetchStatus(GetPrefetchStatusRequest) returns (PrefetchStatusResponse);
  rpc ClearCache(ClearCacheRequest) returns (ClearCacheResponse);
  rpc PrewarmAirport(PrewarmAirportRequest) returns (PrewarmAirportResponse);
}
```

This service is where future runtime reconfiguration RPCs (`SetConfig`, `GetConfig`, `SwitchProvider`, `ListPlugins`, `EnablePlugin`, `DisablePlugin`) would be added as the API grows beyond 0.5.0.

### Daemon Lifecycle

#### Startup

```
1.  Parse CLI args (--listen, --config, --log-level)
2.  Load configuration from ~/.xearthlayer/config.ini
3.  Check for existing daemon (state file PID check)
4.  Create tokio runtime (daemon owns it)
5.  Discover and load plugins (#112)
6.  Boot XEarthLayerService::start() (FUSE, cache, executor, prefetch)
7.  Register plugin hooks into FusePipeline (#111)
8.  Start gRPC server (bind to socket or address)
9.  Write state file
10. sd_notify(READY=1)
11. Await shutdown signal
```

The systemd unit uses `Type=notify` so that `systemctl start xearthlayer` blocks until step 10 completes, guaranteeing the service is ready when the command returns.

#### Shutdown

Both triggers (SIGTERM from systemd, Shutdown RPC from a client) converge on the same path:

```
1. Cancel async tasks (prefetch, telemetry, etc.) via CancellationToken
2. Drain active gRPC streams, reject new RPCs
3. Deregister plugin hooks, shutdown plugins
4. ServiceOrchestrator::shutdown() (unmount FUSE, flush cache, stop executor)
5. Remove state file
6. Exit
```

#### systemd Unit File

Installed to `~/.config/systemd/user/xearthlayer.service` (user-level, no root required):

```ini
[Unit]
Description=XEarthLayer Satellite Imagery Service
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
ExecStart=/usr/local/bin/xearthlayer-daemon
ExecStop=/bin/kill -SIGTERM $MAINPID
Restart=on-failure
RestartSec=5
LimitNOFILE=65536

[Install]
WantedBy=default.target
```

### Telemetry Streaming

The daemon pushes telemetry to connected clients using server-streaming RPCs at the interval each client requests.

- **Telemetry snapshots**: Per-client tokio interval timer calls `ServiceOrchestrator::telemetry_snapshot()`, serializes to proto, pushes to stream. Client disconnect cleans up the timer.
- **Tile progress**: Wraps `SharedTileProgressTracker`. Polls and diffs, pushing only on state changes.
- **Aircraft position**: Wraps `SharedAircraftPosition`. Pushes on updates from the `StateAggregator`.

This reuses the existing metrics infrastructure without modification. The gRPC layer is a thin translation and delivery mechanism.

### Client Architecture

#### DaemonClient

The `xearthlayer-api` crate provides `DaemonClient` as the single entry point for all UI and CLI interactions with the daemon.

**Discovery sequence**:

1. Check `$XEARTHLAYER_ENDPOINT` environment variable
2. Read state file from `$XDG_RUNTIME_DIR/xearthlayer/daemon.state`
3. Validate PID is alive
4. Connect to the endpoint and Ping to verify

If the daemon is not running, the client returns `ClientError::DaemonNotRunning`. UIs use this to show clear messages or offer to start the service.

**Lifecycle control**:

- `DaemonClient::start_daemon()` — Invokes the platform's service manager, waits for the state file
- `DaemonClient::start_and_connect()` — Starts the daemon and returns a connected client
- `client.stop()` — Sends the Shutdown RPC
- `client.restart()` — Stops, waits for exit, starts, reconnects

**Error translation**:

| gRPC Status | ClientError | User Message |
|-------------|-------------|-------------|
| `UNAVAILABLE` | `DaemonNotRunning` | "Daemon is not running" |
| `DEADLINE_EXCEEDED` | `Timeout` | "Daemon not responding" |
| `INTERNAL` | `DaemonError(msg)` | "Daemon error: {msg}" |
| `INVALID_ARGUMENT` | `InvalidRequest(msg)` | "Invalid request: {msg}" |

The TUI handles `DaemonNotRunning` with reconnection backoff, since the daemon may be restarting via systemd `Restart=on-failure`.

#### TUI Client

The TUI becomes a pure presentation process:

1. `DaemonClient::connect()` (or offer to start the daemon if not running)
2. Subscribe to telemetry stream (100ms), tile progress stream, aircraft position stream
3. Enter ratatui event loop, rendering from the latest streamed state
4. Keyboard commands (pause prefetch, etc.) send RPCs to the daemon
5. On quit: drop streams, disconnect — daemon keeps running

#### CLI Commands

```
xearthlayer service start       Start daemon via OS service manager
xearthlayer service stop        Shutdown RPC + service manager stop
xearthlayer service restart     Stop then start
xearthlayer service status      Ping + GetInfo (uptime, version, endpoint)
xearthlayer service install     Install systemd unit / launchd plist
xearthlayer service uninstall   Remove unit/plist files
xearthlayer service logs        Convenience wrapper for journalctl

xearthlayer prefetch status     GetPrefetchStatus RPC
xearthlayer prefetch pause      PausePrefetch RPC
xearthlayer prefetch resume     ResumePrefetch RPC

xearthlayer prewarm <ICAO>      PrewarmAirport RPC

xearthlayer run                 Start daemon + attach TUI (backward compat)
```

Short-lived commands create a `DaemonClient`, make one RPC, print the result, and exit.

## Relationship to Other 0.5.0 Work

This design develops on the `develop/0.5.0` branch established by #113. It interacts with two other workstreams:

**#111 (FUSE hook pipeline)**: The hook pipeline runs inside the daemon process. No impact on the gRPC API — hooks are an internal daemon concern. The two workstreams develop in parallel.

**#112 (Plugin architecture)**: Plugins are loaded by the daemon at startup. Plugin hooks register into the FUSE pipeline inside the daemon. Future gRPC RPCs for plugin management would be added to `XEarthLayerControl` post-0.5.0.

### Development Ordering

| Phase | Work | Dependencies |
|-------|------|-------------|
| 0 | #113 — Release channels, `develop/0.5.0` branch and CI | None, must be first |
| 1a | #111 — FUSE hook pipeline | #113 |
| 1b | `xearthlayer-proto` and `xearthlayer-api` crates | #113 |
| 2 | #112 — Plugin SDK and dynamic loading | #111 |
| 3 | `xearthlayer-daemon` binary | #112, phase 1b |
| 4 | `DaemonClient` and `xearthlayer-tui` extraction | Phase 3 |
| 5 | CLI cleanup, `run` alias, remove old TUI coupling | Phase 4 |

Phases 1a and 1b proceed in parallel. The daemon binary (phase 3) is where FUSE hooks, plugin loading, and gRPC server converge.

## Migration Strategy

The migration follows the Strangler Fig pattern: new components grow alongside existing code, and old paths are removed only after the new ones are proven.

Through phases 0-3, the existing `xearthlayer run` command continues to work exactly as it does today. Users see no disruption.

Phase 4 introduces the new TUI client, testable against the daemon while the old path still exists.

Phase 5 removes the old in-process path, replaces `xearthlayer run` with a convenience alias that starts the daemon then attaches the TUI, and removes TUI dependencies (`ratatui`, `crossterm`) from the CLI crate.

## Packaging and Distribution

### GPU Feature Flag Convergence

The `gpu-encode` Cargo feature flag is removed in 0.5.0. GPU-accelerated DDS compression (`wgpu` + `block_compression`) is compiled unconditionally into the daemon binary. Users select the compressor backend at runtime via `texture.compressor` in config (software, ispc, or gpu). This eliminates the doubled build matrix that the feature flag created — no more separate GPU release artifacts.

### Package Model

The three binaries ship as individual packages with a meta-package umbrella:

```
xearthlayer                     (meta-package — depends on all below)
├── xearthlayer-daemon          (service binary + systemd unit)
├── xearthlayer-cli             (offline CLI + daemon client commands)
└── xearthlayer-tui             (terminal UI client)
```

Individual packages can be installed independently. `xearthlayer-daemon` alone is sufficient for a headless server. The meta-package provides the familiar `apt install xearthlayer` or `dnf install xearthlayer` experience that pulls everything in.

### Per-Format Details

| Format | Implementation |
|--------|---------------|
| **deb** | Three real packages + one virtual meta-package. `xearthlayer-daemon` includes the systemd unit file. The meta-package declares `Depends: xearthlayer-daemon, xearthlayer-cli, xearthlayer-tui`. |
| **rpm** | Same structure. Meta-package uses `Requires:` directives. |
| **AUR** | Four PKGBUILDs: one per binary, one meta. Arch users can install individually. |
| **tarball** | Single archive containing all three binaries, the systemd unit file, README, and LICENSE. Same as the current single-archive approach. |

### Versioning

All packages share the same **major.minor** version. Patch versions can diverge — a TUI bug fix does not require rebuilding the daemon. The proto crate includes a version field in `PingResponse` so clients and daemon can detect incompatible versions at connection time.

Package dependencies use version ranges to enforce this:

```
# xearthlayer-tui 0.5.3
Depends: xearthlayer-daemon (>= 0.5.0), xearthlayer-daemon (<< 0.6.0)
```

### CI/CD Pipeline

The release workflow builds three binaries in parallel from the same workspace, then packages them:

```
build-jobs (parallel):
  - build-daemon    → xearthlayer-daemon
  - build-cli       → xearthlayer
  - build-tui       → xearthlayer-tui

package-jobs (parallel):
  - package-tarball     (all three binaries + systemd unit)
  - package-deb         (3 real packages + 1 meta)
  - package-rpm         (3 real packages + 1 meta)
  - prepare-aur         (4 PKGBUILDs)
```

No build variants, no matrix. One build per binary, one set of packages.

### GUI Distribution (Future)

The GUI ships as a separate package because its dependency tree (UI toolkit, windowing) and distribution channels differ:

| Format | Approach |
|--------|----------|
| **deb/rpm** | `xearthlayer-gui` package with `Depends: xearthlayer-daemon, xearthlayer-cli` |
| **Flatpak** | Self-contained sandbox bundling all binaries including daemon |
| **AppImage** | Single downloadable file bundling everything |

For users who want a single GUI installer that includes all dependencies, Flatpak or AppImage provides that experience naturally.

## Testing Strategy

### xearthlayer-proto

No tests needed. Generated code is validated by compilation.

### xearthlayer-api (server adapter)

Unit tests mock service orchestrator dependencies and verify gRPC implementations translate domain types to proto types correctly.

Integration tests start a real tonic server in-process with mocked service state, connect a client, and exercise the full RPC path including streaming.

### xearthlayer-api (client wrapper)

Unit tests verify `DaemonClient` methods translate calls to RPCs and handle gRPC error status codes correctly.

Discovery tests create temporary directories with state files and verify endpoint resolution, stale PID detection, missing state files, and environment variable overrides.

### xearthlayer-daemon

Process-level tests spawn the daemon binary as a child process, wait for the state file, connect via `DaemonClient`, exercise RPCs, then shut down. Tests verify clean startup, singleton enforcement, SIGTERM handling, and state file cleanup.

### xearthlayer-tui

Unit tests cover stream-to-dashboard-state transformation: given a sequence of proto messages, verify local rendering state is correctly maintained. Terminal rendering is validated manually.

### ServiceManager

The `ServiceManager` trait enables mock-based unit tests. Integration tests for systemd interaction run in CI environments that provide it.
