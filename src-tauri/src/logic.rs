//! Pure logic ported 1:1 from the Electron build: parse.js, forecast.js,
//! pace.js, daylog.js, format.js. No I/O here, so it is all unit-tested.

use chrono::{Datelike, Local, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const CANONICAL_ORDER: [&str; 4] = ["session", "weekly", "weekly_opus", "fable"];
const SESSION_HINTS: [&str; 4] = ["five_hour", "5_hour", "fivehour", "session"];
const WEEKLY_HINTS: [&str; 5] = ["seven_day", "7_day", "sevenday", "weekly", "week"];
const DAY: f64 = 86400.0;

pub fn label_of(key: &str) -> Option<&'static str> {
    Some(match key {
        "session" => "Session",
        "weekly" => "Weekly",
        "weekly_opus" => "Weekly (Opus)",
        "fable" => "Fable",
        _ => return None,
    })
}

fn short_of(key: &str) -> Option<&'static str> {
    Some(match key {
        "session" => "S",
        "weekly" => "W",
        "weekly_opus" => "O",
        "fable" => "F",
        _ => return None,
    })
}

fn order(key: &str) -> usize {
    CANONICAL_ORDER.iter().position(|k| *k == key).unwrap_or(99)
}

#[derive(Clone, Debug, Serialize)]
pub struct Meter {
    pub key: String,
    pub label: String,
    pub short: String,
    pub pct: f64,
    #[serde(rename = "resetsAt")]
    pub resets_at: Option<f64>,
    pub path: String,
    pub known: bool,
}

impl Meter {
    fn new(key: String, label: String, pct: f64, resets_at: Option<f64>, path: String) -> Self {
        let short = short_of(&key)
            .map(String::from)
            .unwrap_or_else(|| label.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default());
        let known = label_of(&key).is_some();
        Meter { key, label, short, pct, resets_at, path, known }
    }
}

// ---------------------------------------------------------------- parse.js

fn parse_ts(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64().map(|x| if x > 1e11 { x / 1000.0 } else { x }),
        Value::String(s) if !s.is_empty() => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.timestamp_millis() as f64 / 1000.0),
        _ => None,
    }
}

/// JS `Number(x)` for the shapes the payload uses; NaN -> None.
fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|x: &f64| x.is_finite())
}

/// REST /usage `utilization` is 0..100. `1` means one percent. Never scale here.
fn to_pct(v: Option<&Value>) -> Option<f64> {
    num(v).map(|x| x.clamp(0.0, 100.0))
}

fn classify(path: &str, node: &Map<String, Value>) -> String {
    let mut hay = path.to_lowercase();
    for f in ["type", "name", "key", "scope", "model", "limit_type", "id"] {
        if let Some(Value::String(s)) = node.get(f) {
            hay.push(' ');
            hay.push_str(&s.to_lowercase());
        }
    }
    let has = |h: &[&str]| h.iter().any(|x| hay.contains(x));
    if hay.contains("fable") {
        return "fable".into();
    }
    if hay.contains("opus") {
        return "weekly_opus".into();
    }
    if has(&SESSION_HINTS) {
        return "session".into();
    }
    if has(&WEEKLY_HINTS) {
        return "weekly".into();
    }
    let mut out = String::from("raw:");
    let mut in_idx = false;
    for c in path.chars() {
        match c {
            '[' => { in_idx = true; out.push_str("[]"); }
            ']' => in_idx = false,
            _ if in_idx => {}
            _ => out.push(c),
        }
    }
    out
}

fn walk<'a>(node: &'a Value, path: String, out: &mut Vec<(String, &'a Map<String, Value>)>) {
    match node {
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                walk(v, format!("{path}[{i}]"), out);
            }
        }
        Value::Object(o) => {
            if o.contains_key("utilization") || o.contains_key("used") || o.contains_key("remaining") {
                out.push((if path.is_empty() { "$".into() } else { path.clone() }, o));
            }
            for (k, v) in o {
                walk(v, if path.is_empty() { k.clone() } else { format!("{path}.{k}") }, out);
            }
        }
        _ => {}
    }
}

fn node_pct(n: &Map<String, Value>) -> Option<f64> {
    if let Some(p) = to_pct(n.get("utilization")) {
        return Some(p);
    }
    let limit = num(n.get("limit").filter(|v| !v.is_null()).or(n.get("total")))?;
    if limit > 0.0 {
        if let Some(used) = num(n.get("used")) {
            return Some((used / limit * 100.0).clamp(0.0, 100.0));
        }
        if let Some(rem) = num(n.get("remaining")) {
            return Some(((1.0 - rem / limit) * 100.0).clamp(0.0, 100.0));
        }
    }
    None
}

/// Turn a /usage response into meters, tolerating schema drift (walks the tree).
pub fn extract_meters(raw: &Value) -> Vec<Meter> {
    let Some(obj) = raw.as_object() else { return vec![] };
    let mut found: Vec<Meter> = Vec::new();
    let depth = |s: &str| s.matches('.').count();
    let mut nodes = Vec::new();
    walk(raw, String::new(), &mut nodes);
    for (path, node) in nodes {
        let Some(pct) = node_pct(node) else { continue };
        let key = classify(&path, node);
        let resets_at = ["resets_at", "reset_at", "resets", "expires_at", "window_end"]
            .iter()
            .find_map(|f| parse_ts(node.get(*f)));
        let label = label_of(&key)
            .map(String::from)
            .unwrap_or_else(|| path.rsplit('.').next().unwrap_or("").replace('_', " "));
        let m = Meter::new(key, label, pct, resets_at, path);
        // A shallower path wins: top-level fields beat nested duplicates.
        match found.iter_mut().find(|e| e.key == m.key) {
            Some(e) if depth(&m.path) < depth(&e.path) => *e = m,
            Some(_) => {}
            None => found.push(m),
        }
    }
    // Newer payloads: `limits[]` with `percent` and a model scope. Fills gaps only.
    if let Some(Value::Array(limits)) = obj.get("limits") {
        for (i, item) in limits.iter().enumerate() {
            let Some(item) = item.as_object() else { continue };
            let Some(pct) = to_pct(item.get("percent")) else { continue };
            let model = item
                .get("scope")
                .and_then(|s| s.get("model"))
                .and_then(|m| m.get("display_name"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let s = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or("");
            let hay = format!("{} {} {}", s("kind"), s("group"), model.unwrap_or("")).to_lowercase();
            let key = if let Some(model) = model {
                if hay.contains("fable") { "fable".into() }
                else if hay.contains("opus") { "weekly_opus".into() }
                else { format!("scoped:{}", model.to_lowercase()) }
            } else if hay.contains("session") { "session".into() }
            else if hay.contains("weekly") { "weekly".into() }
            else { continue };
            if found.iter().any(|m| m.key == key) {
                continue;
            }
            let label = label_of(&key).map(String::from).unwrap_or_else(|| model.unwrap_or("").to_string());
            found.push(Meter::new(key, label, pct, parse_ts(item.get("resets_at")), format!("limits[{i}]")));
        }
    }
    found.sort_by_key(|m| order(&m.key));
    found
}

// ------------------------------------------------------------- forecast.js

const RESET_JUMP_SECONDS: f64 = 1800.0;
const MAX_SAMPLES: usize = 120;

#[derive(Default)]
pub struct History {
    samples: Vec<(f64, f64)>,
    last_resets_at: Option<f64>,
}

impl History {
    fn is_reset(&self, pct: f64, resets_at: Option<f64>) -> bool {
        if let (Some(l), Some(r)) = (self.last_resets_at, resets_at) {
            if l != 0.0 && r != 0.0 && r - l >= RESET_JUMP_SECONDS {
                return true;
            }
        }
        matches!(self.samples.last(), Some(&(_, prev)) if prev >= 5.0 && pct <= prev * 0.4)
    }

    /// Record a sample. Returns true when a window reset was detected.
    pub fn add(&mut self, pct: f64, resets_at: Option<f64>, window_minutes: f64, now: f64) -> bool {
        let reset = self.is_reset(pct, resets_at);
        if reset {
            self.samples.clear();
        }
        self.samples.push((now, pct));
        self.last_resets_at = resets_at;
        let cutoff = now - window_minutes * 60.0;
        self.samples.retain(|(t, _)| *t >= cutoff);
        if self.samples.len() > MAX_SAMPLES {
            self.samples.drain(..self.samples.len() - MAX_SAMPLES);
        }
        reset
    }

    /// Least-squares slope of pct over time, in percent per hour.
    pub fn rate_per_hour(&self) -> Option<f64> {
        let s = &self.samples;
        if s.len() < 3 || s[s.len() - 1].0 - s[0].0 < 480.0 {
            return None;
        }
        let n = s.len() as f64;
        let mt = s.iter().map(|x| x.0).sum::<f64>() / n;
        let mp = s.iter().map(|x| x.1).sum::<f64>() / n;
        let (mut num, mut den) = (0.0, 0.0);
        for (t, p) in s {
            num += (t - mt) * (p - mp);
            den += (t - mt).powi(2);
        }
        (den > 0.0).then(|| num / den * 3600.0)
    }
}

pub fn format_duration(seconds: f64) -> String {
    let total = seconds.round().max(0.0) as i64;
    let (h, m) = (total / 3600, (total % 3600) / 60);
    match (h, m) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

pub fn forecast(pct: f64, h: &History, resets_at: Option<f64>, now: f64) -> String {
    if pct >= 99.5 {
        return "limit reached".into();
    }
    let Some(rate) = h.rate_per_hour() else { return "measuring pace…".into() };
    if rate <= 0.5 {
        return "steady — no drain".into();
    }
    let hours_left = (100.0 - pct) / rate;
    match resets_at {
        Some(r) if r != 0.0 && r <= now + hours_left * 3600.0 => {
            format!("resets first (in {})", format_duration(r - now))
        }
        _ => format!("~{} at current pace", format_duration(hours_left * 3600.0)),
    }
}

pub fn until_reset(resets_at: Option<f64>, now: f64) -> String {
    match resets_at {
        None => "unknown".into(),
        Some(r) if r <= now => "due now".into(),
        Some(r) => format!("in {}", format_duration(r - now)),
    }
}

/// French or English, picked once from the Windows user locale.
pub fn is_fr() -> bool {
    *crate::platform::IS_FR.get_or_init(crate::platform::user_locale_is_fr)
}

fn local(ts: f64) -> chrono::DateTime<Local> {
    Local
        .timestamp_opt(ts.floor() as i64, 0)
        .earliest()
        .unwrap_or_else(Local::now)
}

const FR_DAYS: [&str; 7] = ["lun.", "mar.", "mer.", "jeu.", "ven.", "sam.", "dim."];
const EN_DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const FR_MON: [&str; 12] = ["janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.", "déc."];
const EN_MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Weekly reset as a day: a reset at midnight shows as 23:59 of the day before,
/// so the date is the last day you can still use.
pub fn reset_date_time(resets_at: Option<f64>, short: bool) -> String {
    let Some(r) = resets_at else { return "unknown".into() };
    let mut d = local((r / 60.0).round() * 60.0);
    if d.hour() == 0 && d.minute() == 0 {
        d = d - chrono::Duration::minutes(1);
    }
    let (wd, mo) = (d.weekday().num_days_from_monday() as usize, d.month0() as usize);
    let year = if short { String::new() } else { format!(" {}", d.year()) };
    if is_fr() {
        format!("{} {} {}{}, {:02}:{:02}", FR_DAYS[wd], d.day(), FR_MON[mo], year, d.hour(), d.minute())
    } else {
        format!("{}, {} {}{}, {:02}:{:02}", EN_DAYS[wd], d.day(), EN_MON[mo], year, d.hour(), d.minute())
    }
}

pub fn clock(ms: f64) -> String {
    local(ms / 1000.0).format("%H:%M:%S").to_string()
}

pub fn date_time(ts: f64) -> String {
    local(ts).format("%Y-%m-%d %H:%M").to_string()
}

// ----------------------------------------------------------------- pace.js

pub struct Win {
    pub seconds: f64,
    pub unit: &'static str,
    unit_seconds: f64,
}

pub fn window_of(key: &str) -> Option<Win> {
    match key {
        "session" => Some(Win { seconds: 5.0 * 3600.0, unit: "h", unit_seconds: 3600.0 }),
        "weekly" | "weekly_opus" | "fable" => Some(Win { seconds: 7.0 * DAY, unit: "day", unit_seconds: DAY }),
        _ => None,
    }
}

pub fn is_weekly(m: &Meter) -> bool {
    matches!(m.key.as_str(), "weekly" | "weekly_opus" | "fable")
}

pub fn local_day(ts: f64) -> String {
    local(ts).format("%Y-%m-%d").to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Baseline {
    pub day: String,
    pub pct: f64,
    pub at: f64,
    #[serde(rename = "resetsAt", default)]
    pub resets_at: Option<f64>,
}

/// New local day, a reset, or pct dropping below the baseline restarts it.
pub fn update_baseline(b: &mut BTreeMap<String, Baseline>, m: &Meter, now: f64) {
    let day = local_day(now);
    let fresh = Baseline { day: day.clone(), pct: m.pct, at: now, resets_at: m.resets_at };
    match b.get_mut(&m.key) {
        Some(cur) => {
            let reset = matches!((m.resets_at, cur.resets_at), (Some(a), Some(c)) if a != 0.0 && c != 0.0 && a - c >= 1800.0);
            if cur.day != day || reset || m.pct < cur.pct {
                *cur = fresh;
            } else if let Some(r) = m.resets_at.filter(|r| *r != 0.0) {
                // resets_at jitters by under a second per poll; ignore that noise.
                if (r - cur.resets_at.unwrap_or(0.0)).abs() >= 60.0 {
                    cur.resets_at = Some(r);
                }
            }
        }
        None => {
            b.insert(m.key.clone(), fresh);
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pace {
    pub unit: &'static str,
    pub target: f64,
    pub expected: f64,
    pub avg: f64,
    pub budget: Option<f64>,
    pub left_units: f64,
    pub left_pct: f64,
    pub left_text: String,
    pub delta: f64,
    pub over_pace: bool,
    pub today: Option<f64>,
    pub today_since: Option<f64>,
    pub today_over: bool,
    pub allowance: f64,
    pub hot: bool,
    pub warn: bool,
    pub level: &'static str,
}

pub fn pace(m: &Meter, baseline: Option<&Baseline>, now: f64) -> Option<Pace> {
    let win = window_of(&m.key)?;
    let resets = m.resets_at.filter(|r| *r != 0.0)?;
    let start = resets - win.seconds;
    let elapsed = (now - start).max(0.0).min(win.seconds);
    let left = (resets - now).max(0.0);
    let target = 100.0 / (win.seconds / win.unit_seconds);
    let elapsed_units = elapsed / win.unit_seconds;
    let left_units = left / win.unit_seconds;
    let expected = (elapsed_units * target).min(100.0);
    let avg = m.pct / elapsed_units.max(if win.unit == "day" { 0.1 } else { 0.25 });
    let budget = (left_units > 0.01).then(|| (100.0 - m.pct) / left_units);
    let over_pace = m.pct > expected + 2.0;

    let (mut today, mut today_over, mut today_since, mut allowance) = (None, false, None, target);
    if let Some(b) = baseline.filter(|b| win.unit == "day" && b.day == local_day(now)) {
        let t = (m.pct - b.pct).max(0.0);
        today = Some(t);
        today_since = Some(b.at);
        let days_left_at_start = (resets - b.at) / DAY;
        if days_left_at_start > 0.05 {
            allowance = ((100.0 - b.pct) / days_left_at_start.max(1.0)).min(100.0);
        }
        today_over = t > allowance;
    }
    let ahead = m.pct - expected;
    let hourly = win.unit == "h";
    let hot = if today.is_some() { today_over } else if hourly { ahead > 20.0 } else { over_pace };
    let warn = !hot && match today {
        Some(t) => t >= 0.7 * allowance,
        None => hourly && ahead > 10.0,
    };
    Some(Pace {
        unit: win.unit,
        target,
        expected,
        avg,
        budget,
        left_units,
        left_pct: 100.0 - m.pct,
        left_text: dur(left),
        delta: ahead,
        over_pace,
        today,
        today_since,
        today_over,
        allowance,
        hot,
        warn,
        level: if hot { "hot" } else if warn { "warn" } else { "ok" },
    })
}

fn dur(sec: f64) -> String {
    let m = (sec / 60.0).round().max(0.0) as i64;
    let (d, h) = (m / 1440, (m % 1440) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {}m", m % 60)
    } else {
        format!("{}m", m % 60)
    }
}

pub fn f1(n: f64) -> String {
    format!("{:.1}", (n * 10.0).round() / 10.0)
}

fn dot(p: &Pace) -> &'static str {
    if p.unit == "h" {
        if p.hot { "🔴" } else if p.warn { "🟠" } else { "🟢" }
    } else if p.over_pace { "🔴" } else { "🟢" }
}

/// Lines for the tray menu submenu.
pub fn pace_lines(p: Option<&Pace>) -> Vec<String> {
    let Some(p) = p else { return vec![] };
    let u = p.unit;
    let mut out = vec![format!(
        "{} Avg {}%/{u} vs target {}%/{u} ({}{} pts vs pace)",
        dot(p), f1(p.avg), f1(p.target), if p.delta >= 0.0 { "+" } else { "" }, f1(p.delta)
    )];
    if let Some(t) = p.today {
        let d = if p.today_over { "🔴" } else if p.warn { "🟠" } else { "🟢" };
        out.push(format!("{d} Today +{}% · allowed {}% (100/7 = {}%)", f1(t), f1(p.allowance), f1(p.target)));
    }
    if let Some(b) = p.budget {
        let rate = if u == "day" && p.left_units >= 1.0 { format!(" (~{}%/day)", f1(b)) } else { String::new() };
        out.push(format!("Left {}% for {}{rate}", f1(p.left_pct), p.left_text));
    }
    out
}

/// Compact lines for the 127-char Windows tray tooltip.
pub fn tip_lines(p: Option<&Pace>) -> Vec<String> {
    let Some(p) = p else { return vec![] };
    let u = if p.unit == "day" { "d" } else { "h" };
    let mut out = vec![format!("{} Avg {}%/{u} · target {}", dot(p), f1(p.avg), f1(p.target))];
    if let Some(t) = p.today {
        let d = if p.today_over { "🔴" } else if p.warn { "🟠" } else { "🟢" };
        out.push(format!("{d} Today +{}% (max {})", f1(t), f1(p.allowance)));
    }
    if let Some(b) = p.budget {
        let rate = if p.unit == "day" && p.left_units >= 1.0 { format!(" ({}%/d)", f1(b)) } else { String::new() };
        out.push(format!("Left {}% · {}{rate}", f1(p.left_pct), p.left_text));
    }
    out
}

/// Windows truncates tray tooltips at 127 UTF-16 units; drop whole lines instead.
pub fn fit_tip(lines: &[String]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for l in lines {
        let next_len: usize = out.iter().chain(std::iter::once(&l.as_str())).map(|s| s.encode_utf16().count()).sum::<usize>() + out.len();
        if next_len > 127 {
            break;
        }
        out.push(l);
    }
    if out.is_empty() {
        lines.first().map(|l| l.chars().take(120).collect()).unwrap_or_default()
    } else {
        out.join("\n")
    }
}

// --------------------------------------------------------------- daylog.js

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DayLog {
    #[serde(rename = "resetsAt")]
    pub resets_at: f64,
    pub days: BTreeMap<String, f64>,
    pub top: f64,
}

/// Keep the highest pct seen per local day of the current weekly window.
pub fn update_day_log(logs: &mut BTreeMap<String, DayLog>, m: &Meter, now: f64) {
    let Some(r) = m.resets_at.filter(|r| *r != 0.0) else { return };
    if window_of(&m.key).map(|w| w.unit) != Some("day") {
        return;
    }
    let log = logs.entry(m.key.clone()).or_insert_with(|| DayLog { resets_at: r, days: BTreeMap::new(), top: m.pct });
    if (r - log.resets_at).abs() >= 1800.0 || m.pct < log.top - 0.5 {
        *log = DayLog { resets_at: r, days: BTreeMap::new(), top: m.pct };
    }
    let e = log.days.entry(local_day(now)).or_insert(0.0);
    *e = e.max(m.pct);
    log.top = log.top.max(m.pct);
}

#[derive(Clone, Debug, Serialize)]
pub struct Seg {
    pub days: Vec<f64>,
    pub from: f64,
    pub to: f64,
    pub kind: &'static str,
    #[serde(rename = "noData", skip_serializing_if = "Option::is_none")]
    pub no_data: Option<bool>,
}

/// Weekly bar split by usage: each past day = what it used, today = budget,
/// future days share what is left. None without a day baseline.
pub fn week_segments(m: &Meter, p: Option<&Pace>, log: Option<&DayLog>, now: f64) -> Option<Vec<Seg>> {
    let win = window_of(&m.key).filter(|w| w.unit == "day")?;
    let resets = m.resets_at.filter(|r| *r != 0.0)?;
    let p = p?;
    let today = p.today?;
    let n = (win.seconds / DAY).round() as usize;
    let start = resets - n as f64 * DAY;
    let days: Vec<(String, f64)> = (0..n)
        .map(|i| {
            let ts = start + (i as f64 + 0.5) * DAY;
            (local_day(ts), ts)
        })
        .collect();
    let t = days.iter().position(|d| d.0 == local_day(now))?;
    let b = (m.pct - today).max(0.0);
    let valid = log.filter(|l| (resets - l.resets_at).abs() < 1800.0);
    let mut segs = Vec::with_capacity(n);
    let mut from = 0.0_f64;
    for (i, (key, ts)) in days.iter().enumerate().take(t) {
        let end = if i == t - 1 { Some(b) } else { valid.and_then(|l| l.days.get(key).copied()) };
        let to = end.map_or(from, |e| b.min(from.max(e)));
        segs.push(Seg { days: vec![*ts], from, to, kind: "past", no_data: Some(end.is_none()) });
        from = to;
    }
    let budget_end = (b + p.allowance).min(100.0);
    segs.push(Seg { days: vec![days[t].1], from: b, to: budget_end, kind: "today", no_data: None });
    let nf = n - t - 1;
    let each = if nf > 0 { (100.0 - budget_end).max(0.0) / nf as f64 } else { 0.0 };
    for i in 0..nf {
        let a = budget_end + i as f64 * each;
        segs.push(Seg { days: vec![days[t + 1 + i].1], from: a, to: a + each, kind: "future", no_data: None });
    }
    Some(segs)
}

// --------------------------------------------------------------- format.js

pub fn severity(pct: f64, warn_at: f64, danger_at: f64) -> &'static str {
    if pct >= danger_at { "danger" } else if pct >= warn_at { "warn" } else { "normal" }
}

pub fn chosen_meters<'a>(meters: &'a [Meter], show: &[String]) -> Vec<&'a Meter> {
    let picked: Vec<&Meter> = show.iter().filter_map(|k| meters.iter().find(|m| &m.key == k)).collect();
    if !picked.is_empty() {
        return picked;
    }
    let known: Vec<&Meter> = meters.iter().filter(|m| m.known).collect();
    if known.is_empty() { meters.iter().collect() } else { known }
}

pub fn tooltip(chosen: &[&Meter], stale: bool) -> String {
    if chosen.is_empty() {
        return "Claude Usage Meter — no data yet".into();
    }
    let mut lines = vec!["Claude Usage Meter".to_string()];
    lines.extend(chosen.iter().map(|m| format!("{}: {}%", m.label, m.pct.round())));
    if stale {
        lines.push("(last reading is stale)".into());
    }
    lines.join("\n")
}

pub fn icon_state(chosen: &[&Meter], stale: bool, warn_at: f64, danger_at: f64) -> &'static str {
    if stale || chosen.is_empty() {
        return "stale";
    }
    let worst = chosen.iter().map(|m| m.pct).fold(0.0, f64::max);
    severity(worst, warn_at, danger_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: f64 = 1750000000.0;
    const T: f64 = 1793750000.0;

    fn m(key: &str, pct: f64, r: f64) -> Meter {
        Meter::new(key.into(), label_of(key).unwrap().into(), pct, Some(r), "x".into())
    }
    fn base(pct: f64, at: f64) -> Baseline {
        Baseline { day: local_day(T), pct, at, resets_at: None }
    }

    #[test]
    fn parse_nested_payload() {
        let ms = extract_meters(&json!({
            "five_hour": { "utilization": 42, "resets_at": "2026-08-30T18:00:00+00:00" },
            "seven_day": { "utilization": 18, "resets_at": "2026-09-02T09:00:00+00:00" },
            "seven_day_opus": { "utilization": 3, "resets_at": "2026-09-02T09:00:00+00:00" },
            "limits": [
                { "type": "weekly_scoped", "model": "fable", "utilization": 7 },
                { "type": "weekly_scoped", "model": "opus", "utilization": 3 }
            ]
        }));
        let keys: Vec<_> = ms.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, ["session", "weekly", "weekly_opus", "fable"]);
        assert_eq!(ms[0].short, "S");
        assert_eq!(ms[3].pct, 7.0);
    }

    #[test]
    fn pick_pct_rule_and_counts() {
        assert_eq!(extract_meters(&json!({"five_hour": {"utilization": 1}}))[0].pct, 1.0);
        assert_eq!(extract_meters(&json!({"five_hour": {"used": 25, "limit": 200}}))[0].pct, 12.5);
    }

    #[test]
    fn shallow_wins_unknown_survives_malformed_empty() {
        let ms = extract_meters(&json!({"five_hour": {"utilization": 40}, "nested": {"deep": {"five_hour": {"utilization": 99}}}}));
        assert_eq!(ms.iter().find(|m| m.key == "session").unwrap().pct, 40.0);
        let ms = extract_meters(&json!({"mystery_window": {"utilization": 5}}));
        assert_eq!(ms.len(), 1);
        assert!(!ms[0].known);
        assert!(extract_meters(&Value::Null).is_empty());
        assert!(extract_meters(&json!({})).is_empty());
        assert!(extract_meters(&json!({"five_hour": {"utilization": "x"}})).is_empty());
    }

    #[test]
    fn fable_from_limits() {
        let ms = extract_meters(&json!({
            "five_hour": { "utilization": 41, "resets_at": "2026-11-10T12:19:59+00:00" },
            "seven_day": { "utilization": 73, "resets_at": "2026-11-10T22:59:59+00:00" },
            "seven_day_opus": null,
            "extra_usage": { "utilization": null },
            "spend": { "used": { "amount_minor": 0 }, "limit": null, "percent": 0 },
            "limits": [
                { "kind": "session", "group": "session", "percent": 41, "resets_at": "2026-11-10T12:19:59+00:00", "scope": null },
                { "kind": "weekly_all", "group": "weekly", "percent": 73, "resets_at": "2026-11-10T22:59:59+00:00", "scope": null },
                { "kind": "weekly_scoped", "group": "weekly", "percent": 11, "resets_at": "2026-11-10T22:59:59+00:00", "scope": { "model": { "id": null, "display_name": "Fable" }, "surface": null } }
            ]
        }));
        let got: Vec<_> = ms.iter().map(|m| (m.key.as_str(), m.pct)).collect();
        assert_eq!(got, [("session", 41.0), ("weekly", 73.0), ("fable", 11.0)]);
        assert_eq!(ms[2].path, "limits[2]");
        assert!(ms[2].resets_at.unwrap() > 0.0);
    }

    #[test]
    fn history_resets_and_rate() {
        let mut h = History::default();
        h.add(50.0, Some(NOW), 120.0, NOW);
        assert!(!h.add(51.0, Some(NOW + 7.0), 120.0, NOW + 60.0));
        let mut h = History::default();
        h.add(50.0, Some(NOW), 120.0, NOW);
        assert!(h.add(48.0, Some(NOW + 5.0 * 3600.0), 120.0, NOW + 120.0));
        let mut h = History::default();
        h.add(80.0, Some(NOW), 120.0, NOW);
        assert!(h.add(2.0, Some(NOW + 10.0), 120.0, NOW + 60.0));
        let mut h = History::default();
        h.add(10.0, Some(NOW), 120.0, NOW);
        h.add(12.0, Some(NOW), 120.0, NOW + 60.0);
        assert_eq!(h.rate_per_hour(), None);
        let mut h = History::default();
        for i in 0..6 {
            h.add(10.0 + i as f64 * 4.0, Some(NOW), 120.0, NOW - 3600.0 + i as f64 * 600.0);
        }
        assert_eq!(h.rate_per_hour().unwrap().round(), 24.0);
        assert!(forecast(30.0, &h, Some(NOW + 900.0), NOW).contains("resets first"));
        assert!(forecast(30.0, &h, Some(NOW + 4.0 * 3600.0), NOW).contains("at current pace"));
    }

    #[test]
    fn pace_cases() {
        let p = pace(&m("weekly", 87.0, T + 0.35 * DAY), None, T).unwrap();
        assert!((p.target - 14.2857).abs() < 1e-3);
        assert!((p.expected - 95.0).abs() < 0.01);
        assert!(!p.over_pace);
        assert!((p.budget.unwrap() - 13.0 / 0.35).abs() < 0.01);

        let p = pace(&m("weekly", 50.0, T + 5.0 * DAY), None, T).unwrap();
        assert!(p.over_pace);
        assert_eq!(p.avg.round(), 25.0);

        let mut b = BTreeMap::new();
        update_baseline(&mut b, &m("weekly", 30.0, T + 4.0 * DAY), T - 60.0);
        let p = pace(&m("weekly", 50.0, T + 4.0 * DAY), b.get("weekly"), T).unwrap();
        assert!((p.allowance - 70.0 / 4.0).abs() < 0.01);
        assert_eq!(p.today.unwrap().round(), 20.0);
        assert!(p.today_over && p.hot);

        let p = pace(&m("weekly", 38.0, T + 4.5 * DAY), Some(&base(35.0, T - 3600.0)), T).unwrap();
        assert!(p.over_pace);
        assert_eq!(p.level, "ok");

        let bl = base(30.0, T - 3600.0);
        let r = T + 5.0 * DAY;
        assert_eq!(pace(&m("weekly", 39.0, r), Some(&bl), T).unwrap().level, "ok");
        assert_eq!(pace(&m("weekly", 40.0, r), Some(&bl), T).unwrap().level, "warn");
        assert_eq!(pace(&m("weekly", 45.0, r), Some(&bl), T).unwrap().level, "hot");

        let mut b = BTreeMap::new();
        update_baseline(&mut b, &m("weekly", 90.0, T), T - 60.0);
        update_baseline(&mut b, &m("weekly", 1.0, T + 7.0 * DAY), T);
        assert_eq!(b["weekly"].pct, 1.0);

        let p = pace(&m("session", 30.0, T + 4.0 * 3600.0), None, T).unwrap();
        assert_eq!((p.unit, p.target, p.over_pace), ("h", 20.0, true));
        let r = T + 4.5 * 3600.0;
        assert_eq!(pace(&m("session", 6.0, r), None, T).unwrap().level, "ok");
        assert_eq!(pace(&m("session", 25.0, r), None, T).unwrap().level, "warn");
        assert_eq!(pace(&m("session", 31.0, r), None, T).unwrap().level, "hot");
    }

    #[test]
    fn tooltip_fits_127() {
        let p = pace(&m("weekly", 87.0, T + 0.35 * DAY), Some(&base(70.0, T - 3600.0)), T);
        let mut lines = vec!["Weekly — 87%, resets Wed, 4 Nov, 00:00".to_string()];
        lines.extend(tip_lines(p.as_ref()));
        assert!(lines.join("\n").encode_utf16().count() <= 127);
        assert_eq!(fit_tip(&lines), lines.join("\n"));
    }

    #[test]
    fn week_segments_per_day_with_no_data_gaps() {
        // Window started 3.5 days ago: today is index 3 (or 4 across DST/midnight edges).
        let r = T + 3.5 * DAY;
        let meter = m("weekly", 40.0, r);
        let bl = base(30.0, T - 3600.0);
        let p = pace(&meter, Some(&bl), T).unwrap();
        let start = r - 7.0 * DAY;
        let mut days = BTreeMap::new();
        days.insert(local_day(start + 0.5 * DAY), 12.0);
        let log = DayLog { resets_at: r, days, top: 40.0 };
        let segs = week_segments(&meter, Some(&p), Some(&log), T).unwrap();
        assert_eq!(segs.len(), 7);
        let t = segs.iter().position(|s| s.kind == "today").unwrap();
        assert!(segs[..t].iter().all(|s| s.kind == "past"));
        assert_eq!(segs[0].to, 12.0);
        assert_eq!(segs[0].no_data, Some(false));
        assert_eq!(segs[t - 1].to, 30.0); // yesterday ends at "used before today"
        if t >= 3 {
            assert_eq!(segs[1].no_data, Some(true));
            assert_eq!(segs[1].from, segs[1].to);
        }
        assert!((segs.last().unwrap().to - 100.0).abs() < 1e-9);
    }

    #[test]
    fn reset_day_label() {
        assert!(reset_date_time(Some(T), true).contains(':'));
        assert_eq!(reset_date_time(None, true), "unknown");
    }
}
