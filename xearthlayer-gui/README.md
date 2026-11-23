# XEarthLayer desktop app

A Tauri 2 app with a React + Vite + TypeScript frontend for people who do not
use the terminal. It does what the `xearthlayer` CLI and its dashboards do:

| Page        | CLI equivalent                                                    |
|-------------|-------------------------------------------------------------------|
| Dashboard   | `run` (start/stop, `--airport`, `--no-prefetch`, `--no-cache`) and its dashboard |
| Packages    | `packages list/install/update/remove`, the package manager TUI, overlay rebuild |
| Setup       | `setup` (X-Plane, packages, provider, network, cache, encoder)    |
| Settings    | `config list/get/set/upgrade/path`, `init`                         |
| Maintenance | `cache stats/clear`, `scenery-index status/update/clear`, `patches list`, `diagnostics` |

`migrate` is not in the app yet.

## Layout

- `src-tauri/`: the `xearthlayer-gui` workspace crate (binary `xearthlayer-gui`).
  It links the `xearthlayer` core library directly, as the CLI does. Some
  front-end rules (path precedence, provider resolution, overlay rebuild) still
  live in the CLI crate and are repeated here; they move into core once it
  serves both clients.
- `src/`: the React frontend. `api.ts` has typed wrappers for every Tauri command.

The streaming service runs on one dedicated thread that owns the
`ServiceOrchestrator`. Commands only raise flags and read the status it
publishes, and the frontend polls that status. Closing the window, or
SIGINT/SIGTERM/SIGHUP, unmounts before the process exits. The app writes
`xearthlayer-gui.log` in the state directory, never the flight log. Set
`XEARTHLAYER_DEBUG=1` for debug logging.

## Building

```bash
make build        # npm ci (when needed) + frontend + every binary, GUI included
make release      # the same, optimized
make gui-dev      # hot reload: Vite dev server + debug app
make gui-bundle   # .app/.dmg (macOS) or .deb/.AppImage (Linux) in target/release/bundle
```

Requires Node.js 20.19+. Linux also needs `libwebkit2gtk-4.1-dev` and
`librsvg2-dev`. A plain `cargo build` without the frontend still compiles:
`build.rs` writes a placeholder page into `dist/`.
