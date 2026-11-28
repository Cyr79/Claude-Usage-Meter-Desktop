//! Tiny JSON store, same file format and keys as the Electron build.

use serde_json::{json, Map, Value};
use std::fs;
use std::path::PathBuf;

fn defaults() -> Map<String, Value> {
    let v = json!({
        "pollSeconds": 120,
        "show": ["session", "weekly", "fable"],
        "titleStyle": "colon",
        "macTrayIcon": "logo",
        "warnAt": 80,
        "dangerAt": 95,
        "forecastWindowMinutes": 120,
        "orgUuid": null,
        "startAtLogin": true,
        "windowsTrayMode": "per-meter",
        "firstRunDone": false,
        "paceColours": true,
        "dayBaselines": {},
        "dayLogs": {},
        "showContext": true,
        "contextBadge": true,
        "contextLimit": "auto",
        "gaugePinned": false,
        "gaugePos": null,
        "settingsVersion": 2
    });
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

pub struct Store {
    file: PathBuf,
    data: Map<String, Value>,
    /// True when this run copied the Electron config (first launch of this build).
    pub migrated: bool,
}

/// Electron's userData for productName "Claude Usage Meter".
fn electron_config() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("Claude Usage Meter").join("config.json"))
}

fn read(file: &PathBuf) -> Option<Map<String, Value>> {
    match serde_json::from_slice(&fs::read(file).ok()?).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

impl Store {
    pub fn load(dir: PathBuf) -> Store {
        let file = dir.join("config.json");
        let mut migrated = false;
        let raw = read(&file).or_else(|| {
            let old = read(&electron_config()?)?;
            migrated = true;
            Some(old)
        });
        let mut data = defaults();
        let had = raw.is_some();
        for (k, v) in raw.unwrap_or_default() {
            data.insert(k, v);
        }
        let s = Store { file, data, migrated };
        if !had || migrated {
            s.save();
        }
        s
    }

    pub fn get(&self, k: &str) -> &Value {
        self.data.get(k).filter(|v| !v.is_null()).unwrap_or(&Value::Null)
    }
    pub fn flag(&self, k: &str) -> bool {
        self.get(k).as_bool().unwrap_or(false)
    }
    pub fn num(&self, k: &str) -> f64 {
        self.get(k).as_f64().unwrap_or(0.0)
    }
    pub fn str(&self, k: &str) -> &str {
        self.get(k).as_str().unwrap_or("")
    }
    pub fn show(&self) -> Vec<String> {
        self.get("show")
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    }

    pub fn set(&mut self, k: &str, v: Value) {
        // Polls re-set the same values; don't rewrite the file for nothing.
        if self.data.get(k) == Some(&v) {
            return;
        }
        self.data.insert(k.into(), v);
        self.save();
    }

    fn save(&self) {
        if let Some(d) = self.file.parent() {
            let _ = fs::create_dir_all(d);
        }
        // Write-then-rename so a crash mid-write can't leave a truncated config.
        let tmp = self.file.with_extension("json.tmp");
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.data) {
            if fs::write(&tmp, bytes).is_ok() {
                let _ = fs::rename(&tmp, &self.file);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_then_round_trip() {
        let dir = std::env::temp_dir().join(format!("cum-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store { file: dir.join("config.json"), data: defaults(), migrated: false };
        assert_eq!(s.num("pollSeconds"), 120.0);
        s.set("gaugePinned", json!(true));
        let again = read(&dir.join("config.json")).unwrap();
        assert_eq!(again["gaugePinned"], json!(true));
        assert_eq!(again["show"], json!(["session", "weekly", "fable"]));
        let _ = fs::remove_dir_all(&dir);
    }
}
