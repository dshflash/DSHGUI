import { useCallback, useEffect, useRef, useState } from "react";
import {
  getLog,
  getSettings,
  getState,
  onEngineLog,
  onEngineSetup,
  onEngineState,
  openExternal,
  pickEngineDir,
  setEngineDir,
  setupEngine,
  startEngine,
  stopEngine,
  type EngineState,
  type Phase,
  type SetupEvent,
} from "./api";

const PHASE_COPY: Record<Phase, { label: string; color: string; dot: string }> = {
  idle: { label: "Idle", color: "text-slate-400 border-slate-600", dot: "bg-slate-400" },
  starting: { label: "Starting engine…", color: "text-amber-300 border-amber-500/50", dot: "bg-amber-400 animate-pulse" },
  ready: { label: "Engine ready", color: "text-emerald-300 border-emerald-500/50", dot: "bg-emerald-400" },
  stopped: { label: "Engine stopped", color: "text-slate-300 border-slate-600", dot: "bg-slate-400" },
  error: { label: "Engine error", color: "text-red-300 border-red-500/50", dot: "bg-red-400" },
};

function Spinner({ show }: { show: boolean }) {
  if (!show) return null;
  return (
    <span className="inline-block h-3.5 w-3.5 animate-spin rounded-full border-2 border-amber-400 border-t-transparent align-middle" />
  );
}

function Card({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="rounded-xl border border-white/10 bg-white/[0.03] p-5">
      <h2 className="mb-3 text-xs font-semibold uppercase tracking-widest text-slate-400">{title}</h2>
      {children}
    </section>
  );
}

export default function App() {
  const [state, setState] = useState<EngineState | null>(null);
  const [log, setLog] = useState<string[]>([]);
  const [dirDraft, setDirDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [setupStage, setSetupStage] = useState<string | null>(null);
  const [showLog, setShowLog] = useState(true);
  const logRef = useRef<HTMLDivElement>(null);
  const logIndex = useRef(0);

  useEffect(() => {
    let disposed = false;
    void getState().then((s) => { if (!disposed) setState(s); });
    void getSettings().then((s) => { if (!disposed) setDirDraft(s.engine_dir ?? ""); });
    void getLog(0).then((lines) => { if (!disposed) { setLog(lines); logIndex.current = lines.length; } });
    let unState: (() => void) | undefined;
    let unLog: (() => void) | undefined;
    let unSetup: (() => void) | undefined;
    void onEngineState((s) => {
      setState(s);
      if (s.log_len > logIndex.current) {
        void getLog(logIndex.current).then((lines) => {
          setLog((prev) => [...prev, ...lines]);
          logIndex.current += lines.length;
        });
      }
    }).then((fn) => { unState = fn; });
    void onEngineLog((line) => {
      setLog((prev) => [...prev.slice(-999), line]);
      logIndex.current += 1;
    }).then((fn) => { unLog = fn; });
    void onEngineSetup((e: SetupEvent) => {
      const line = e.line;
      if (line) setLog((prev) => [...prev.slice(-999), line]);
      if (e.done) setSetupStage(null);
      if (e.error) setSetupStage(null);
    }).then((fn) => { unSetup = fn; });
    return () => {
      disposed = true;
      unState?.();
      unLog?.();
      unSetup?.();
    };
  }, []);

  useEffect(() => {
    if (showLog && logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight;
  }, [log, showLog]);

  const run = useCallback(async (fn: () => Promise<unknown>) => {
    setBusy(true);
    try { await fn(); } finally { setBusy(false); }
  }, []);

  const phase = state?.phase ?? "idle";
  const copy = PHASE_COPY[phase];
  const setupBusy = setupStage !== null || state?.setup_active === true;

  const onSaveDir = () => {
    const dir = dirDraft.trim();
    void run(() => setEngineDir(dir.length > 0 ? dir : null));
  };

  const onUseNpx = () => {
    setDirDraft("");
    void run(() => setEngineDir(null));
  };

  return (
    <div className="mx-auto flex h-full max-w-3xl flex-col gap-4 overflow-y-auto p-6">
      <header className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-gradient-to-br from-blue-500 to-indigo-700 text-sm font-bold text-white">
            DSH
          </div>
          <div>
            <h1 className="text-lg font-semibold leading-tight">DeepSeek Harness</h1>
            <p className="text-xs text-slate-400">Desktop shell · dsh web engine</p>
          </div>
        </div>
        <div className={`flex items-center gap-2 rounded-full border px-3 py-1 text-xs font-medium ${copy.color}`}>
          <span className={`h-2 w-2 rounded-full ${copy.dot}`} />
          {copy.label}
        </div>
      </header>

      <Card title="Engine">
        <div className="space-y-3">
          <div className="flex items-center gap-2 text-sm">
            <Spinner show={phase === "starting" || setupBusy} />
            <span className="text-slate-300">
              {setupBusy
                ? setupStage
                  ? `Engine setup: ${setupStage}… (this can take several minutes)`
                  : "Engine setup in progress…"
                : phase === "starting"
                  ? "Waiting for the Web UI server…"
                  : phase === "idle"
                    ? "Engine not started."
                    : phase === "ready"
                      ? "The Web UI is running in this window."
                      : phase === "stopped"
                        ? state?.exit_code !== null
                          ? `Engine exited (code ${state?.exit_code ?? "unknown"}).`
                          : "Engine stopped."
                        : "The engine could not start."}
            </span>
          </div>

          {state?.mode && (
            <p className="text-xs text-slate-500">
              Source: <span className="font-mono">{state.mode}</span>
            </p>
          )}

          {phase === "error" && state?.error && (
            <div className="rounded-lg border border-red-500/30 bg-red-500/10 p-3 text-sm text-red-200">
              {state.error}
              {state.setup_needed && (
                <div className="mt-2 text-xs text-red-300/80">
                  The checkout is installed but not built. Run <span className="font-mono">Setup engine</span> to run{" "}
                  <span className="font-mono">pnpm install</span> + <span className="font-mono">pnpm run build</span> inside it.
                </div>
              )}
            </div>
          )}

          {phase === "ready" && state?.url && (
            <div className="flex items-center gap-2 rounded-lg border border-emerald-500/30 bg-emerald-500/10 p-3 text-sm">
              <span className="font-mono text-emerald-200">{state.url}</span>
              <button
                className="ml-auto rounded-md bg-emerald-600 px-3 py-1.5 text-xs font-semibold text-white hover:bg-emerald-500 disabled:opacity-50"
                disabled={busy}
                onClick={() => void run(() => openExternal(state.url ?? ""))}
              >
                Open in browser
              </button>
            </div>
          )}

          <div className="flex flex-wrap gap-2">
            {(phase === "idle" || phase === "stopped" || phase === "error") && (
              <button
                className="rounded-md bg-blue-600 px-4 py-2 text-sm font-semibold text-white hover:bg-blue-500 disabled:opacity-50"
                disabled={busy || setupBusy}
                onClick={() => void run(() => startEngine())}
              >
                {phase === "error" ? "Retry" : "Start engine"}
              </button>
            )}
            {state?.setup_needed && (
              <button
                className="rounded-md bg-amber-600 px-4 py-2 text-sm font-semibold text-white hover:bg-amber-500 disabled:opacity-50"
                disabled={busy || setupBusy}
                onClick={() => void run(() => setupEngine())}
              >
                {setupBusy ? "Setting up…" : "Setup engine (pnpm install + build)"}
              </button>
            )}
            {(phase === "starting" || phase === "ready") && (
              <button
                className="rounded-md border border-white/15 px-4 py-2 text-sm font-medium text-slate-200 hover:bg-white/5 disabled:opacity-50"
                disabled={busy || setupBusy}
                onClick={() => void run(() => stopEngine())}
              >
                Stop engine
              </button>
            )}
            {(phase === "starting" || phase === "ready") && (
              <button
                className="rounded-md border border-white/15 px-4 py-2 text-sm font-medium text-slate-200 hover:bg-white/5 disabled:opacity-50"
                disabled={busy || setupBusy}
                onClick={() => void run(() => startEngine())}
              >
                Restart
              </button>
            )}
          </div>
        </div>
      </Card>

      <Card title="Engine source">
        <p className="mb-3 text-xs leading-relaxed text-slate-400">
          Point at a <span className="font-mono">deepseek-harness</span> checkout (installed with{" "}
          <span className="font-mono">pnpm install</span> and built with <span className="font-mono">pnpm run build</span>), or
          fall back to the published <span className="font-mono">@deepseek-ai/dsh</span> package via npx. The environment
          variable <span className="font-mono">DSH_ENGINE_DIR</span> is used when this field is empty.
        </p>
        <div className="flex gap-2">
          <input
            className="min-w-0 flex-1 rounded-md border border-white/15 bg-black/30 px-3 py-2 font-mono text-xs text-slate-200 outline-none focus:border-blue-500"
            placeholder="Path to a deepseek-harness checkout"
            value={dirDraft}
            onChange={(e) => setDirDraft(e.target.value)}
            spellCheck={false}
          />
          <button
            className="rounded-md border border-white/15 px-3 py-2 text-xs font-medium text-slate-200 hover:bg-white/5 disabled:opacity-50"
            disabled={busy}
            onClick={() => void run(async () => {
              const dir = await pickEngineDir();
              if (dir) { setDirDraft(dir); await setEngineDir(dir); }
            })}
          >
            Browse…
          </button>
          <button
            className="rounded-md bg-blue-600 px-3 py-2 text-xs font-semibold text-white hover:bg-blue-500 disabled:opacity-50"
            disabled={busy}
            onClick={onSaveDir}
          >
            Save &amp; restart
          </button>
          <button
            className="rounded-md border border-white/15 px-3 py-2 text-xs font-medium text-slate-200 hover:bg-white/5 disabled:opacity-50"
            disabled={busy || dirDraft === ""}
            onClick={onUseNpx}
          >
            Use npx package
          </button>
        </div>
      </Card>

      <Card title="Engine log">
        <button
          className="mb-2 rounded-md border border-white/15 px-3 py-1.5 text-xs font-medium text-slate-200 hover:bg-white/5"
          onClick={() => setShowLog((v) => !v)}
        >
          {showLog ? "Hide log" : "Show log"}
        </button>
        {showLog && (
          <div
            ref={logRef}
            className="log-line max-h-72 overflow-y-auto rounded-lg border border-white/10 bg-black/40 p-3 text-slate-300"
          >
            {log.length === 0 ? (
              <span className="text-slate-600">No engine output yet.</span>
            ) : (
              log.map((line, i) => (
                <div key={i} className="text-slate-300">
                  {line || "\u00a0"}
                </div>
              ))
            )}
          </div>
        )}
      </Card>

      <p className="pb-4 text-center text-[11px] text-slate-600">
        The harness UI runs unprivileged in this window; it never gains Tauri capabilities. Engine data lives under the
        standard user config directory.
      </p>
    </div>
  );
}
