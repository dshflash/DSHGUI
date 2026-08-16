# dsh-desktop — DeepSeek Harness desktop app

A Tauri 2 shell that runs the [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) engine
(`dsh web`) as a managed child process and embeds its Web UI in a native window.

## Architecture

DeepSeek Harness is a Cordis plugin monorepo whose entire UI is served by the engine over
localhost HTTP/WebSockets (the page only works with the server-injected `window.__DSH_BOOT__`).
The desktop app therefore does not reimplement the UI: it is a thin native shell.

- **Rust (`src-tauri/`)** owns the engine lifecycle: engine-source resolution, free-port
  reservation, spawn, readiness detection (URL line `dsh web: http://127.0.0.1:<port>` plus a
  TCP health probe), process-tree teardown on exit, and window navigation between the splash
  shell and the engine UI.
- **Shell UI (`src/`)** is a small React splash: engine status, start/stop/restart/setup
  actions, engine-directory setting, and a live engine log. The splash is only visible while
  the engine boots or after it stops.
- **Security**: the remote harness UI runs unprivileged in the webview. It gets no Tauri IPC
  and no capability; the capability file covers only the shell window's own `core:default`.

## Engine source resolution

The shell picks the engine in this order:

1. The engine directory saved in the shell settings (app config `settings.json`).
2. The `DSH_ENGINE_DIR` environment variable.
3. A `deepseek-harness` checkout next to the app (dev convenience).
4. Fallback: the published package via `npx --yes @deepseek-ai/dsh web`.

A checkout is used when `apps/cli/src/bin.ts` exists and is considered ready when
`pnpm install` ran and `apps/cli/lib/bin.js` exists (built). An installed-but-unbuilt
checkout shows a **Setup engine** action that runs `pnpm install` and `pnpm run build`
inside it, streaming progress to the log.

Node.js 22+ is required. Override the node binary with `DSH_NODE_BIN`.

## Development

Prerequisites: Node.js 22+, pnpm, Rust toolchain (MSVC on Windows), WebView2 runtime.

```sh
# 1. Prepare the engine (the checkout in this repo's folder)
cd deepseek-harness
pnpm install
pnpm run build
cd ..

# 2. Install the shell's frontend dependencies
npm install

# 3. Run the desktop app (compiles the Rust shell on first run)
npm run tauri dev
```

The window opens with a splash while the engine boots, then navigates to the engine UI.
Stop the app by closing the window; the engine process tree is killed on exit. Even a
hard-killed shell cannot orphan the engine: on Windows the engine runs inside a job
object with `KILL_ON_JOB_CLOSE`.

Engine diagnostics: every engine line and shell event is appended to
`engine-debug.log` in the app config directory (`%APPDATA%\ai.deepseek.harness-desktop\`
on Windows).

## Packaging

```sh
npm run tauri build   # produces NSIS/MSI installers under src-tauri/target/release/bundle
```

The packaged app has no bundled engine: without `DSH_ENGINE_DIR` or a saved engine
directory it uses the published `@deepseek-ai/dsh` package via npx (requires network on
first start). To ship a self-contained build, install the harness into a directory and set
it once in the shell settings, or bundle the checkout with the installer.

## Layout

```
src/                 React splash shell (Vite)
src-tauri/
  src/engine.rs      engine process manager (spawn, readiness, teardown, events)
  src/commands.rs    typed Tauri commands for the shell UI
  src/settings.rs    persisted shell settings (JSON in app config dir)
  src/lib.rs         app builder: menu, events, navigation wiring
  capabilities/      shell-window capabilities only
deepseek-harness/    upstream engine checkout (source of record)
```