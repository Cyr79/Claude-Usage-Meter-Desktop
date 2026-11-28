#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod badge;
mod context;
mod fetch;
mod logic;
mod platform;
mod store;

use logic::{Baseline, DayLog, History, Meter};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::image::Image;
use tauri::menu::{CheckMenuItemBuilder, Menu, MenuBuilder, MenuItem, SubmenuBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, RunEvent, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent, Wry};
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::Notify;

const GAUGE: &str = "gauge";
const SIGNIN: &str = "signin";
const STALE_AFTER_MS: f64 = 15.0 * 60.0 * 1000.0;
const REPO_URL: &str = "https://github.com/Cyr79/Claude-Usage-Meter-Desktop";

struct State {
    store: store::Store,
    meters: Vec<Meter>,
    hist: HashMap<String, History>,
    last_ok: Option<f64>,
    last_error: Option<String>,
    needs_login: bool,
    ctx: Option<context::Ctx>,
    ctx_key: String,
    gauge_shown_at: Option<Instant>,
    gauge_hidden_at: Option<Instant>,
    badge_looks: HashMap<String, String>,
    ctx_look: String,
    icon: &'static str,
    errors: u32,
    rate_hits: u32,
    clearance_at: Option<Instant>,
    slow: bool,
}

struct Shared {
    st: Mutex<State>,
    fetcher: tokio::sync::Mutex<fetch::Fetcher>,
    reader: Mutex<context::Reader>,
    wake: Notify,
    /// Downloaded update, installed on quit (electron-updater autoInstallOnAppQuit).
    update: Mutex<Option<(tauri_plugin_updater::Update, Vec<u8>)>>,
}

fn now_s() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn sh(app: &AppHandle) -> &Shared {
    app.state::<Shared>().inner()
}

fn baselines(st: &State) -> BTreeMap<String, Baseline> {
    serde_json::from_value(st.store.get("dayBaselines").clone()).unwrap_or_default()
}

fn day_logs(st: &State) -> BTreeMap<String, DayLog> {
    serde_json::from_value(st.store.get("dayLogs").clone()).unwrap_or_default()
}

fn is_stale(st: &State) -> bool {
    st.last_ok.map_or(true, |t| now_s() * 1000.0 - t > STALE_AFTER_MS)
}

fn reset_text(m: &Meter) -> String {
    if logic::is_weekly(m) { logic::reset_date_time(m.resets_at, false) } else { logic::until_reset(m.resets_at, now_s()) }
}

fn notify(app: &AppHandle, body: &str) {
    let _ = tauri_winrt_notification::Toast::new(&app.config().identifier)
        .title("Claude Usage Meter")
        .text1(body)
        .show();
}

/// Like checkForUpdatesAndNotify: one check at startup, silent download, toast, install on quit.
async fn check_update(app: AppHandle) {
    let Ok(updater) = app.updater() else { return };
    let Ok(Some(up)) = updater.check().await else { return };
    let Ok(bytes) = up.download(|_, _| {}, || {}).await else { return };
    notify(&app, &format!("A new version {} has been downloaded. Restart the application to apply the updates.", up.version));
    *sh(&app).update.lock().unwrap() = Some((up, bytes));
}

// ------------------------------------------------------------------ polling

async fn fetch_once(f: &mut fetch::Fetcher, cached: &str) -> Result<(String, Value), fetch::Error> {
    let org = f.resolve_org(Some(cached), false).await?;
    let raw = f.usage(&org).await?;
    Ok((org, raw))
}

async fn poll_once(app: &AppHandle) -> Result<(), fetch::Error> {
    let s = sh(app);
    let cached = s.st.lock().unwrap().store.str("orgUuid").to_string();
    let mut res = fetch_once(&mut *s.fetcher.lock().await, &cached).await;
    if matches!(res, Err(fetch::Error::Blocked(_))) {
        // Bot check: let a hidden webview pick up a fresh clearance, at most every 30 min.
        let due = s.st.lock().unwrap().clearance_at.map_or(true, |t| t.elapsed() > Duration::from_secs(1800));
        if due {
            s.st.lock().unwrap().clearance_at = Some(Instant::now());
            refresh_clearance(app).await;
            res = fetch_once(&mut *s.fetcher.lock().await, &cached).await;
        }
    }
    match res {
        Ok((org, raw)) => {
            apply_result(app, &org, &raw);
            Ok(())
        }
        Err(e) => {
            let mut st = s.st.lock().unwrap();
            if matches!(e, fetch::Error::Auth(_)) {
                st.needs_login = true;
            }
            // A transient failure is never reported as "signed out".
            st.last_error = Some(e.to_string());
            drop(st);
            render(app);
            Err(e)
        }
    }
}

fn apply_result(app: &AppHandle, org: &str, raw: &Value) {
    let meters = logic::extract_meters(raw);
    let now = now_s();
    let mut resets = Vec::new();
    {
        let s = sh(app);
        let mut st = s.st.lock().unwrap();
        let win = st.store.num("forecastWindowMinutes").max(1.0);
        for m in &meters {
            if st.hist.entry(m.key.clone()).or_default().add(m.pct, m.resets_at, win, now) {
                resets.push(m.label.clone());
            }
        }
        let (mut b, mut logs) = (baselines(&st), day_logs(&st));
        for m in &meters {
            logic::update_baseline(&mut b, m, now);
            logic::update_day_log(&mut logs, m, now);
        }
        st.store.set("dayBaselines", serde_json::to_value(b).unwrap_or(json!({})));
        st.store.set("dayLogs", serde_json::to_value(logs).unwrap_or(json!({})));
        st.store.set("orgUuid", json!(org));
        st.last_error = meters.is_empty().then(|| "No usage windows in the response. Free plans do not expose /usage.".to_string());
        st.meters = meters;
        st.last_ok = Some(now * 1000.0);
        st.needs_login = false;
    }
    for label in resets {
        notify(app, &format!("{label} window reset — allowance refreshed."));
    }
    render(app);
}

/// Adaptive delay: slower when idle/locked, back off on 429/errors, and
/// wake just after a reset instead of polling faster all the time.
fn next_delay(st: &mut State, res: &Result<(), fetch::Error>) -> u64 {
    let base = st.store.num("pollSeconds").max(20.0) as u64;
    let mut d = match res {
        Ok(()) => {
            st.errors = 0;
            st.rate_hits = 0;
            base
        }
        Err(fetch::Error::RateLimited(ra)) => {
            st.rate_hits += 1;
            ra.unwrap_or(300 << (st.rate_hits - 1).min(3)).clamp(60, 1800)
        }
        Err(fetch::Error::Auth(_)) => 3600,
        Err(_) => {
            st.errors += 1;
            (base << st.errors.min(4)).min(1800)
        }
    };
    if platform::session_locked() {
        d = d.max(900);
    } else if platform::idle_seconds() >= 600 {
        d = d.max(base * 5);
    }
    if res.is_ok() {
        let now = now_s();
        if let Some(r) = st.meters.iter().filter_map(|m| m.resets_at).filter(|r| *r > now).reduce(f64::min) {
            let until = (r - now) as u64 + 15;
            if until < d {
                d = until.max(20);
            }
        }
    }
    st.slow = d > base;
    d
}

async fn poll_loop(app: AppHandle) {
    loop {
        let can = {
            let st = sh(&app).st.lock().unwrap();
            st.store.flag("firstRunDone") && !st.needs_login
        } && sh(&app).fetcher.lock().await.has_session();
        let delay = if can {
            let res = poll_once(&app).await;
            next_delay(&mut sh(&app).st.lock().unwrap(), &res)
        } else {
            3600
        };
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
            _ = sh(&app).wake.notified() => {}
        }
    }
}

/// Context every 5 s while you work, 30-60 s otherwise; relative labels every 30 s.
async fn tick_loop(app: AppHandle) {
    let mut last_render = Instant::now();
    loop {
        let locked = platform::session_locked();
        let idle = platform::idle_seconds();
        if !locked {
            poll_context(&app, false);
            if last_render.elapsed() >= Duration::from_secs(30) {
                last_render = Instant::now();
                render(&app);
            }
            // Back from idle: don't wait out a long idle-mode sleep.
            let s = sh(&app);
            let mut st = s.st.lock().unwrap();
            if st.slow && idle < 60 {
                st.slow = false;
                s.wake.notify_one();
            }
        }
        let secs = if locked { 60 } else if idle >= 120 { 30 } else { 5 };
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }
}

fn poll_context(app: &AppHandle, force: bool) {
    let s = sh(app);
    let (show, limit) = {
        let st = s.st.lock().unwrap();
        (st.store.flag("showContext"), st.store.get("contextLimit").clone())
    };
    let ctx = show.then(|| s.reader.lock().unwrap().read(&limit));
    let key = match &ctx {
        Some(c) if c.available => format!("{}|{}|{}", c.pct.round(), c.active, c.tokens),
        Some(c) => format!("x|{:?}", c.reason),
        None => "off".into(),
    };
    let changed = {
        let mut st = s.st.lock().unwrap();
        st.ctx = ctx;
        let changed = st.ctx_key != key;
        st.ctx_key = key;
        changed
    };
    if force || changed {
        render(app);
    }
}

// ------------------------------------------------------------------ sign-in

fn cookie_pairs(w: &WebviewWindow) -> Vec<(String, String)> {
    let url = tauri::Url::parse(fetch::ORIGIN).expect("origin url");
    w.cookies_for_url(url)
        .map(|v| v.iter().map(|c| (c.name().to_string(), c.value().to_string())).collect())
        .unwrap_or_default()
}

/// wry's defaults plus: no GPU process, one renderer, no occlusion tracking. Idle cost is zero anyway
/// (windows are destroyed); this trims the popup / sign-in while they are open.
const WV_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion --disable-gpu --renderer-process-limit=1";

fn claude_window(app: &AppHandle, label: &str, path: &str, visible: bool) -> Option<WebviewWindow> {
    let url = format!("{}{path}", fetch::ORIGIN).parse().ok()?;
    // Remote page: no capability grants it IPC, so it can't call our commands.
    WebviewWindowBuilder::new(app, label, WebviewUrl::External(url))
        .title("Claude Usage Meter — sign in")
        .inner_size(1100.0, 800.0)
        .user_agent(fetch::UA)
        .additional_browser_args(WV_ARGS)
        .visible(visible)
        .skip_taskbar(!visible)
        .build()
        .ok()
}

async fn refresh_clearance(app: &AppHandle) {
    let Some(w) = claude_window(app, "clearance", "/", false) else { return };
    for i in 0..10 {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let pairs = cookie_pairs(&w);
        if pairs.iter().any(|(n, _)| n == "cf_clearance") || i == 9 {
            let mut f = sh(app).fetcher.lock().await;
            if f.set_cookies(&pairs) {
                f.persist();
            }
            break;
        }
    }
    let _ = w.destroy();
}

fn sign_in(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(SIGNIN) {
        let _ = w.set_focus();
        return;
    }
    sh(app).st.lock().unwrap().last_error = Some("Sign-in window open…".into());
    render(app);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let ok = sign_in_flow(&app).await;
        let s = sh(&app);
        if !ok {
            // Keep the previous (maybe still valid) session if this one never verified.
            let dir = app.path().app_config_dir().unwrap_or_default();
            *s.fetcher.lock().await = fetch::Fetcher::new(dir);
        }
        {
            let mut st = s.st.lock().unwrap();
            st.needs_login = !ok;
            st.last_error = (!ok).then(|| "Sign-in was not confirmed. Try again.".to_string());
        }
        render(&app);
        if ok {
            s.wake.notify_one();
        }
    });
}

/// Verified end to end (org + usage) before the window closes.
async fn sign_in_flow(app: &AppHandle) -> bool {
    let Some(w) = claude_window(app, SIGNIN, "/login", true) else { return false };
    let _ = w.set_focus();
    let deadline = Instant::now() + Duration::from_secs(300);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    loop {
        if app.get_webview_window(SIGNIN).is_none() {
            return false;
        }
        if Instant::now() > deadline {
            let _ = w.destroy();
            return false;
        }
        let pairs = cookie_pairs(&w);
        let mut f = sh(app).fetcher.lock().await;
        if f.set_cookies(&pairs) {
            if let Ok(org) = f.resolve_org(None, true).await {
                if f.usage(&org).await.is_ok() {
                    f.persist();
                    drop(f);
                    sh(app).st.lock().unwrap().store.set("orgUuid", json!(org));
                    let _ = w.destroy();
                    return true;
                }
            }
        }
        drop(f);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// -------------------------------------------------------------------- gauge

fn gauge_data(st: &State) -> Value {
    let now = now_s();
    let (b, logs) = (baselines(st), day_logs(st));
    let show = st.store.show();
    let meters: Vec<Value> = logic::chosen_meters(&st.meters, &show)
        .into_iter()
        .map(|m| {
            let p = logic::pace(m, b.get(&m.key), now);
            let h = st.hist.get(&m.key);
            json!({
                "key": m.key,
                "label": m.label,
                "pct": m.pct,
                "resetText": if m.resets_at.is_some() { reset_text(m) } else { "unknown".into() },
                "forecast": logic::forecast(m.pct, h.unwrap_or(&History::default()), m.resets_at, now),
                "pace": p,
                "resetsAt": m.resets_at,
                "segs": logic::week_segments(m, p.as_ref(), logs.get(&m.key), now),
            })
        })
        .collect();
    let mut ctx = st.ctx.as_ref().and_then(|c| serde_json::to_value(c).ok()).unwrap_or(json!({}));
    ctx["enabled"] = json!(st.store.flag("showContext"));
    json!({
        "needsLogin": st.needs_login,
        "pinned": st.store.flag("gaugePinned"),
        "error": st.last_error,
        "updated": st.last_ok,
        "context": ctx,
        "meters": meters,
    })
}

fn toggle_gauge(app: &AppHandle) {
    // A click on the tray blurs the popup first; don't instantly reopen it.
    if sh(app).st.lock().unwrap().gauge_hidden_at.is_some_and(|t| t.elapsed() < Duration::from_millis(350)) {
        return;
    }
    if app.get_webview_window(GAUGE).is_some() { close_gauge(app) } else { show_gauge(app) }
}

/// Built off the event loop: creating a webview inside a Windows event handler deadlocks.
fn show_gauge(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(w) = app.get_webview_window(GAUGE) {
            let _ = w.show();
            let _ = w.set_focus();
            return;
        }
        let pinned = sh(&app).st.lock().unwrap().store.flag("gaugePinned");
        let built = WebviewWindowBuilder::new(&app, GAUGE, WebviewUrl::App("index.html".into()))
            .title("Claude Usage Meter")
            .inner_size(270.0, 420.0)
            .decorations(false)
            .resizable(false)
            .skip_taskbar(true)
            .always_on_top(true)
            .shadow(false)
            .additional_browser_args(WV_ARGS)
            .visible(false)
            .focused(!pinned)
            .background_color(tauri::window::Color(0x16, 0x16, 0x1c, 0xff))
            .build();
        let Ok(w) = built else { return };
        let a = app.clone();
        w.on_window_event(move |ev| match ev {
            WindowEvent::Focused(false) => {
                let st = sh(&a).st.lock().unwrap();
                let fresh = st.gauge_shown_at.map_or(true, |t| t.elapsed() < Duration::from_millis(500));
                if !st.store.flag("gaugePinned") && !fresh {
                    drop(st);
                    close_gauge(&a);
                }
            }
            WindowEvent::Moved(p) => {
                let mut st = sh(&a).st.lock().unwrap();
                if st.store.flag("gaugePinned") {
                    st.store.set("gaugePos", json!({ "x": p.x, "y": p.y }));
                }
            }
            _ => {}
        });
    });
}

/// Destroyed, not hidden: no webview stays alive while the popup is closed.
fn close_gauge(app: &AppHandle) {
    sh(app).st.lock().unwrap().gauge_hidden_at = Some(Instant::now());
    if let Some(w) = app.get_webview_window(GAUGE) {
        tauri::async_runtime::spawn(async move {
            let _ = w.destroy();
        });
    }
}

fn set_pinned(app: &AppHandle, pinned: bool) {
    // Window calls before locking: they may hop to the main thread, which may want the lock.
    let pos = pinned.then(|| app.get_webview_window(GAUGE).and_then(|w| w.outer_position().ok())).flatten();
    {
        let mut st = sh(app).st.lock().unwrap();
        st.store.set("gaugePinned", json!(pinned));
        if let Some(p) = pos {
            st.store.set("gaugePos", json!({ "x": p.x, "y": p.y }));
        }
    }
    render(app);
}

fn place_gauge(app: &AppHandle, w: &WebviewWindow) {
    let Ok(size) = w.outer_size() else { return };
    let (gw, gh) = (size.width as i32, size.height as i32);
    let saved = {
        let st = sh(app).st.lock().unwrap();
        let p = st.store.get("gaugePos");
        (st.store.flag("gaugePinned") && p.is_object()).then(|| (p["x"].as_f64().unwrap_or(0.0), p["y"].as_f64().unwrap_or(0.0)))
    };
    let point = saved.or_else(|| app.cursor_position().ok().map(|p| (p.x, p.y))).unwrap_or((0.0, 0.0));
    let Some(mon) = app.monitor_from_point(point.0, point.1).ok().flatten().or_else(|| app.primary_monitor().ok().flatten()) else { return };
    let wa = mon.work_area();
    let (ax, ay, aw, ah) = (wa.position.x, wa.position.y, wa.size.width as i32, wa.size.height as i32);
    let m = (8.0 * mon.scale_factor()) as i32;
    let (x, y) = if saved.is_some() {
        ((point.0 as i32).clamp(ax, (ax + aw - gw).max(ax)), (point.1 as i32).clamp(ay, (ay + ah - gh).max(ay)))
    } else {
        // Taskbar at the bottom (usual) -> above the icon; at the top -> below it.
        let y = if point.1 as i32 > ay + ah / 2 { ay + ah - gh - m } else { ay + m };
        ((point.0 as i32 - gw / 2).clamp(ax + m, (ax + aw - gw - m).max(ax + m)), y.max(ay))
    };
    let _ = w.set_position(PhysicalPosition::new(x, y));
}

fn push_gauge(app: &AppHandle, st: &State) {
    if app.get_webview_window(GAUGE).is_some() {
        let _ = app.emit_to(GAUGE, "gauge:data", gauge_data(st));
    }
}

// -------------------------------------------------------------- commands

fn gauge_only(w: &WebviewWindow) -> Result<(), String> {
    if w.label() == GAUGE { Ok(()) } else { Err("forbidden".into()) }
}

#[tauri::command]
async fn ready(window: WebviewWindow, app: AppHandle) -> Result<Value, String> {
    gauge_only(&window)?;
    let st = sh(&app).st.lock().unwrap();
    Ok(gauge_data(&st))
}

#[tauri::command]
async fn refresh(window: WebviewWindow, app: AppHandle) -> Result<(), String> {
    gauge_only(&window)?;
    sh(&app).wake.notify_one();
    poll_context(&app, true);
    Ok(())
}

#[tauri::command]
async fn hide(window: WebviewWindow, app: AppHandle) -> Result<(), String> {
    gauge_only(&window)?;
    if sh(&app).st.lock().unwrap().store.flag("gaugePinned") {
        set_pinned(&app, false);
    }
    close_gauge(&app);
    Ok(())
}

#[tauri::command]
async fn pin(window: WebviewWindow, app: AppHandle) -> Result<(), String> {
    gauge_only(&window)?;
    let p = sh(&app).st.lock().unwrap().store.flag("gaugePinned");
    set_pinned(&app, !p);
    Ok(())
}

/// The page reports its card height after each render; first report shows the window.
#[tauri::command]
async fn fit(window: WebviewWindow, app: AppHandle, h: f64) -> Result<(), String> {
    gauge_only(&window)?;
    let h = h.clamp(40.0, 720.0).ceil();
    let _ = window.set_size(tauri::LogicalSize::new(270.0, h));
    if !window.is_visible().unwrap_or(true) {
        place_gauge(&app, &window);
        let pinned = {
            let mut st = sh(&app).st.lock().unwrap();
            st.gauge_shown_at = Some(Instant::now());
            st.store.flag("gaugePinned")
        };
        let _ = window.show();
        if !pinned {
            let _ = window.set_focus();
        }
    } else {
        place_gauge(&app, &window);
    }
    Ok(())
}

#[tauri::command]
async fn open_claude(window: WebviewWindow) -> Result<(), String> {
    gauge_only(&window)?;
    open_claude_desktop();
    Ok(())
}

fn open_claude_desktop() {
    if !(platform::protocol_registered("claude") && platform::open("claude://")) {
        platform::open(fetch::ORIGIN);
    }
}

// ------------------------------------------------------------------ menu

fn line(app: &AppHandle, text: &str) -> tauri::Result<MenuItem<Wry>> {
    MenuItem::new(app, text, false, None::<&str>)
}

fn check(app: &AppHandle, id: &str, text: &str, on: bool) -> tauri::Result<tauri::menu::CheckMenuItem<Wry>> {
    CheckMenuItemBuilder::with_id(id, text).checked(on).build(app)
}

fn build_menu(app: &AppHandle, st: &State) -> tauri::Result<Menu<Wry>> {
    let now = now_s();
    let b = baselines(st);
    let mut m = MenuBuilder::new(app)
        .text("show", "⚡ Show energy gauge")
        .item(&check(app, "pin", "📌 Keep gauge on screen (always on top)", st.store.flag("gaugePinned"))?)
        .separator();
    if st.needs_login {
        m = m.item(&line(app, "Not signed in")?).text("signin", "Sign in to Claude…").separator();
    } else if st.meters.is_empty() {
        m = m.item(&line(app, "Waiting for first reading…")?).separator();
    } else {
        for meter in &st.meters {
            let p = logic::pace(meter, b.get(&meter.key), now);
            let h = st.hist.get(&meter.key);
            let mut sub = SubmenuBuilder::new(app, format!("{}   {}%", meter.label, meter.pct.round()))
                .item(&line(app, &format!("Resets {}", reset_text(meter)))?)
                .item(&line(app, &logic::forecast(meter.pct, h.unwrap_or(&History::default()), meter.resets_at, now))?);
            for l in logic::pace_lines(p.as_ref()) {
                sub = sub.item(&line(app, &l)?);
            }
            if let Some(r) = meter.resets_at.filter(|_| !logic::is_weekly(meter)) {
                sub = sub.item(&line(app, &logic::date_time(r))?);
            }
            m = m.item(&sub.separator().item(&line(app, &format!("source: {}", meter.path))?).build()?);
        }
        m = m.separator();
    }
    if let Some(c) = st.ctx.as_ref().filter(|c| c.available && st.store.flag("showContext")) {
        let t = format!(
            "Claude Code context   {}%  ({}k/{}k{})",
            c.pct.round(), (c.tokens / 1000.0).round(), (c.limit / 1000.0).round(), if c.active { "" } else { ", idle" }
        );
        m = m.item(&line(app, &t)?).separator();
    }
    m = m.item(&line(app, &match st.last_ok {
        Some(t) => format!("Updated {}", logic::clock(t)),
        None => "Updated never".into(),
    })?);
    if let Some(e) = &st.last_error {
        m = m.item(&line(app, &format!("⚠ {}", e.chars().take(60).collect::<String>()))?);
    }
    m = m.text("refresh", "Refresh now").separator();

    let single = st.store.str("windowsTrayMode") == "single";
    let badges = SubmenuBuilder::new(app, "Taskbar badges")
        .item(&check(app, "tray:per-meter", "One badge per meter", !single)?)
        .item(&check(app, "tray:single", "Single badge (closest to limit)", single)?)
        .build()?;
    let limit = match st.store.get("contextLimit") {
        Value::Number(n) => n.to_string(),
        v => v.as_str().unwrap_or("auto").to_string(),
    };
    let mut sizes = SubmenuBuilder::new(app, "Context window size");
    for (v, l) in [("auto", "Auto (200k, 1M if larger)"), ("200000", "200k"), ("1000000", "1M")] {
        sizes = sizes.item(&check(app, &format!("ctxl:{v}"), l, limit == v)?);
    }
    let mut display = SubmenuBuilder::new(app, "Display")
        .item(&badges)
        .separator()
        .item(&check(app, "pace", "Red badge when over daily pace", st.store.get("paceColours") != &json!(false))?)
        .item(&check(app, "ctx", "Claude Code context gauge", st.store.flag("showContext"))?)
        .item(&sizes.build()?)
        .separator();
    let show = st.store.show();
    for key in logic::CANONICAL_ORDER {
        let has = st.meters.is_empty() || st.meters.iter().any(|m| m.key == key);
        let label = format!("Show {}{}", logic::label_of(key).unwrap_or(key), if has { "" } else { "  (no separate limit on your account)" });
        display = display.item(&CheckMenuItemBuilder::with_id(format!("show:{key}"), label).checked(show.iter().any(|k| k == key)).enabled(has).build(app)?);
    }
    m.item(&display.build()?)
        .text("claude", "Open Claude Desktop")
        .text("web", "Open claude.ai")
        .text("signin", "Sign in again…")
        .text("diag", "Copy diagnostics")
        .item(&check(app, "login", "Start at login", platform::login_item_enabled())?)
        .separator()
        .text("about", "About Claude Usage Meter…")
        .text("quit", "Quit")
        .build()
}

fn handle_menu(app: &AppHandle, id: &str) {
    let s = sh(app);
    let toggle = |k: &str, default_on: bool| {
        let mut st = s.st.lock().unwrap();
        let cur = st.store.get(k).as_bool().unwrap_or(default_on);
        st.store.set(k, json!(!cur));
    };
    match id {
        "show" => show_gauge(app),
        "pin" => {
            let p = !s.st.lock().unwrap().store.flag("gaugePinned");
            set_pinned(app, p);
            if p {
                show_gauge(app);
            }
        }
        "signin" => sign_in(app),
        "refresh" => {
            s.wake.notify_one();
            poll_context(app, true);
        }
        "pace" => {
            toggle("paceColours", true);
            render(app);
        }
        "ctx" => {
            toggle("showContext", false);
            poll_context(app, true);
        }
        "claude" => open_claude_desktop(),
        "web" => {
            platform::open(fetch::ORIGIN);
        }
        "diag" => copy_diagnostics(app),
        "login" => {
            let on = !platform::login_item_enabled();
            platform::set_login_item(on);
            s.st.lock().unwrap().store.set("startAtLogin", json!(on));
            render(app);
        }
        "about" => {
            let text = format!(
                "Claude Usage Meter {}\n\nYour Claude.ai usage limits in the tray, with daily pace and live Claude Code context.\n\n\
                 Fork maintained by Cyr79 — {REPO_URL}\nBased on the original Claude Usage Meter by SelectorsHub.\n\
                 Independent project, not affiliated with Anthropic.\n\n\
                 Privacy: talks only to claude.ai with your own sign-in and to GitHub for updates. Your login stays on this PC (DPAPI-encrypted).\n\n\
                 Tauri {} · WebView2\n\nOpen the project on GitHub?",
                app.package_info().version,
                tauri::VERSION
            );
            std::thread::spawn(move || {
                if platform::message("About Claude Usage Meter", &text, true) {
                    platform::open(REPO_URL);
                }
            });
        }
        "quit" => {
            let pending = s.update.lock().unwrap().take();
            if let Some((up, bytes)) = pending {
                let _ = up.install(bytes); // runs the installer and exits the process
            }
            app.exit(0)
        }
        _ => {
            if let Some(mode) = id.strip_prefix("tray:") {
                s.st.lock().unwrap().store.set("windowsTrayMode", json!(mode));
                destroy_meter_trays(app);
                render(app);
            } else if let Some(v) = id.strip_prefix("ctxl:") {
                let v = v.parse::<u64>().map(|n| json!(n)).unwrap_or(json!("auto"));
                s.st.lock().unwrap().store.set("contextLimit", v);
                poll_context(app, true);
            } else if let Some(key) = id.strip_prefix("show:") {
                let mut st = s.st.lock().unwrap();
                let mut show = st.store.show();
                match show.iter().position(|k| k == key) {
                    Some(i) => { show.remove(i); }
                    None => show.push(key.to_string()),
                }
                show.sort_by_key(|k| logic::CANONICAL_ORDER.iter().position(|c| c == k).unwrap_or(99));
                st.store.set("show", json!(show));
                drop(st);
                render(app);
            }
        }
    }
}

fn copy_diagnostics(app: &AppHandle) {
    let s = sh(app);
    // Never block the UI thread on a poll in flight.
    let last_fetch = s.fetcher.try_lock().map(|f| f.diag.clone()).unwrap_or(json!("poll in progress"));
    let st = s.st.lock().unwrap();
    let text = serde_json::to_string_pretty(&json!({
        "version": app.package_info().version.to_string(),
        "platform": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "tauri": tauri::VERSION,
        "needsLogin": st.needs_login,
        "lastError": st.last_error,
        "meters": st.meters.iter().map(|m| json!([m.key, m.pct, m.path])).collect::<Vec<_>>(),
        "lastFetch": last_fetch,
    }))
    .unwrap_or_default();
    drop(st);
    if platform::copy_text(&text) {
        notify(app, "Diagnostics copied to clipboard.");
    }
}

// ------------------------------------------------------------------ tray

fn state_icon(name: &str) -> Image<'static> {
    let bytes: &'static [u8] = match name {
        "warn" => include_bytes!("../icons/tray/warn@2x.png"),
        "danger" => include_bytes!("../icons/tray/danger@2x.png"),
        "stale" => include_bytes!("../icons/tray/stale@2x.png"),
        _ => include_bytes!("../icons/tray/normal@2x.png"),
    };
    Image::from_bytes(bytes).expect("bundled tray png")
}

fn new_tray(app: &AppHandle, id: &str, icon: Image<'static>, menu: &Menu<Wry>) -> tauri::Result<tauri::tray::TrayIcon<Wry>> {
    TrayIconBuilder::with_id(id)
        .icon(icon)
        .menu(menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                toggle_gauge(tray.app_handle());
            }
        })
        .build(app)
}

fn destroy_meter_trays(app: &AppHandle) {
    let keys: Vec<String> = sh(app).st.lock().unwrap().badge_looks.drain().map(|(k, _)| k).collect();
    for k in keys {
        drop(app.remove_tray_by_id(&format!("m:{k}")));
    }
}

/// UI work always happens on the main thread; callers may be anywhere.
fn render(app: &AppHandle) {
    let a = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Err(e) = render_now(&a) {
            eprintln!("render: {e}");
        }
    });
}

fn render_now(app: &AppHandle) -> tauri::Result<()> {
    let s = sh(app);
    let mut st = s.st.lock().unwrap();
    let stale = is_stale(&st);
    let menu = build_menu(app, &st)?;
    let show = st.store.show();
    let (warn_at, danger_at) = (st.store.num("warnAt"), st.store.num("dangerAt"));
    let chosen: Vec<Meter> = logic::chosen_meters(&st.meters, &show).into_iter().cloned().collect();
    let refs: Vec<&Meter> = chosen.iter().collect();

    let icon = logic::icon_state(&refs, stale, warn_at, danger_at);
    let main = match app.tray_by_id("main") {
        Some(t) => t,
        None => new_tray(app, "main", state_icon(icon), &menu)?,
    };
    if icon != st.icon {
        main.set_icon(Some(state_icon(icon)))?;
        st.icon = icon;
    }
    main.set_tooltip(Some(if st.needs_login { "Claude Usage Meter — click to sign in".into() } else { logic::tooltip(&refs, stale) }))?;
    main.set_menu(Some(menu.clone()))?;

    // One numbered badge per meter (or just the closest to its limit).
    let mut wanted: Vec<&Meter> = if st.needs_login { vec![] } else { refs.clone() };
    if st.store.str("windowsTrayMode") == "single" {
        wanted = wanted.into_iter().reduce(|a, b| if b.pct > a.pct { b } else { a }).into_iter().collect();
    }
    let gone: Vec<String> = st.badge_looks.keys().filter(|k| !wanted.iter().any(|m| &&m.key == k)).cloned().collect();
    for k in gone {
        st.badge_looks.remove(&k);
        drop(app.remove_tray_by_id(&format!("m:{k}")));
    }
    let b = baselines(&st);
    let now = now_s();
    let pace_colours = st.store.get("paceColours") != &json!(false);
    for m in wanted {
        let p = logic::pace(m, b.get(&m.key), now);
        let mut role = if stale { "stale" } else { logic::severity(m.pct, warn_at, danger_at) };
        if let Some(p) = p.as_ref().filter(|_| !stale && pace_colours) {
            if p.hot {
                role = "danger";
            } else if p.warn && role != "danger" {
                role = "warn";
            }
        }
        let look = format!("{}|{role}", m.pct.round());
        let id = format!("m:{}", m.key);
        let img = || Image::new_owned(badge::render(m.pct, role), 32, 32);
        let tray = match app.tray_by_id(&id) {
            Some(t) => {
                if st.badge_looks.get(&m.key) != Some(&look) {
                    t.set_icon(Some(img()))?;
                }
                t
            }
            None => new_tray(app, &id, img(), &menu)?,
        };
        st.badge_looks.insert(m.key.clone(), look);
        let reset = m.resets_at.map(|_| {
            if logic::is_weekly(m) { logic::reset_date_time(m.resets_at, true) } else { logic::until_reset(m.resets_at, now) }
        });
        let mut lines = vec![format!("{} — {}%{}", m.label, m.pct.round(), reset.map(|r| format!(", resets {r}")).unwrap_or_default())];
        lines.extend(logic::tip_lines(p.as_ref()));
        tray.set_tooltip(Some(logic::fit_tip(&lines)))?;
        tray.set_menu(Some(menu.clone()))?;
    }

    // Live Claude Code context badge.
    let ctx = st.ctx.clone().filter(|c| st.store.flag("showContext") && st.store.get("contextBadge") != &json!(false) && c.available && c.active);
    match ctx {
        None => {
            st.ctx_look.clear();
            drop(app.remove_tray_by_id("ctx"));
        }
        Some(c) => {
            let role = if c.pct >= 90.0 { "danger" } else if c.pct >= 70.0 { "warn" } else { "context" };
            let look = format!("{}|{role}", c.pct.round());
            let img = || Image::new_owned(badge::render(c.pct, role), 32, 32);
            let tray = match app.tray_by_id("ctx") {
                Some(t) => {
                    if st.ctx_look != look {
                        t.set_icon(Some(img()))?;
                    }
                    t
                }
                None => new_tray(app, "ctx", img(), &menu)?,
            };
            st.ctx_look = look;
            tray.set_tooltip(Some(format!(
                "Claude Code context — {}%\n{}k / {}k tokens",
                c.pct.round(), (c.tokens / 1000.0).round(), (c.limit / 1000.0).round()
            )))?;
            tray.set_menu(Some(menu.clone()))?;
        }
    }
    push_gauge(app, &st);
    Ok(())
}

// ------------------------------------------------------------------ boot

fn first_run(app: &AppHandle) {
    let s = sh(app);
    let has_session = s.fetcher.try_lock().map(|f| f.has_session()).unwrap_or(false);
    let mut st = s.st.lock().unwrap();
    let migrated = st.store.migrated;
    if st.store.flag("firstRunDone") && (has_session || !migrated) {
        return;
    }
    // Electron users keep their start-at-login choice; new users get it on.
    if !migrated || st.store.flag("startAtLogin") {
        platform::set_login_item(true);
        st.store.set("startAtLogin", json!(true));
    }
    st.store.set("firstRunDone", json!(true));
    drop(st);
    let app = app.clone();
    std::thread::spawn(move || {
        platform::message(
            "Claude Usage Meter",
            "Welcome\n\nA window will open once so you can sign in to Claude. After that the meter runs in the \
             background and you will not see it again.\n\nYour usage percentages appear in the system tray. Click a \
             badge for the energy gauge, hover for pace, right-click for the menu.",
            false,
        );
        sign_in(&app);
    });
}

fn main() {
    // One worker thread is plenty for a tray app that sleeps between polls.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    tauri::async_runtime::set(rt.handle().clone());
    let _rt = Box::leak(Box::new(rt));

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_gauge(app)))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![ready, refresh, hide, pin, fit, open_claude])
        .on_menu_event(|app, ev| handle_menu(app, ev.id().as_ref()))
        .setup(|app| {
            let dir = app.path().app_config_dir()?;
            let store = store::Store::load(dir.clone());
            let fetcher = fetch::Fetcher::new(dir);
            let needs_login = !fetcher.has_session();
            app.manage(Shared {
                st: Mutex::new(State {
                    store,
                    meters: vec![],
                    hist: HashMap::new(),
                    last_ok: None,
                    last_error: None,
                    needs_login,
                    ctx: None,
                    ctx_key: String::new(),
                    gauge_shown_at: None,
                    gauge_hidden_at: None,
                    badge_looks: HashMap::new(),
                    ctx_look: String::new(),
                    icon: "stale",
                    errors: 0,
                    rate_hits: 0,
                    clearance_at: None,
                    slow: false,
                }),
                fetcher: tokio::sync::Mutex::new(fetcher),
                reader: Mutex::new(context::Reader::default()),
                wake: Notify::new(),
                update: Mutex::new(None),
            });
            let h = app.handle().clone();
            render_now(&h)?;
            first_run(&h);
            if sh(&h).st.lock().unwrap().store.flag("gaugePinned") {
                let a = h.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    show_gauge(&a);
                });
            }
            tauri::async_runtime::spawn(poll_loop(h.clone()));
            tauri::async_runtime::spawn(check_update(h.clone()));
            tauri::async_runtime::spawn(tick_loop(h));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("tauri app");
    app.run(|_app, ev| {
        // A tray app has no windows by design; closing them must not quit it.
        if let RunEvent::ExitRequested { api, code: None, .. } = ev {
            api.prevent_exit();
        }
    });
}
