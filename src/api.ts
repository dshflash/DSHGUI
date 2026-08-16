// Typed bridge between the splash shell and the Rust layer.
// The harness UI itself never uses Tauri IPC: it is served by the engine over
// localhost HTTP/WebSockets and runs with no privileged access.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Phase = "idle" | "starting" | "ready" | "stopped" | "error";

export interface EngineState {
  phase: Phase;
  url: string | null;
  port: number | null;
  error: string | null;
  exit_code: number | null;
  mode: string | null;
  setup_needed: boolean;
  setup_active: boolean;
  log_len: number;
}

export interface Settings {
  engine_dir: string | null;
}

export interface SetupEvent {
  stage: string;
  line: string | null;
  done: boolean;
  error: string | null;
}

export const getState = (): Promise<EngineState> => invoke("get_state");
export const getLog = (from: number): Promise<string[]> => invoke("get_log", { from });
export const startEngine = (): Promise<void> => invoke("start_engine");
export const stopEngine = (): Promise<void> => invoke("stop_engine");
export const setupEngine = (): Promise<void> => invoke("setup_engine");
export const pickEngineDir = (): Promise<string | null> => invoke("pick_engine_dir");
export const setEngineDir = (dir: string | null): Promise<void> => invoke("set_engine_dir", { dir });
export const getSettings = (): Promise<Settings> => invoke("get_settings");
export const openExternal = (url: string): Promise<void> => invoke("open_external", { url });

export function onEngineState(cb: (s: EngineState) => void): Promise<UnlistenFn> {
  return listen<EngineState>("engine://state", (e) => cb(e.payload));
}

export function onEngineLog(cb: (line: string) => void): Promise<UnlistenFn> {
  return listen<string>("engine://log", (e) => cb(e.payload));
}

export function onEngineSetup(cb: (e: SetupEvent) => void): Promise<UnlistenFn> {
  return listen<SetupEvent>("engine://setup", (e) => cb(e.payload));
}
