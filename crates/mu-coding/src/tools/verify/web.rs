//! The web probe (mu-lg8j1 phase 2): boot the artifact in headless
//! Chrome, collect what a real runtime says about it, exercise it, and
//! render a text report. A port of the battery-1 grading rig's
//! `t3-probe.py` onto [`super::cdp`]:
//!
//! - **boot**: page load event, uncaught exceptions with locations,
//!   `console.error`/`console.log`, network/log errors;
//! - **raf**: `requestAnimationFrame` ticks per second;
//! - **render**: two screenshots a second apart — non-blank, and
//!   changing (animation). Screenshots are compared as PNG hashes; WebGL
//!   canvases without `preserveDrawingBuffer` read back empty in-page,
//!   the compositor output is the truth;
//! - **input**: scripted clicks/keys, then a third screenshot — did
//!   pixels respond, did anything throw.
//!
//! Every step runs against one deadline; hitting it ends the probe with
//! whatever was collected and a note saying where it stopped.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::cdp::{Cdp, Launcher};
use super::http::StaticServer;
use super::{probe_chrome, VerifySettings};

/// Ceiling on one key hold / wait so a script can't burn the timeout.
pub const MAX_HOLD_MS: u64 = 5_000;
pub const MAX_WAIT_MS: u64 = 10_000;
const MAX_LOG_LINES: usize = 40;
const MAX_LINE_CHARS: usize = 300;
/// PNG size below which a screenshot is called blank (blank pages
/// compress to a few KB; anything drawn is well past this).
const BLANK_PNG_BYTES: usize = 8_000;

#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Click { x: f64, y: f64 },
    Key { key: String, hold_ms: u64 },
    Wait { ms: u64 },
}

impl InputEvent {
    pub fn parse(v: &Value) -> Result<Self, String> {
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| "verify: input_events item needs a string 'type'".to_string())?;
        match kind {
            "click" => {
                let x = v.get("x").and_then(Value::as_f64).unwrap_or(640.0);
                let y = v.get("y").and_then(Value::as_f64).unwrap_or(400.0);
                Ok(InputEvent::Click { x, y })
            }
            "key" => {
                let key = v
                    .get("key")
                    .and_then(Value::as_str)
                    .filter(|k| !k.is_empty())
                    .ok_or_else(|| {
                        "verify: key event needs 'key' (e.g. \"w\", \" \", \"ArrowUp\")".to_string()
                    })?
                    .to_string();
                let hold_ms = v
                    .get("hold_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(120)
                    .min(MAX_HOLD_MS);
                Ok(InputEvent::Key { key, hold_ms })
            }
            "wait" => Ok(InputEvent::Wait {
                ms: v
                    .get("ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(500)
                    .min(MAX_WAIT_MS),
            }),
            other => Err(format!(
                "verify: unknown input event type '{other}' (click, key, wait)"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WebOptions {
    pub timeout: Duration,
    pub settle: Duration,
    pub input_events: Vec<InputEvent>,
}

#[derive(Debug, Clone)]
pub struct Shot {
    pub path: PathBuf,
    pub hash: String,
    pub bytes: usize,
}

impl Shot {
    pub fn nonblank(&self) -> bool {
        self.bytes > BLANK_PNG_BYTES
    }
}

#[derive(Debug, Default, Clone)]
pub struct WebReport {
    pub artifact: PathBuf,
    pub url: String,
    pub backend: String,
    pub loaded_after: Option<Duration>,
    pub crashed: bool,
    pub exceptions: Vec<String>,
    pub console_errors: Vec<String>,
    pub console_logs: Vec<String>,
    /// Exceptions counted at the end of the boot/settle phase.
    pub boot_exception_count: usize,
    pub raf_frames_per_s: Option<u64>,
    pub shot_a: Option<Shot>,
    pub shot_b: Option<Shot>,
    pub shot_after_input: Option<Shot>,
    pub input_dispatched: usize,
    pub input_errors: Vec<String>,
    pub stopped_at: Option<String>,
    pub notes: Vec<String>,
    pub elapsed: Duration,
}

impl WebReport {
    pub fn passed(&self) -> bool {
        self.loaded_after.is_some()
            && self.exceptions.is_empty()
            && !self.crashed
            && self.stopped_at.is_none()
    }
    pub fn animating(&self) -> Option<bool> {
        match (&self.shot_a, &self.shot_b) {
            (Some(a), Some(b)) => Some(a.hash != b.hash),
            _ => None,
        }
    }
    pub fn responded_to_input(&self) -> Option<bool> {
        match (&self.shot_b, &self.shot_after_input) {
            (Some(b), Some(c)) => Some(b.hash != c.hash),
            _ => None,
        }
    }
}

struct Deadline(Instant);

impl Deadline {
    fn remaining(&self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }
    async fn run<T>(
        &self,
        step: &str,
        fut: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> Result<T, Stop> {
        match tokio::time::timeout(self.remaining(), fut).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(Stop::Failed(format!("{step}: {e:#}"))),
            Err(_) => Err(Stop::Timeout(step.to_string())),
        }
    }
}

#[derive(Debug)]
enum Stop {
    Timeout(String),
    Failed(String),
}

/// Run the probe. Errors are reserved for "could not even start"
/// (no Chrome, artifact dir unservable); everything after launch lands in
/// the report, including a timeout.
pub async fn probe(
    settings: &VerifySettings,
    artifact: &Path,
    opts: WebOptions,
) -> anyhow::Result<WebReport> {
    let start = Instant::now();
    let chrome = match (&settings.chrome, &settings.chrome_ssh) {
        (Some(c), _) => c.clone(),
        (None, Some(host)) => anyhow::bail!(
            "[verify].chrome_ssh = \"{host}\" needs [verify].chrome set to the Chrome path on that host"
        ),
        (None, None) => probe_chrome().ok_or_else(|| {
            anyhow::anyhow!(
                "no Chrome found — set [verify].chrome (or $CHROME), or [verify].chrome_ssh + chrome for a remote one"
            )
        })?,
    };
    let dir = artifact
        .parent()
        .ok_or_else(|| anyhow::anyhow!("artifact has no parent directory"))?;
    let file_name = artifact
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("artifact has no usable file name"))?;
    let server = StaticServer::serve_dir(dir).await?;
    let url = server.url_for(file_name);
    tokio::fs::create_dir_all(&settings.screenshot_dir)
        .await
        .ok();

    let launcher = Launcher {
        chrome,
        ssh: settings.chrome_ssh.clone(),
        extra_args: settings.chrome_args.clone(),
        forward_port: settings.chrome_ssh.as_ref().map(|_| server.port()),
    };
    let mut report = WebReport {
        artifact: artifact.to_path_buf(),
        url: url.clone(),
        backend: match &settings.chrome_ssh {
            Some(h) => format!("chrome via ssh {h}"),
            None => "local chrome".to_string(),
        },
        ..Default::default()
    };
    let deadline = Deadline(start + opts.timeout);
    let mut cdp = Cdp::launch(&launcher).await?;
    let outcome = drive(
        &mut cdp,
        &deadline,
        &opts,
        settings,
        file_name,
        &url,
        &mut report,
    )
    .await;
    match outcome {
        Ok(()) => {}
        Err(Stop::Timeout(step)) => {
            report.stopped_at = Some(format!(
                "timed out ({}s) during {step}",
                opts.timeout.as_secs()
            ))
        }
        Err(Stop::Failed(what)) => {
            let note = if report.loaded_after.is_none() {
                let stderr = cdp.launcher_stderr();
                if stderr.is_empty() {
                    what
                } else {
                    format!(
                        "{what}; launcher stderr: {}",
                        stderr.lines().take(5).collect::<Vec<_>>().join(" | ")
                    )
                }
            } else {
                what
            };
            report.stopped_at = Some(note);
        }
    }
    let dropped = cdp.dropped_events();
    if dropped > 0 {
        report.notes.push(format!(
            "{dropped} console/log event(s) dropped: the page emitted more than {} while the probe was busy",
            super::cdp::EVENT_QUEUE_CAP
        ));
    }
    cdp.close().await;
    drop(server);
    report.elapsed = start.elapsed();
    Ok(report)
}

async fn drive(
    cdp: &mut Cdp,
    deadline: &Deadline,
    opts: &WebOptions,
    settings: &VerifySettings,
    file_name: &str,
    url: &str,
    report: &mut WebReport,
) -> Result<(), Stop> {
    let session = deadline.run("open page", cdp.open_page()).await?;
    let s = session.as_str();
    for method in ["Page.enable", "Runtime.enable", "Log.enable"] {
        deadline
            .run(method, cdp.call(Some(s), method, json!({})))
            .await?;
    }
    let nav_start = Instant::now();
    deadline
        .run(
            "Page.navigate",
            cdp.call(Some(s), "Page.navigate", json!({"url": url})),
        )
        .await?;
    // Wait for the load event (absorbing everything that arrives), then
    // the settle window (still absorbing).
    let load_wait = deadline.remaining().min(Duration::from_secs(60));
    let load_deadline = Instant::now() + load_wait;
    while report.loaded_after.is_none() && Instant::now() < load_deadline {
        let step = load_deadline.saturating_duration_since(Instant::now());
        match cdp.next_event(step).await {
            Some(ev) => {
                if absorb(report, &ev, nav_start) && report.loaded_after.is_none() {
                    report.loaded_after = Some(nav_start.elapsed());
                }
            }
            None => break,
        }
    }
    if report.loaded_after.is_none() {
        report.notes.push(format!(
            "load event not fired within {}s",
            load_wait.as_secs()
        ));
        if deadline.remaining().is_zero() {
            return Err(Stop::Timeout("waiting for load".into()));
        }
    }
    let settle_end = Instant::now() + opts.settle.min(deadline.remaining());
    while Instant::now() < settle_end {
        let step = settle_end.saturating_duration_since(Instant::now());
        match cdp.next_event(step).await {
            Some(ev) => {
                absorb(report, &ev, nav_start);
            }
            None => break,
        }
    }
    report.boot_exception_count = report.exceptions.len();
    if report.crashed {
        return Ok(());
    }

    // rAF: frames in one second (guarded so a page that never ticks
    // still answers).
    let raf_expr = "new Promise(r=>{let n=0;const t0=performance.now();function f(){n++;if(performance.now()-t0<1000)requestAnimationFrame(f);else r(n)}requestAnimationFrame(f);setTimeout(()=>r(n),2500)})";
    let raf = deadline
        .run(
            "rAF probe",
            cdp.call(
                Some(s),
                "Runtime.evaluate",
                json!({"expression": raf_expr, "awaitPromise": true, "returnByValue": true}),
            ),
        )
        .await?;
    report.raf_frames_per_s = raf.pointer("/result/value").and_then(Value::as_u64);
    for ev in cdp.drain_events() {
        absorb(report, &ev, nav_start);
    }

    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("artifact")
        .to_string();
    let tag = format!(
        "{}-{}-{:04x}",
        stem,
        chrono_like_stamp(),
        rand::random::<u16>()
    );
    // Screenshots are auxiliary evidence: a capture/write failure is a
    // probe-side problem and must not turn a cleanly loaded page into a
    // FAIL. Record it as a note and carry on.
    let shot = deadline
        .run(
            "screenshot A",
            screenshot(cdp, s, &settings.screenshot_dir, &format!("{tag}-a")),
        )
        .await;
    report.shot_a = shot_or_note(report, "A", shot)?;
    tokio::time::sleep(Duration::from_secs(1).min(deadline.remaining())).await;
    let shot = deadline
        .run(
            "screenshot B",
            screenshot(cdp, s, &settings.screenshot_dir, &format!("{tag}-b")),
        )
        .await;
    report.shot_b = shot_or_note(report, "B", shot)?;
    for ev in cdp.drain_events() {
        absorb(report, &ev, nav_start);
    }

    if !opts.input_events.is_empty() {
        let before = report.exceptions.len();
        for ev in &opts.input_events {
            deadline.run("input", dispatch(cdp, s, ev)).await?;
            report.input_dispatched += 1;
            for e in cdp.drain_events() {
                absorb(report, &e, nav_start);
            }
        }
        tokio::time::sleep(Duration::from_millis(500).min(deadline.remaining())).await;
        for e in cdp.drain_events() {
            absorb(report, &e, nav_start);
        }
        report.input_errors = report.exceptions[before..].to_vec();
        let shot = deadline
            .run(
                "screenshot after input",
                screenshot(cdp, s, &settings.screenshot_dir, &format!("{tag}-input")),
            )
            .await;
        report.shot_after_input = shot_or_note(report, "after input", shot)?;
    }
    Ok(())
}

/// A screenshot failure becomes a note, never a verdict; only the
/// deadline itself still stops the probe.
fn shot_or_note(
    report: &mut WebReport,
    which: &str,
    outcome: Result<Shot, Stop>,
) -> Result<Option<Shot>, Stop> {
    match outcome {
        Ok(shot) => Ok(Some(shot)),
        Err(Stop::Failed(what)) => {
            report
                .notes
                .push(format!("screenshot {which} not captured: {what}"));
            Ok(None)
        }
        Err(stop @ Stop::Timeout(_)) => Err(stop),
    }
}

/// Fold one CDP event into the report. Returns true for the load event.
fn absorb(report: &mut WebReport, ev: &Value, nav_start: Instant) -> bool {
    let method = ev.get("method").and_then(Value::as_str).unwrap_or("");
    let p = ev.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "Page.loadEventFired" => return true,
        "Runtime.exceptionThrown" => {
            let d = &p["exceptionDetails"];
            let text = d["text"].as_str().unwrap_or("Uncaught");
            let desc = d["exception"]["description"]
                .as_str()
                .or_else(|| d["exception"]["value"].as_str())
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("");
            let (url, line, col) = location(d);
            let mut s = if desc.is_empty() {
                text.to_string()
            } else {
                format!("{text} {desc}")
            };
            if !url.is_empty() {
                s.push_str(&format!(" @ {}:{}:{}", short_url(&url), line, col));
            }
            push_capped(&mut report.exceptions, clip(&s));
        }
        "Runtime.consoleAPICalled" => {
            let kind = p["type"].as_str().unwrap_or("log");
            let args: Vec<String> = p["args"]
                .as_array()
                .map(|a| a.iter().map(arg_text).collect())
                .unwrap_or_default();
            let line = clip(&args.join(" "));
            match kind {
                "error" | "assert" => push_capped(&mut report.console_errors, line),
                "warning" => push_capped(&mut report.console_logs, format!("[warn] {line}")),
                _ => push_capped(&mut report.console_logs, line),
            }
        }
        "Log.entryAdded" => {
            let e = &p["entry"];
            if e["level"].as_str() == Some("error") {
                let src = e["source"].as_str().unwrap_or("log");
                let text = e["text"].as_str().unwrap_or("");
                let url = e["url"].as_str().map(short_url).unwrap_or_default();
                // Every page without a favicon logs this; it is never the bug.
                if url == "favicon.ico" {
                    return false;
                }
                push_capped(
                    &mut report.console_errors,
                    clip(format!("[{src}] {text} {url}").trim()),
                );
            }
        }
        "Inspector.targetCrashed" => {
            report.crashed = true;
            report.notes.push(format!(
                "renderer crashed {:.1}s after navigation",
                nav_start.elapsed().as_secs_f64()
            ));
        }
        _ => {}
    }
    false
}

fn location(d: &Value) -> (String, u64, u64) {
    let from_details = (
        d["url"].as_str().unwrap_or("").to_string(),
        d["lineNumber"].as_u64(),
        d["columnNumber"].as_u64(),
    );
    if !from_details.0.is_empty() {
        return (
            from_details.0,
            from_details.1.map(|l| l + 1).unwrap_or(0),
            from_details.2.map(|c| c + 1).unwrap_or(0),
        );
    }
    let frame = &d["stackTrace"]["callFrames"][0];
    (
        frame["url"].as_str().unwrap_or("").to_string(),
        frame["lineNumber"].as_u64().map(|l| l + 1).unwrap_or(0),
        frame["columnNumber"].as_u64().map(|c| c + 1).unwrap_or(0),
    )
}

fn arg_text(a: &Value) -> String {
    if let Some(v) = a.get("value") {
        match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    } else if let Some(d) = a.get("description").and_then(Value::as_str) {
        d.lines().next().unwrap_or("").to_string()
    } else {
        a.get("type")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string()
    }
}

fn short_url(u: &str) -> String {
    u.rsplit('/').next().unwrap_or(u).to_string()
}

fn clip(s: &str) -> String {
    if s.chars().count() <= MAX_LINE_CHARS {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX_LINE_CHARS).collect();
        out.push('…');
        out
    }
}

/// Every collected vector is bounded: the report renders at most
/// `MAX_LOG_LINES` of each, and an untrusted page throwing or logging in
/// a loop for the whole settle window must not grow memory past this.
fn push_capped(v: &mut Vec<String>, line: String) {
    if v.len() < MAX_LOG_LINES * 4 {
        v.push(line);
    }
}

fn chrono_like_stamp() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{ms}")
}

async fn screenshot(cdp: &mut Cdp, session: &str, dir: &Path, name: &str) -> anyhow::Result<Shot> {
    let r = cdp
        .call(
            Some(session),
            "Page.captureScreenshot",
            json!({"format": "png"}),
        )
        .await?;
    let data = r["data"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("captureScreenshot returned no data"))?;
    let png = base64::engine::general_purpose::STANDARD.decode(data)?;
    let hash = format!("{:x}", Sha256::digest(&png))[..16].to_string();
    let path = dir.join(format!("{name}.png"));
    tokio::fs::write(&path, &png).await?;
    Ok(Shot {
        path,
        hash,
        bytes: png.len(),
    })
}

async fn dispatch(cdp: &mut Cdp, session: &str, ev: &InputEvent) -> anyhow::Result<()> {
    match ev {
        InputEvent::Click { x, y } => {
            for (kind, button, clicks) in [
                ("mouseMoved", "none", 0),
                ("mousePressed", "left", 1),
                ("mouseReleased", "left", 1),
            ] {
                cdp.call(
                    Some(session),
                    "Input.dispatchMouseEvent",
                    json!({"type": kind, "x": x, "y": y, "button": button, "clickCount": clicks}),
                )
                .await?;
            }
        }
        InputEvent::Key { key, hold_ms } => {
            let (code, text, vk) = key_descriptor(key);
            let mut down =
                json!({"type": "keyDown", "key": key, "code": code, "windowsVirtualKeyCode": vk});
            if let Some(t) = &text {
                down["text"] = Value::String(t.clone());
            }
            cdp.call(Some(session), "Input.dispatchKeyEvent", down)
                .await?;
            tokio::time::sleep(Duration::from_millis(*hold_ms)).await;
            cdp.call(
                Some(session),
                "Input.dispatchKeyEvent",
                json!({"type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": vk}),
            )
            .await?;
        }
        InputEvent::Wait { ms } => tokio::time::sleep(Duration::from_millis(*ms)).await,
    }
    Ok(())
}

/// (code, text, windowsVirtualKeyCode) for the keys a game script uses.
pub fn key_descriptor(key: &str) -> (String, Option<String>, u64) {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            let up = c.to_ascii_uppercase();
            return (format!("Key{up}"), Some(c.to_string()), up as u64);
        }
        if c.is_ascii_digit() {
            return (format!("Digit{c}"), Some(c.to_string()), c as u64);
        }
        if c == ' ' {
            return ("Space".into(), Some(" ".into()), 32);
        }
    }
    match key {
        "ArrowUp" => ("ArrowUp".into(), None, 38),
        "ArrowDown" => ("ArrowDown".into(), None, 40),
        "ArrowLeft" => ("ArrowLeft".into(), None, 37),
        "ArrowRight" => ("ArrowRight".into(), None, 39),
        "Enter" => ("Enter".into(), Some("\r".into()), 13),
        "Escape" => ("Escape".into(), None, 27),
        "Shift" => ("ShiftLeft".into(), None, 16),
        "Tab" => ("Tab".into(), None, 9),
        other => (other.to_string(), None, 0),
    }
}

/// The model-facing report. First line is the verdict.
pub fn format_report(r: &WebReport) -> String {
    let verdict = if r.passed() { "PASS" } else { "FAIL" };
    let mut out = format!("VERIFY web {verdict} — {}\n", r.artifact.display());
    out.push_str(&format!(
        "url: {}  backend: {}  elapsed: {:.1}s\n",
        r.url,
        r.backend,
        r.elapsed.as_secs_f64()
    ));
    match r.loaded_after {
        Some(d) => out.push_str(&format!("boot: load event after {:.2}s\n", d.as_secs_f64())),
        None => out.push_str("boot: LOAD EVENT NOT FIRED\n"),
    }
    if r.crashed {
        out.push_str("renderer: CRASHED\n");
    }
    if let Some(stop) = &r.stopped_at {
        out.push_str(&format!("probe stopped: {stop}\n"));
    }
    let boot_exc = &r.exceptions[..r.boot_exception_count.min(r.exceptions.len())];
    out.push_str(&format!(
        "uncaught exceptions during boot/settle: {}\n",
        boot_exc.len()
    ));
    for (i, e) in boot_exc.iter().take(MAX_LOG_LINES).enumerate() {
        out.push_str(&format!("  {}. {e}\n", i + 1));
    }
    let later = r
        .exceptions
        .len()
        .saturating_sub(r.boot_exception_count)
        .saturating_sub(r.input_errors.len());
    if later > 0 {
        out.push_str(&format!(
            "uncaught exceptions after settle (before input): {later}\n"
        ));
        for e in r.exceptions[r.boot_exception_count..r.exceptions.len() - r.input_errors.len()]
            .iter()
            .take(MAX_LOG_LINES)
        {
            out.push_str(&format!("  - {e}\n"));
        }
    }
    out.push_str(&format!("console.error: {}\n", r.console_errors.len()));
    for e in r.console_errors.iter().take(MAX_LOG_LINES) {
        out.push_str(&format!("  - {e}\n"));
    }
    out.push_str(&format!("console.log: {} line(s)\n", r.console_logs.len()));
    for l in r.console_logs.iter().take(MAX_LOG_LINES) {
        out.push_str(&format!("  | {l}\n"));
    }
    match r.raf_frames_per_s {
        Some(n) => out.push_str(&format!(
            "requestAnimationFrame: {n} frame(s) in 1s ({})\n",
            if n > 0 { "ticking" } else { "NOT ticking" }
        )),
        None => out.push_str("requestAnimationFrame: not measured\n"),
    }
    if let Some(a) = &r.shot_a {
        out.push_str(&format!(
            "render: {}; animating (two samples 1s apart differ): {}\n",
            if a.nonblank() {
                format!("screenshot {} bytes (content drawn)", a.bytes)
            } else {
                format!(
                    "screenshot only {} bytes — mostly blank page (fine for a small canvas, \
                     a bug for a full-window game)",
                    a.bytes
                )
            },
            r.animating()
                .map(|b| if b { "yes" } else { "no" })
                .unwrap_or("?")
        ));
    }
    if r.input_dispatched > 0 {
        out.push_str(&format!(
            "input: {} event(s) dispatched; pixels changed after input: {}; new uncaught exceptions during input: {}\n",
            r.input_dispatched,
            r.responded_to_input().map(|b| if b { "yes" } else { "no" }).unwrap_or("?"),
            r.input_errors.len()
        ));
        for e in &r.input_errors {
            out.push_str(&format!("  - {e}\n"));
        }
    }
    let shots: Vec<String> = [&r.shot_a, &r.shot_b, &r.shot_after_input]
        .iter()
        .filter_map(|s| s.as_ref().map(|s| s.path.display().to_string()))
        .collect();
    if !shots.is_empty() {
        out.push_str(&format!("screenshots: {}\n", shots.join(" ")));
    }
    for n in &r.notes {
        out.push_str(&format!("note: {n}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_chrome(dir: &Path) -> PathBuf {
        let p = dir.join("fake-chrome");
        std::fs::write(&p, super::super::cdp::fake_chrome_script()).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn input_event_parsing_and_key_descriptors() {
        assert_eq!(
            InputEvent::parse(&json!({"type":"click"})).unwrap(),
            InputEvent::Click { x: 640.0, y: 400.0 }
        );
        assert_eq!(
            InputEvent::parse(&json!({"type":"key","key":"w","hold_ms":99999})).unwrap(),
            InputEvent::Key {
                key: "w".into(),
                hold_ms: MAX_HOLD_MS
            }
        );
        assert!(InputEvent::parse(&json!({"type":"key"})).is_err());
        assert!(InputEvent::parse(&json!({"type":"nope"})).is_err());
        assert_eq!(key_descriptor("w"), ("KeyW".into(), Some("w".into()), 87));
        assert_eq!(key_descriptor(" "), ("Space".into(), Some(" ".into()), 32));
        assert_eq!(key_descriptor("ArrowUp").0, "ArrowUp");
        assert_eq!(key_descriptor("F5"), ("F5".into(), None, 0));
    }

    #[tokio::test]
    async fn probe_against_fake_chrome_reports_exceptions_console_raf_and_input() {
        let dir = tempfile::tempdir().unwrap();
        let chrome = fake_chrome(dir.path());
        let artifact = dir.path().join("game.html");
        std::fs::write(&artifact, "<html><body>x</body></html>").unwrap();
        let settings = VerifySettings {
            chrome: Some(chrome),
            screenshot_dir: dir.path().join("shots"),
            ..VerifySettings::default()
        };
        let report = probe(
            &settings,
            &artifact,
            WebOptions {
                timeout: Duration::from_secs(30),
                settle: Duration::from_millis(300),
                input_events: vec![
                    InputEvent::Click { x: 1.0, y: 2.0 },
                    InputEvent::Key {
                        key: "x".into(),
                        hold_ms: 10,
                    },
                    InputEvent::Wait { ms: 10 },
                ],
            },
        )
        .await
        .unwrap();
        assert!(report.loaded_after.is_some(), "{report:?}");
        assert_eq!(report.boot_exception_count, 1);
        assert!(report.exceptions[0].contains("TypeError: Cannot read properties of null (reading 'getContext') @ game.html:120:15"), "{:?}", report.exceptions);
        assert_eq!(
            report.console_errors.len(),
            2,
            "{:?}",
            report.console_errors
        );
        assert!(report.console_errors[1].contains("[network] Failed to load resource: 404 tex.png"));
        // favicon 404s are filtered (noise on every page without one).
        let mut r2 = WebReport::default();
        let fav = json!({"method":"Log.entryAdded","params":{"entry":{"level":"error","source":"network","text":"404","url":"http://127.0.0.1/favicon.ico"}}});
        absorb(&mut r2, &fav, Instant::now());
        assert!(r2.console_errors.is_empty());
        assert_eq!(report.console_logs, vec!["booting 7".to_string()]);
        assert_eq!(report.raf_frames_per_s, Some(58));
        assert!(report.shot_a.as_ref().unwrap().path.exists());
        assert_eq!(report.animating(), Some(false), "identical fake PNGs");
        assert_eq!(report.input_dispatched, 3);
        assert_eq!(report.input_errors.len(), 1);
        assert!(
            report.input_errors[0].contains("ReferenceError: jump is not defined @ game.html:42:3"),
            "{:?}",
            report.input_errors
        );
        assert!(!report.passed());
        let text = format_report(&report);
        assert!(text.starts_with("VERIFY web FAIL — "), "{text}");
        assert!(
            text.contains("uncaught exceptions during boot/settle: 1\n  1. Uncaught TypeError"),
            "{text}"
        );
        assert!(text.contains("requestAnimationFrame: 58 frame(s) in 1s (ticking)"));
        assert!(text.contains("new uncaught exceptions during input: 1"));
        assert!(text.contains("screenshots: "));
    }

    #[tokio::test]
    async fn probe_needs_a_chrome() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("a.html");
        std::fs::write(&artifact, "x").unwrap();
        let settings = VerifySettings {
            chrome: None,
            chrome_ssh: Some("nowhere".into()),
            ..VerifySettings::default()
        };
        let e = probe(
            &settings,
            &artifact,
            WebOptions {
                timeout: Duration::from_secs(5),
                settle: Duration::ZERO,
                input_events: vec![],
            },
        )
        .await
        .unwrap_err();
        assert!(e.to_string().contains("needs [verify].chrome"), "{e}");
    }

    /// Manual smoke against a REAL Chrome (local or over ssh). Not run in
    /// CI. Example:
    /// `MU_VERIFY_CHROME=/home/me/chrome-test/chrome-linux64/chrome \
    ///  MU_VERIFY_CHROME_SSH=gpubox cargo test -p mu-coding --all-features \
    ///  -- --ignored real_chrome_smoke --nocapture`
    #[tokio::test]
    #[ignore]
    async fn real_chrome_smoke() {
        let Ok(chrome) = std::env::var("MU_VERIFY_CHROME") else {
            eprintln!("MU_VERIFY_CHROME unset; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let crashy = dir.path().join("crashy.html");
        std::fs::write(
            &crashy,
            r#"<!doctype html><html><body><canvas id="c" width="320" height="200"></canvas>
<script>
console.log("booting", 1);
const ctx = document.getElementById("c").getContext("2d");
let t = 0;
function frame(){ t++; ctx.fillStyle = `hsl(${t*7%360},80%,50%)`; ctx.fillRect(0,0,320,200); requestAnimationFrame(frame); }
requestAnimationFrame(frame);
window.addEventListener("keydown", e => { if (e.key === "x") jump(); });
document.getElementById("missing").getContext("2d");
</script></body></html>"#,
        )
        .unwrap();
        let settings = VerifySettings {
            chrome: Some(PathBuf::from(chrome)),
            chrome_ssh: std::env::var("MU_VERIFY_CHROME_SSH")
                .ok()
                .filter(|s| !s.is_empty()),
            screenshot_dir: dir.path().join("shots"),
            ..VerifySettings::default()
        };
        let report = probe(
            &settings,
            &crashy,
            WebOptions {
                timeout: Duration::from_secs(120),
                settle: Duration::from_secs(2),
                input_events: vec![
                    InputEvent::Click { x: 100.0, y: 100.0 },
                    InputEvent::Key {
                        key: "x".into(),
                        hold_ms: 50,
                    },
                ],
            },
        )
        .await
        .unwrap();
        let text = format_report(&report);
        eprintln!("{text}");
        assert!(report.loaded_after.is_some(), "{text}");
        assert!(
            report.exceptions.iter().any(|e| e.contains("getContext")),
            "{text}"
        );
        assert!(report.raf_frames_per_s.unwrap_or(0) > 0, "{text}");
        assert_eq!(report.animating(), Some(true), "{text}");
        assert!(
            report
                .input_errors
                .iter()
                .any(|e| e.contains("jump is not defined")),
            "{text}"
        );
        assert!(
            report.console_logs.iter().any(|l| l == "booting 1"),
            "{text}"
        );
    }

    #[test]
    fn exception_and_error_vectors_are_capped() {
        let mut r = WebReport::default();
        let exc = json!({"method":"Runtime.exceptionThrown","params":{"exceptionDetails":{"text":"Uncaught","exception":{"description":"Error: boom"}}}});
        let err = json!({"method":"Runtime.consoleAPICalled","params":{"type":"error","args":[{"type":"string","value":"bad"}]}});
        let log = json!({"method":"Log.entryAdded","params":{"entry":{"level":"error","source":"network","text":"404","url":"http://127.0.0.1/x.png"}}});
        for _ in 0..5000 {
            absorb(&mut r, &exc, Instant::now());
            absorb(&mut r, &err, Instant::now());
            absorb(&mut r, &log, Instant::now());
        }
        assert_eq!(r.exceptions.len(), MAX_LOG_LINES * 4);
        assert_eq!(r.console_errors.len(), MAX_LOG_LINES * 4);
    }

    #[test]
    fn screenshot_failure_is_a_note_not_a_verdict() {
        let mut r = WebReport {
            loaded_after: Some(Duration::from_millis(50)),
            ..Default::default()
        };
        let out = shot_or_note(&mut r, "A", Err(Stop::Failed("disk full".into()))).unwrap();
        assert!(out.is_none());
        assert!(r.passed(), "{r:?}");
        assert!(r.notes[0].contains("screenshot A not captured: disk full"));
        assert!(matches!(
            shot_or_note(&mut r, "B", Err(Stop::Timeout("screenshot B".into()))),
            Err(Stop::Timeout(_))
        ));
    }

    #[test]
    fn report_pass_shape() {
        let r = WebReport {
            artifact: PathBuf::from("/x/ok.html"),
            loaded_after: Some(Duration::from_millis(120)),
            raf_frames_per_s: Some(60),
            ..Default::default()
        };
        assert!(r.passed());
        let t = format_report(&r);
        assert!(t.starts_with("VERIFY web PASS — /x/ok.html\n"), "{t}");
        assert!(t.contains("boot: load event after 0.12s"));
    }
}
