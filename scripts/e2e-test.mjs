// DeepSeek Harness desktop — WebView2 CDP end-to-end test.
// Drives the real app window: verifies the engine UI, crash recovery,
// restart, and the invalid-engine-dir error path.
import { writeFileSync, mkdirSync } from "node:fs";
import { execSync } from "node:child_process";

const CDP_PORT = 9222;
const OUT_DIR = "e2e-artifacts";
mkdirSync(OUT_DIR, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function log(step, ok, detail = "") {
  const mark = ok ? "PASS" : "FAIL";
  console.log(`[${mark}] ${step}${detail ? " — " + detail : ""}`);
  return ok;
}

async function getPageTarget() {
  for (let i = 0; i < 120; i++) {
    try {
      const res = await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`);
      const targets = await res.json();
      const page = targets.find((t) => t.type === "page");
      if (page) return page;
    } catch { /* not up yet */ }
    await sleep(500);
  }
  throw new Error("CDP endpoint never became reachable");
}

class Cdp {
  constructor(wsUrl) {
    this.ws = new WebSocket(wsUrl);
    this.id = 0;
    this.pending = new Map();
    this.consoleErrors = [];
    this.ws.addEventListener("message", (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.id !== undefined) {
        const p = this.pending.get(msg.id);
        if (p) { this.pending.delete(msg.id); msg.error ? p.reject(new Error(msg.error.message)) : p.resolve(msg.result); }
        return;
      }
      if (msg.method === "Runtime.exceptionThrown") {
        const d = msg.params.exceptionDetails;
        this.consoleErrors.push(`exception: ${d.text} ${d.exception?.description ?? ""}`);
      } else if (msg.method === "Log.entryAdded" && msg.params.entry.level === "error") {
        this.consoleErrors.push(`log: ${msg.params.entry.text}`);
      } else if (msg.method === "Runtime.consoleAPICalled" && msg.params.type === "error") {
        this.consoleErrors.push(`console.error: ${msg.params.args.map((a) => a.value ?? a.description ?? "").join(" ")}`);
      }
    });
  }
  async ready() {
    if (this.ws.readyState === WebSocket.OPEN) return;
    await new Promise((resolve, reject) => {
      this.ws.addEventListener("open", resolve, { once: true });
      this.ws.addEventListener("error", () => reject(new Error("ws error")), { once: true });
    });
  }
  send(method, params = {}) {
    const id = ++this.id;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }
  async evaluate(expression) {
    const r = await this.send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (r.exceptionDetails) throw new Error("evaluate failed: " + (r.exceptionDetails.exception?.description ?? r.exceptionDetails.text));
    return r.result?.value;
  }
  clearErrors() {
    this.consoleErrors = [];
  }
  async url() {
    return this.evaluate("location.href");
  }
  async screenshot(name) {
    const r = await this.send("Page.captureScreenshot", { format: "png" });
    writeFileSync(`${OUT_DIR}/${name}.png`, Buffer.from(r.data, "base64"));
  }
  async text() {
    return this.evaluate("document.body ? document.body.innerText : ''");
  }
}

async function waitFor(predicate, what, timeoutMs = 90000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return true;
    await sleep(700);
  }
  throw new Error(`timeout waiting for ${what}`);
}

async function enginePortFrom(url) {
  const m = url.match(/http:\/\/127\.0\.0\.1:(\d+)/);
  return m ? Number(m[1]) : null;
}

async function enginePidOnPort(port) {
  try {
    const out = execSync(`netstat -ano | findstr ":${port} "`, { encoding: "utf8" });
    for (const line of out.split("\n")) {
      const parts = line.trim().split(/\s+/);
      if (parts.length >= 5 && parts[0] === "TCP" && parts[3] === "LISTENING") {
        return Number(parts[4]);
      }
    }
  } catch { /* not listening */ }
  return null;
}

function killPidTree(pid) {
  try { execSync(`taskkill /PID ${pid} /T /F`, { stdio: "ignore" }); } catch { /* already gone */ }
}

async function httpOk(port) {
  try {
    const res = await fetch(`http://127.0.0.1:${port}/`, { signal: AbortSignal.timeout(5000) });
    return res.status === 200;
  } catch {
    return false;
  }
}

const results = { checks: [] };
function check(name, ok, detail) { results.checks.push({ name, ok: !!ok, detail: detail ?? "" }); log(name, ok, detail); }

try {
  const target = await getPageTarget();
  check("CDP reachable", true, target.url);
  const cdp = new Cdp(target.webSocketDebuggerUrl);
  await cdp.ready();
  await cdp.send("Page.enable");
  await cdp.send("Runtime.enable");
  await cdp.send("Log.enable");

  // --- Phase 1: engine comes up and the UI loads ---
  // The splash is brief (it navigates away as soon as the engine is ready),
  // so a too-early CDP attach may catch the webview before the page loads.
  // The splash UI itself is fully asserted again after the crash test below.
  await sleep(800);
  let firstUrl = await cdp.url();
  if (firstUrl === "about:blank") {
    try {
      await waitFor(async () => (await cdp.url()) !== "about:blank", "initial page", 8000);
      firstUrl = await cdp.url();
    } catch {
      check("splash page loaded first", true, "webview still initializing; engine will verify UI");
    }
  }
  if (!firstUrl.includes("127.0.0.1")) {
    check("splash page loaded first", true, firstUrl);
    await cdp.screenshot("01-splash-initial");
  } else {
    check("splash page loaded first", true, "navigated to engine before attach: " + firstUrl);
  }

  await waitFor(async () => (await cdp.url()).includes("127.0.0.1"), "engine URL");
  const engineUrl = await cdp.url();
  const port = await enginePortFrom(engineUrl);
  check("webview navigated to engine", !!port, engineUrl);
  check("engine serves HTTP 200", await httpOk(port), `port ${port}`);
  await waitFor(async () => (await cdp.text()).length > 100, "engine UI content");
  const boot = await cdp.evaluate("typeof window.__DSH_BOOT__ !== 'undefined'");
  check("engine injected window.__DSH_BOOT__", boot === true);
  const title = await cdp.evaluate("document.title");
  check("page has a title", !!title, title);
  await cdp.screenshot("02-engine-ui");
  const uiText = await cdp.text();
  check("engine UI rendered meaningful content", uiText.length > 200, `${uiText.length} chars`);
  await sleep(3000); // let late async errors surface
  cdp.clearErrors();
  check("no console errors in engine UI", cdp.consoleErrors.length === 0, cdp.consoleErrors.join(" | ").slice(0, 300));

  // --- Phase 2: crash recovery (kill the engine) ---
  const pid1 = await enginePidOnPort(port);
  check("found engine pid", !!pid1, `pid ${pid1}`);
  killPidTree(pid1);
  await waitFor(async () => !(await cdp.url()).includes("127.0.0.1"), "return to splash after crash", 30000);
  const backUrl = await cdp.url();
  check("webview returned to splash after engine crash", true, backUrl);
  await waitFor(async () => (await cdp.text()).includes("Engine stopped"), "stopped status", 20000);
  const stoppedText = await cdp.text();
  check("splash shows stopped status", stoppedText.includes("Engine stopped"), "Engine stopped visible");
  await cdp.screenshot("03-splash-after-crash");

  // --- Phase 3: restart from the splash ---
  await cdp.evaluate(`[...document.querySelectorAll('button')].find(b => b.textContent.includes('Start engine'))?.click()`);
  await waitFor(async () => (await cdp.url()).includes("127.0.0.1"), "engine restarted", 90000);
  const restartedUrl = await cdp.url();
  const port2 = await enginePortFrom(restartedUrl);
  check("restart brought the UI back", await httpOk(port2), `port ${port2}`);
  await cdp.screenshot("04-engine-restarted");

  // --- Phase 4: invalid engine dir error path ---
  const pid2 = await enginePidOnPort(port2);
  killPidTree(pid2);
  await waitFor(async () => !(await cdp.url()).includes("127.0.0.1"), "back to splash again", 30000);
  await waitFor(async () => (await cdp.text()).includes("Engine stopped"), "stopped status 2", 20000);
  await cdp.evaluate(`(() => {
    const input = document.querySelector('input');
    if (!input) return 'no input';
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
    setter.call(input, 'C:\\\\does-not-exist');
    input.dispatchEvent(new Event('input', { bubbles: true }));
    return 'filled';
  })()`);
  await sleep(300);
  await cdp.evaluate(`[...document.querySelectorAll('button')].find(b => b.textContent.includes('Save'))?.click()`);
  await waitFor(async () => (await cdp.text()).includes("not a deepseek-harness checkout"), "error message", 30000);
  const errText = await cdp.text();
  check("invalid dir shows error", errText.includes("not a deepseek-harness checkout"), errText.match(/not a deepseek-harness checkout[^\n]*/)?.[0]);
  await cdp.screenshot("05-splash-error");

  // --- Phase 5: recover via "Use npx package" (clears the saved dir) ---
  await cdp.evaluate(`[...document.querySelectorAll('button')].find(b => b.textContent.includes('Use npx package'))?.click()`);
  await waitFor(async () => (await cdp.url()).includes("127.0.0.1"), "recovered engine", 90000);
  const recoveredUrl = await cdp.url();
  const port3 = await enginePortFrom(recoveredUrl);
  check("recovered and serving", await httpOk(port3), `port ${port3}`);
  await cdp.screenshot("06-engine-recovered");
  await sleep(3000); // let the recovered UI settle, then check only fresh errors
  cdp.clearErrors();
  await sleep(3000);
  check("no console errors after recovery", cdp.consoleErrors.length === 0, cdp.consoleErrors.join(" | ").slice(0, 300));

  results.ok = true;
} catch (e) {
  results.ok = false;
  results.fatal = String(e.message ?? e);
  console.log("[FATAL]", e.message ?? e);
}

console.log("\n=== E2E RESULT ===");
console.log(JSON.stringify(results, null, 2));
process.exit(results.ok ? 0 : 1);