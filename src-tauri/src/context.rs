//! Live context fill of the newest Claude Code transcript (port of context.js).
//! One stat() per call in the common case; a folder rescan at most every 20 s;
//! the 512 KB tail is re-read only when the file changed.

use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

const ACTIVE_FOR: f64 = 30.0 * 60.0;
const TAIL_BYTES: u64 = 512 * 1024;
const RESCAN: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ctx {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub active: bool,
    pub idle_sec: f64,
    pub tokens: f64,
    pub limit: f64,
    pub pct: f64,
    pub model: String,
    pub project: String,
}

impl Ctx {
    fn none(reason: &str) -> Self {
        Ctx { reason: Some(reason.into()), ..Default::default() }
    }
}

#[derive(Default)]
pub struct Reader {
    scan_at: Option<Instant>,
    file: Option<(PathBuf, String)>,
    sig: String,
    usage: Option<(f64, String)>,
}

fn projects_dir() -> Option<PathBuf> {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".claude")))?;
    Some(base.join("projects"))
}

fn newest() -> Option<(PathBuf, String)> {
    let mut best: Option<(SystemTime, PathBuf, String)> = None;
    for p in fs::read_dir(projects_dir()?).ok()?.flatten() {
        if !p.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = fs::read_dir(p.path()) else { continue };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(mt) = f.metadata().and_then(|m| m.modified()) {
                if best.as_ref().map_or(true, |b| mt > b.0) {
                    best = Some((mt, path, p.file_name().to_string_lossy().into_owned()));
                }
            }
        }
    }
    best.map(|b| (b.1, b.2))
}

fn read_tail(path: &PathBuf) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let size = f.metadata()?.len();
    f.seek(SeekFrom::Start(size.saturating_sub(TAIL_BYTES)))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Last main-chain assistant usage: input + cache_creation + cache_read.
pub fn last_usage(text: &str) -> Option<(f64, String)> {
    for line in text.lines().rev() {
        if !line.contains("\"usage\"") {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else { continue }; // first line may be cut
        if row.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(msg) = row.get("message") else { continue };
        let Some(u) = msg.get("usage") else { continue };
        let g = |k: &str| u.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        let tokens = g("input_tokens") + g("cache_creation_input_tokens") + g("cache_read_input_tokens");
        if tokens == 0.0 {
            continue;
        }
        return Some((tokens, msg.get("model").and_then(Value::as_str).unwrap_or("").to_string()));
    }
    None
}

/// setting: "auto" or a number. Auto = 200k, or 1M once usage passes 200k.
pub fn context_limit(tokens: f64, model: &str, setting: &Value) -> f64 {
    let n = match setting {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    };
    if let Some(n) = n.filter(|n| *n > 0.0) {
        return n;
    }
    let m = model.to_lowercase();
    if m.contains("[1m]") || m.ends_with("-1m") || m.contains("-1m-") || tokens > 200000.0 {
        1_000_000.0
    } else {
        200_000.0
    }
}

impl Reader {
    pub fn read(&mut self, setting: &Value) -> Ctx {
        if self.file.is_none() || self.scan_at.map_or(true, |t| t.elapsed() >= RESCAN) {
            self.scan_at = Some(Instant::now());
            self.file = newest();
        }
        let Some((path, project)) = self.file.clone() else { return Ctx::none("No Claude Code transcripts found") };
        let Ok(meta) = fs::metadata(&path) else {
            self.file = None;
            return Ctx::none("No Claude Code transcripts found");
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let idle = SystemTime::now().duration_since(mtime).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let sig = format!("{}|{:?}|{}", path.display(), mtime, meta.len());
        if sig != self.sig {
            match read_tail(&path) {
                Ok(t) => self.usage = last_usage(&t),
                Err(e) => return Ctx::none(&e.to_string()),
            }
            self.sig = sig;
        }
        let Some((tokens, model)) = self.usage.clone() else { return Ctx::none("No usage in latest session yet") };
        let limit = context_limit(tokens, &model, setting);
        Ctx {
            available: true,
            reason: None,
            active: idle <= ACTIVE_FOR,
            idle_sec: idle,
            tokens,
            limit,
            pct: (tokens / limit * 100.0).min(100.0),
            model,
            project,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn last_main_chain_usage() {
        let rows = [
            "{\"cut line".to_string(),
            json!({"type":"assistant","message":{"model":"claude-opus-4","usage":{"input_tokens":5,"cache_creation_input_tokens":1000,"cache_read_input_tokens":50000,"output_tokens":300}}}).to_string(),
            json!({"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":9,"cache_read_input_tokens":9}}}).to_string(),
        ]
        .join("\n");
        assert_eq!(last_usage(&rows), Some((51005.0, "claude-opus-4".into())));
    }

    #[test]
    fn limit_auto_detects_1m() {
        assert_eq!(context_limit(150000.0, "x", &json!("auto")), 200000.0);
        assert_eq!(context_limit(250000.0, "x", &json!("auto")), 1000000.0);
        assert_eq!(context_limit(10.0, "x", &json!(200000)), 200000.0);
        assert_eq!(context_limit(10.0, "claude-opus-4[1m]", &json!("auto")), 1000000.0);
    }
}
