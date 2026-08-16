//! Managed `dsh web` engine process.
//!
//! The shell owns one engine lifecycle: resolve how to run the harness
//! (a built checkout, or the published package via npx), reserve a port,
//! spawn the server, detect readiness from its URL line or a TCP probe,
//! and tear the process tree down on exit. Every state change is emitted
//! to the shell UI as `engine://state`; engine output streams as
//! `engine://log`; setup progress as `engine://setup`.

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};
use url::Url;

use crate::settings::Settings;

pub const EVENT_STATE: &str = "engine://state";
pub const EVENT_LOG: &str = "engine://log";
pub const EVENT_SETUP: &str = "engine://setup";

/// Rolling log line cap kept in memory for the shell log panel.
const LOG_CAP: usize = 600;
/// How long the health probe waits for the Web UI to come up.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// CREATE_NO_WINDOW: keep the engine console invisible on Windows.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, serde::Serialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Starting,
    Ready,
    Stopped,
    Error,
}

#[derive(Clone, serde::Serialize)]
pub struct EngineStateView {
    pub phase: Phase,
    pub url: Option<String>,
    pub port: Option<u16>,
    pub error: Option<String>,
    pub exit_code: Option<i32>,
    pub mode: Option<String>,
    pub setup_needed: bool,
    pub setup_active: bool,
    pub log_len: usize,
}

#[derive(Clone, serde::Serialize)]
pub struct SetupEvent {
    pub stage: String,
    pub line: Option<String>,
    pub done: bool,
    pub error: Option<String>,
}

#[derive(Default)]
struct EngineInner {
    phase: Phase,
    url: Option<String>,
    port: Option<u16>,
    error: Option<String>,
    exit_code: Option<i32>,
    mode: Option<String>,
    setup_needed: bool,
    setup_active: bool,
}

/// Tauri-managed wrapper so commands can reach the shared engine.
pub struct ManagedEngine(pub Arc<EngineManager>);

/// How to run the engine on this machine, resolved per start.
struct EnginePlan {
    mode: String,
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
}

pub struct EngineManager {
    app: AppHandle,
    inner: Arc<Mutex<EngineInner>>,
    log: Arc<Mutex<Vec<String>>>,
    child: Arc<Mutex<Option<Child>>>,
    setup_child: Arc<Mutex<Option<Child>>>,
    generation: Arc<AtomicU64>,
    setup_running: Arc<AtomicBool>,
    settings: Arc<Mutex<Settings>>,
    splash_url: Arc<Mutex<Option<Url>>>,
    debug_path: Arc<Mutex<Option<PathBuf>>>,
    #[cfg(target_os = "windows")]
    job: Arc<Mutex<Option<job::KillOnCloseJob>>>,
}

impl EngineManager {
    pub fn new(app: AppHandle, settings: Settings, splash_url: Option<Url>) -> Arc<Self> {
        let manager = Arc::new(Self {
            app: app.clone(),
            inner: Arc::new(Mutex::new(EngineInner::default())),
            log: Arc::new(Mutex::new(Vec::new())),
            child: Arc::new(Mutex::new(None)),
            setup_child: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
            setup_running: Arc::new(AtomicBool::new(false)),
            settings: Arc::new(Mutex::new(settings)),
            splash_url: Arc::new(Mutex::new(splash_url)),
            debug_path: Arc::new(Mutex::new(None)),
            #[cfg(target_os = "windows")]
            job: Arc::new(Mutex::new(job::KillOnCloseJob::new())),
        });
        *manager.debug_path.lock().unwrap() = app
            .path()
            .app_config_dir()
            .ok()
            .map(|dir| dir.join("engine-debug.log"));
        manager
    }

    pub fn snapshot(&self) -> EngineStateView {
        let inner = self.inner.lock().unwrap();
        let log_len = self.log.lock().unwrap().len();
        view_of(&inner, log_len)
    }

    pub fn log_lines(&self, from: usize) -> Vec<String> {
        let log = self.log.lock().unwrap();
        if from >= log.len() {
            Vec::new()
        } else {
            log[from..].to_vec()
        }
    }

    pub fn set_settings(&self, settings: Settings) {
        *self.settings.lock().unwrap() = settings;
    }

    /// Start (or restart from a terminal phase) the engine server.
    pub fn start(self: &Arc<Self>) -> Result<(), String> {
        {
            let inner = self.inner.lock().unwrap();
            if matches!(inner.phase, Phase::Starting | Phase::Ready) {
                return Ok(());
            }
        }
        if self.setup_running.load(Ordering::SeqCst) {
            return Err("engine setup is running; wait for it to finish".to_string());
        }
        let port = match pick_free_port() {
            Ok(port) => port,
            Err(e) => {
                self.debug_log(&format!("[dsh-desktop] start error: {e}"));
                return Err(e);
            }
        };
        let plan = match self.resolve_plan(port) {
            Ok(plan) => plan,
            Err(e) => {
                self.debug_log(&format!("[dsh-desktop] resolve_plan error: {e}"));
                let setup_needed = self
                    .engine_dir()
                    .map(|dir| is_checkout(&dir) && !dir.join("apps").join("cli").join("lib").join("bin.js").is_file())
                    .unwrap_or(false);
                self.transition(|inner| {
                    inner.phase = Phase::Error;
                    inner.error = Some(e.clone());
                    inner.setup_needed = setup_needed;
                });
                return Err(e);
            }
        };
        match self.spawn(plan, port) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.debug_log(&format!("[dsh-desktop] spawn error: {e}"));
                self.transition(|inner| {
                    inner.phase = Phase::Error;
                    inner.error = Some(e.clone());
                });
                Err(e)
            }
        }
    }

    pub fn restart(self: &Arc<Self>) -> Result<(), String> {
        self.stop();
        self.start()
    }

    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.setup_running.store(false, Ordering::SeqCst);
        let child = self.child.lock().unwrap().take();
        let setup_child = self.setup_child.lock().unwrap().take();
        if let Some(mut child) = child {
            let _ = kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(mut child) = setup_child {
            let _ = kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
        self.transition(|inner| {
            inner.setup_active = false;
            if matches!(inner.phase, Phase::Starting | Phase::Ready) {
                inner.phase = Phase::Stopped;
                inner.exit_code = None;
                inner.error = None;
            }
        });
    }

    /// Prepare a checkout that is not yet installed or built: run
    /// `pnpm install` then `pnpm run build`, streaming progress, then
    /// auto-start the engine.
    pub fn setup_engine(self: &Arc<Self>) -> Result<(), String> {
        if self.setup_running.swap(true, Ordering::SeqCst) {
            return Err("engine setup is already running".to_string());
        }
        if self.is_running() {
            self.setup_running.store(false, Ordering::SeqCst);
            return Err("the engine is running; stop it before running setup".to_string());
        }
        let Some(dir) = self.engine_dir() else {
            self.setup_running.store(false, Ordering::SeqCst);
            return Err(
                "no engine checkout configured: set an engine directory in the shell settings first".to_string(),
            );
        };
        if !is_checkout(&dir) {
            self.setup_running.store(false, Ordering::SeqCst);
            return Err(format!(
                "{} is not a deepseek-harness checkout (missing apps/cli/src/bin.ts)",
                dir.display()
            ));
        }
        self.transition(|inner| {
            inner.setup_active = true;
            inner.setup_needed = true;
            inner.error = None;
        });
        let this = Arc::clone(self);
        thread::spawn(move || this.run_setup(&dir));
        Ok(())
    }
    #[cfg(target_os = "windows")]
    fn assign_to_job(&self, pid: u32) {
        if let Some(job) = self.job.lock().unwrap().as_ref() {
            job.assign(pid);
        }
    }

    fn is_running(&self) -> bool {
        matches!(self.inner.lock().unwrap().phase, Phase::Starting | Phase::Ready)
    }

    fn run_setup(self: &Arc<Self>, dir: &Path) {
        let stages: [(&str, &[&str]); 2] = [
            ("install", &["install", "--no-frozen-lockfile"]),
            ("build", &["run", "build"]),
        ];
        for (stage, args) in stages {
            self.emit_setup(SetupEvent {
                stage: stage.to_string(),
                line: None,
                done: false,
                error: None,
            });
            match self.run_setup_stage(dir, stage, args) {
                Ok(true) => {}
                Ok(false) => {
                    self.emit_setup(SetupEvent {
                        stage: stage.to_string(),
                        line: None,
                        done: false,
                        error: Some(format!("pnpm {stage} failed; see the log for details")),
                    });
                    self.finish_setup(false);
                    return;
                }
                Err(e) => {
                    self.emit_setup(SetupEvent {
                        stage: stage.to_string(),
                        line: None,
                        done: false,
                        error: Some(e),
                    });
                    self.finish_setup(false);
                    return;
                }
            }
        }
        self.emit_setup(SetupEvent {
            stage: "done".to_string(),
            line: None,
            done: true,
            error: None,
        });
        self.finish_setup(true);
    }

    fn run_setup_stage(self: &Arc<Self>, dir: &Path, stage: &str, args: &[&str]) -> Result<bool, String> {
        let mut command = make_pnpm_command(dir, args);
        let mut child = command.spawn().map_err(|e| format!("failed to start pnpm {stage}: {e}"))?;
        let pid = child.id();
        #[cfg(target_os = "windows")]
        self.assign_to_job(pid);
        self.push_log(format!("[dsh-desktop] pnpm {stage} started (pid {pid})"));
        let stdout = child.stdout.take().ok_or("pnpm stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("pnpm stderr unavailable")?;
        *self.setup_child.lock().unwrap() = Some(child);
        let this = Arc::clone(self);
        thread::spawn(move || log_reader(stdout, this));
        let this = Arc::clone(self);
        thread::spawn(move || log_reader(stderr, this));
        let child = self.setup_child.lock().unwrap().take();
        let mut child = child.ok_or("pnpm child disappeared")?;
        let status = child.wait().map_err(|e| format!("pnpm {stage} wait failed: {e}"))?;
        Ok(status.success())
    }

    fn finish_setup(self: &Arc<Self>, success: bool) {
        self.setup_running.store(false, Ordering::SeqCst);
        *self.setup_child.lock().unwrap() = None;
        self.transition(|inner| {
            inner.setup_active = false;
            if success {
                inner.setup_needed = false;
                if inner.phase == Phase::Error {
                    inner.phase = Phase::Idle;
                    inner.error = None;
                }
            } else {
                inner.setup_needed = true;
            }
        });
        if success {
            let _ = self.start();
        }
    }

    fn emit_setup(&self, event: SetupEvent) {
        let _ = self.app.emit(EVENT_SETUP, event);
    }

    fn debug_log(&self, msg: &str) {
        let path = self.debug_path.lock().unwrap().clone();
        if let Some(path) = path {
            use std::io::Write;
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{msg}");
            }
        }
    }

    fn push_log(&self, line: String) {
        let clean = strip_ansi(&line);
        {
            let mut log = self.log.lock().unwrap();
            if log.len() >= LOG_CAP {
                let drop = log.len() - LOG_CAP + 1;
                log.drain(..drop);
            }
            log.push(clean.clone());
        }
        self.debug_log(&clean);
        let _ = self.app.emit(EVENT_LOG, clean);
    }

    fn resolve_plan(&self, port: u16) -> Result<EnginePlan, String> {
        let node = resolve_node()?;
        if let Some(dir) = self.engine_dir() {
            if !is_checkout(&dir) {
                return Err(format!(
                    "{} is not a deepseek-harness checkout (missing apps/cli/src/bin.ts)",
                    dir.display()
                ));
            }
            let installed = dir.join("node_modules").is_dir();
            let built = dir.join("apps").join("cli").join("lib").join("bin.js").is_file();
            if !installed || !built {
                return Err(format!(
                    "The engine checkout at {} is not ready: it needs `pnpm install` and `pnpm run build`. Run the setup action to prepare it.",
                    dir.display()
                ));
            }
            let bin = dir.join("apps").join("cli").join("lib").join("bin.js");
            let args = vec![
                bin.to_string_lossy().into_owned(),
                "web".to_string(),
                "--host".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                port.to_string(),
            ];
            return Ok(EnginePlan {
                mode: format!("checkout: {}", dir.display()),
                program: node,
                args,
                cwd: dir,
            });
        }
        // No checkout: use the published package through npx.
        let cwd = self
            .app
            .path()
            .app_config_dir()
            .map_err(|e| format!("cannot resolve app config dir: {e}"))?;
        let mode = "npx @deepseek-ai/dsh (published package)".to_string();
        let common = vec![
            "web".to_string(),
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            port.to_string(),
        ];
        #[cfg(target_os = "windows")]
        {
            let mut args = vec!["/C".to_string(), "npx".to_string(), "--yes".to_string(), "@deepseek-ai/dsh".to_string()];
            args.extend(common);
            Ok(EnginePlan { mode, program: "cmd".to_string(), args, cwd })
        }
        #[cfg(not(target_os = "windows"))]
        {
            let mut args = vec!["--yes".to_string(), "@deepseek-ai/dsh".to_string()];
            args.extend(common);
            Ok(EnginePlan { mode, program: "npx".to_string(), args, cwd })
        }
    }

    /// Engine source resolution order: shell setting, DSH_ENGINE_DIR, then a
    /// `deepseek-harness` checkout next to the app (dev convenience).
    fn engine_dir(&self) -> Option<PathBuf> {
        let settings = self.settings.lock().unwrap().clone();
        settings
            .engine_dir
            .clone()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var("DSH_ENGINE_DIR").ok().filter(|s| !s.trim().is_empty()).map(PathBuf::from))
            .or_else(find_dev_checkout)
    }
    fn spawn(self: &Arc<Self>, plan: EnginePlan, port: u16) -> Result<(), String> {
        let mut command = Command::new(&plan.program);
        command
            .args(&plan.args)
            .current_dir(&plan.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("NO_COLOR", "1");
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().map_err(|e| format!("failed to start engine ({}): {e}", plan.program))?;
        let pid = child.id();
        #[cfg(target_os = "windows")]
        self.assign_to_job(pid);
        let stdout = child.stdout.take().ok_or("engine stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("engine stderr unavailable")?;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        *self.child.lock().unwrap() = Some(child);
        self.transition(|inner| {
            inner.phase = Phase::Starting;
            inner.url = None;
            inner.error = None;
            inner.exit_code = None;
            inner.port = Some(port);
            inner.mode = Some(plan.mode.clone());
            inner.setup_needed = false;
            inner.setup_active = false;
        });
        self.push_log(format!(
            "[dsh-desktop] starting engine (pid {pid}, port {port}): {} (cwd {})",
            plan.mode,
            plan.cwd.display()
        ));
        self.spawn_reader(stdout, generation, port);
        self.spawn_reader(stderr, generation, port);
        self.spawn_health(port, generation);
        self.spawn_waiter(generation);
        Ok(())
    }

    fn spawn_reader(self: &Arc<Self>, stream: impl Read + Send + 'static, generation: u64, port: u16) {
        let this = Arc::clone(self);
        thread::spawn(move || {
            let reader = BufReader::new(stream);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                this.push_log(line.clone());
                if let Some(url) = extract_local_url(&line, port) {
                    this.mark_ready(url, generation);
                }
            }
        });
    }

    fn spawn_health(self: &Arc<Self>, port: u16, generation: u64) {
        let this = Arc::clone(self);
        thread::spawn(move || {
            let deadline = Instant::now() + READY_TIMEOUT;
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            while Instant::now() < deadline {
                if generation != this.generation.load(Ordering::SeqCst) {
                    return;
                }
                if TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok() {
                    this.mark_ready(format!("http://127.0.0.1:{port}"), generation);
                    return;
                }
                thread::sleep(Duration::from_millis(400));
            }
        });
    }

    fn spawn_waiter(self: &Arc<Self>, generation: u64) {
        let this = Arc::clone(self);
        thread::spawn(move || loop {
            if generation != this.generation.load(Ordering::SeqCst) {
                return;
            }
            let status = {
                let mut guard = this.child.lock().unwrap();
                match guard.as_mut() {
                    None => return,
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => {
                            *guard = None;
                            Some(status)
                        }
                        Ok(None) => None,
                        Err(_) => {
                            *guard = None;
                            None
                        }
                    },
                }
            };
            if let Some(status) = status {
                this.on_child_exit(generation, status.code());
                return;
            }
            thread::sleep(Duration::from_millis(500));
        });
    }

    fn mark_ready(&self, url: String, generation: u64) {
        if generation != self.generation.load(Ordering::SeqCst) {
            return;
        }
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.phase != Phase::Starting {
                return;
            }
            inner.phase = Phase::Ready;
            inner.url = Some(url.clone());
            inner.error = None;
        }
        self.debug_log(&format!("[dsh-desktop] engine ready at {url}"));
        let view = self.snapshot();
        let _ = self.app.emit(EVENT_STATE, view);
        self.navigate_to(&url);
    }

    fn on_child_exit(&self, generation: u64, exit_code: Option<i32>) {
        if generation != self.generation.load(Ordering::SeqCst) {
            return;
        }
        let back_to_splash = {
            let mut inner = self.inner.lock().unwrap();
            let result = match inner.phase {
                Phase::Ready => {
                    inner.phase = Phase::Stopped;
                    true
                }
                Phase::Starting => {
                    inner.phase = Phase::Error;
                    inner.error = Some(format!(
                        "The engine exited before the Web UI became ready (code {}). Check the log for details.",
                        exit_code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_string())
                    ));
                    true
                }
                _ => return,
            };
            inner.exit_code = exit_code;
            result
        };
        let view = self.snapshot();
        let _ = self.app.emit(EVENT_STATE, view);
        if back_to_splash {
            self.navigate_splash();
        }
    }

    /// Apply a state mutation, emit the state event, and move the webview
    /// across the Ready boundary (into the UI, or back to the shell).
    fn transition(&self, f: impl FnOnce(&mut EngineInner)) {
        let mut navigate_to: Option<String> = None;
        let mut back_to_splash = false;
        {
            let mut inner = self.inner.lock().unwrap();
            let was_ready = inner.phase == Phase::Ready;
            f(&mut inner);
            let is_ready = inner.phase == Phase::Ready;
            if is_ready && !was_ready {
                navigate_to = inner.url.clone();
            } else if was_ready && !is_ready {
                back_to_splash = true;
            }
        }
        let view = self.snapshot();
        let _ = self.app.emit(EVENT_STATE, view);
        if let Some(url) = navigate_to {
            self.navigate_to(&url);
        }
        if back_to_splash {
            self.navigate_splash();
        }
    }

    fn navigate_to(&self, url: &str) {
        self.debug_log(&format!("[dsh-desktop] navigating webview to {url}"));
        let Ok(url) = Url::parse(url) else { return };
        if let Some(window) = self.app.get_webview_window("main") {
            match window.navigate(url) {
                Ok(()) => self.debug_log("[dsh-desktop] webview navigate ok"),
                Err(e) => self.debug_log(&format!("[dsh-desktop] webview navigate error: {e}")),
            }
        } else {
            self.debug_log("[dsh-desktop] main window not found for navigation");
        }
    }

    fn navigate_splash(&self) {
        let url = self.splash_url.lock().unwrap().clone();
        if let Some(url) = url {
            if let Some(window) = self.app.get_webview_window("main") {
                let _ = window.navigate(url);
            }
        }
    }
}

fn view_of(inner: &EngineInner, log_len: usize) -> EngineStateView {
    EngineStateView {
        phase: inner.phase.clone(),
        url: inner.url.clone(),
        port: inner.port,
        error: inner.error.clone(),
        exit_code: inner.exit_code,
        mode: inner.mode.clone(),
        setup_needed: inner.setup_needed,
        setup_active: inner.setup_active,
        log_len,
    }
}

fn log_reader(stream: impl Read + Send + 'static, engine: Arc<EngineManager>) {
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        engine.push_log(line);
    }
}

fn make_pnpm_command(cwd: &Path, args: &[&str]) -> Command {
    #[cfg(target_os = "windows")]
    {
        let mut command = Command::new("cmd");
        command.arg("/C").arg("pnpm");
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("NO_COLOR", "1");
        command
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut command = Command::new("pnpm");
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("NO_COLOR", "1");
        command
    }
}

fn is_checkout(dir: &Path) -> bool {
    dir.join("apps").join("cli").join("src").join("bin.ts").is_file() && dir.join("package.json").is_file()
}

/// Dev convenience: a `deepseek-harness` checkout beside the app or its
/// binary (works for `tauri dev` from the project root).
fn find_dev_checkout() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("deepseek-harness"));
        candidates.push(cwd.join("..").join("deepseek-harness"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("deepseek-harness"));
            candidates.push(parent.join("..").join("deepseek-harness"));
            candidates.push(parent.join("..").join("..").join("deepseek-harness"));
            candidates.push(parent.join("..").join("..").join("..").join("deepseek-harness"));
        }
    }
    candidates.into_iter().find(|d| is_checkout(d))
}

fn resolve_node() -> Result<String, String> {
    let probe = std::env::var("DSH_NODE_BIN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "node".to_string());
    match Command::new(&probe).arg("--version").output() {
        Ok(out) if out.status.success() => Ok(probe),
        Ok(_) => Err(format!("node check failed for {probe}")),
        Err(e) => Err(format!(
            "Node.js not found ({probe}: {e}). Install Node.js 22+ and ensure it is on PATH, or set DSH_NODE_BIN."
        )),
    }
}

fn pick_free_port() -> Result<u16, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("cannot reserve a port: {e}"))?;
    let port = listener.local_addr().map_err(|e| format!("cannot read reserved port: {e}"))?.port();
    drop(listener);
    Ok(port)
}

/// Match the engine's URL line, e.g. `Web UI served at http://127.0.0.1:38123`,
/// but only for the port this run reserved.
fn extract_local_url(line: &str, expected_port: u16) -> Option<String> {
    const PREFIX: &str = "http://127.0.0.1:";
    let start = line.find(PREFIX)?;
    let after = &line[start + PREFIX.len()..];
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    let port: u16 = digits.parse().ok()?;
    if port != expected_port {
        return None;
    }
    Some(format!("http://127.0.0.1:{port}"))
}

/// Drop ANSI SGR/OSC sequences so the log panel renders plain text.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() || c == '~' || c == '\u{1b}' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    for c in chars.by_ref() {
                        if c == '\u{7}' || c == '\u{1b}' {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(target_os = "windows")]
fn kill_tree(pid: u32) -> std::io::Result<()> {
    let status = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).status()?;
    let _ = status;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn kill_tree(pid: u32) -> std::io::Result<()> {
    let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
    Ok(())
}

/// Windows job object that kills every assigned process tree when the app
/// dies (even a hard kill), so a crashed shell never orphans the engine.
#[cfg(target_os = "windows")]
mod job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    pub struct KillOnCloseJob(HANDLE);

    // SAFETY: the handle is only touched under the engine's job mutex and in
    // Drop at teardown; sharing the opaque handle across threads is safe.
    unsafe impl Send for KillOnCloseJob {}

    impl KillOnCloseJob {
        pub fn new() -> Option<Self> {
            // SAFETY: raw Win32 handle creation; NULL means failure.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return None;
            }
            unsafe {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let _ = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
            }
            Some(Self(handle))
        }

        pub fn assign(&self, pid: u32) -> bool {
            // SAFETY: raw Win32 calls; the process handle is closed in all paths.
            unsafe {
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if process.is_null() {
                    return false;
                }
                let ok = AssignProcessToJobObject(self.0, process);
                CloseHandle(process);
                ok != 0
            }
        }
    }

    impl Drop for KillOnCloseJob {
        fn drop(&mut self) {
            // SAFETY: closing a handle we own.
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_local_url_matches_url_line() {
        assert_eq!(
            extract_local_url("dsh web: http://127.0.0.1:61653", 61653),
            Some("http://127.0.0.1:61653".to_string())
        );
        assert_eq!(
            extract_local_url("Web UI served at http://127.0.0.1:8080.", 8080),
            Some("http://127.0.0.1:8080".to_string())
        );
    }

    #[test]
    fn extract_local_url_ignores_wrong_port() {
        assert_eq!(extract_local_url("dsh web: http://127.0.0.1:61653", 9999), None);
    }

    #[test]
    fn extract_local_url_rejects_non_local_or_missing() {
        assert_eq!(extract_local_url("http://localhost:8080", 8080), None);
        assert_eq!(extract_local_url("no url here", 8080), None);
        assert_eq!(extract_local_url("http://127.0.0.1:", 8080), None);
        assert_eq!(extract_local_url("", 8080), None);
    }

    #[test]
    fn strip_ansi_removes_sgr_and_osc() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("plain text"), "plain text");
        assert_eq!(strip_ansi("\u{1b}]0;window title\u{7}body"), "body");
        assert_eq!(strip_ansi("\u{1b}[2J"), "");
        assert_eq!(strip_ansi("a\u{1b}[1;32mb\u{1b}[0m"), "ab");
    }

    #[test]
    fn is_checkout_requires_cli_sources() {
        let dir = std::env::temp_dir().join("dsh-desktop-test-checkout");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("apps").join("cli").join("src")).unwrap();
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        assert!(!is_checkout(&dir));
        std::fs::write(dir.join("apps").join("cli").join("src").join("bin.ts"), "export {}").unwrap();
        assert!(is_checkout(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_serde_roundtrip() {
        let original = crate::settings::Settings { engine_dir: Some("C:\\harness".to_string()) };
        let json = serde_json::to_string(&original).unwrap();
        let back: crate::settings::Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.engine_dir, original.engine_dir);
        let none: crate::settings::Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(none.engine_dir, None);
    }
}
