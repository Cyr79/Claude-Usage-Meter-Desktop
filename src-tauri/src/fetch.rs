//! claude.ai usage over plain HTTPS (reqwest + rustls). The session cookies are
//! captured from the sign-in webview once, then kept DPAPI-encrypted on disk:
//! the same protection Chromium/Electron gives its own cookie store.

use crate::platform;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

pub const ORIGIN: &str = "https://claude.ai";
/// One fixed browser UA for both the webview and reqwest, so a Cloudflare
/// clearance cookie issued to the webview stays valid for our requests.
pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36 Edg/142.0.0.0";

#[derive(Debug)]
pub enum Error {
    /// Server really rejected the session: sign in again.
    Auth(String),
    /// HTTP 429, with Retry-After seconds when given.
    RateLimited(Option<u64>),
    /// HTML instead of JSON: bot check in the way.
    Blocked(String),
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Auth(s) | Error::Blocked(s) | Error::Other(s) => f.write_str(s),
            Error::RateLimited(_) => f.write_str("Rate limited by claude.ai (HTTP 429). Backing off."),
        }
    }
}

fn safe_id(s: &str) -> bool {
    (8..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub struct Fetcher {
    client: reqwest::Client,
    cookies: Option<String>,
    file: PathBuf,
    pub diag: Value,
}

struct Resp {
    status: u16,
    body: Option<Value>,
    retry_after: Option<u64>,
    sample: String,
}

impl Fetcher {
    pub fn new(dir: PathBuf) -> Fetcher {
        let file = dir.join("session.bin");
        let cookies = std::fs::read(&file)
            .ok()
            .and_then(|b| platform::protect(&b, false))
            .and_then(|b| String::from_utf8(b).ok())
            .filter(|s| !s.is_empty());
        let client = reqwest::Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(30))
            .https_only(true)
            .build()
            .expect("http client");
        Fetcher { client, cookies, file, diag: json!({}) }
    }

    pub fn has_session(&self) -> bool {
        self.cookies.is_some()
    }

    /// Cookie header from the webview's claude.ai cookies. Returns false if no sessionKey.
    pub fn set_cookies(&mut self, pairs: &[(String, String)]) -> bool {
        if !pairs.iter().any(|(n, _)| n == "sessionKey") {
            return false;
        }
        let header = pairs
            .iter()
            // Header injection guard: cookie values never contain these.
            .filter(|(n, v)| !(n.contains(['\r', '\n', ';']) || v.contains(['\r', '\n', ';'])))
            .map(|(n, v)| format!("{n}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        self.cookies = Some(header);
        true
    }

    pub fn persist(&self) {
        if let Some(c) = &self.cookies {
            if let Some(enc) = platform::protect(c.as_bytes(), true) {
                if let Some(d) = self.file.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let _ = std::fs::write(&self.file, enc);
            }
        }
    }

    async fn get(&self, path: &str) -> Result<Resp, Error> {
        let Some(cookies) = &self.cookies else { return Err(Error::Auth("Not signed in.".into())) };
        let r = self
            .client
            .get(format!("{ORIGIN}{path}"))
            .header("Accept", "application/json")
            .header("Cookie", cookies)
            .header("Referer", format!("{ORIGIN}/"))
            .header("Origin", ORIGIN)
            .send()
            .await
            .map_err(|e| Error::Other(format!("Network error: {e}")))?;
        let status = r.status().as_u16();
        let retry_after = r
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        let text = r.text().await.map_err(|e| Error::Other(format!("Network error: {e}")))?;
        let body = serde_json::from_str(&text).ok();
        Ok(Resp { status, body, retry_after, sample: text.chars().take(300).collect() })
    }

    async fn checked(&mut self, path: &str, what: &str) -> Result<Value, Error> {
        let r = self.get(path).await?;
        self.diag[what] = json!({ "status": r.status, "sample": r.sample });
        match (r.status, r.body) {
            (429, _) => Err(Error::RateLimited(r.retry_after)),
            (401, _) => Err(Error::Auth(format!("Session rejected by {what} (HTTP 401)."))),
            (_, None) => Err(Error::Blocked(format!("claude.ai returned a non-JSON page for {what} (HTTP {}).", r.status))),
            (403, Some(_)) => Err(Error::Auth(format!("Session rejected by {what} (HTTP 403)."))),
            (200, Some(b)) => Ok(b),
            (s, _) => Err(Error::Other(format!("{what} returned HTTP {s}."))),
        }
    }

    /// Org id: cached/pinned in the store unless `force`.
    pub async fn resolve_org(&mut self, cached: Option<&str>, force: bool) -> Result<String, Error> {
        if let Some(id) = cached.filter(|id| !force && safe_id(id)) {
            return Ok(id.to_string());
        }
        let orgs = self.checked("/api/organizations", "organizations").await?;
        let orgs = orgs.as_array().filter(|a| !a.is_empty()).ok_or_else(|| Error::Auth("No organizations returned — session looks signed out.".into()))?;
        let score = |o: &Value| {
            let caps: Vec<String> = o["capabilities"].as_array().map(|a| a.iter().map(|c| c.as_str().unwrap_or("").to_lowercase()).collect()).unwrap_or_default();
            (if caps.iter().any(|c| c == "chat") { 2 } else { 0 }) - (if caps.iter().any(|c| c == "api") { 1 } else { 0 })
        };
        let best = orgs.iter().fold(&orgs[0], |a, b| if score(b) > score(a) { b } else { a });
        let id = best["uuid"].as_str().or(best["id"].as_str()).unwrap_or("").to_string();
        // It goes into a URL path: accept only an id shape.
        if !safe_id(&id) {
            return Err(Error::Other("Unexpected organization id format.".into()));
        }
        Ok(id)
    }

    pub async fn usage(&mut self, org: &str) -> Result<Value, Error> {
        let v = self.checked(&format!("/api/organizations/{org}/usage"), "usage").await?;
        self.diag["mode"] = json!("direct");
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn id_shape() {
        assert!(super::safe_id("1234abcd-ef00-1111-2222-333344445555"));
        assert!(!super::safe_id("../../etc"));
        assert!(!super::safe_id("short"));
    }
}
