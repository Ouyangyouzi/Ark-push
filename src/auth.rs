use crate::state::StateStore;
use base64::Engine;
use parking_lot::RwLock;
use rand::RngCore;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use subtle::ConstantTimeEq;

pub const DEFAULT_USER: &str = "admin";
pub const DEFAULT_PASSWORD: &str = "admin";
pub const PBKDF2_ITERATIONS: u32 = 200_000;
pub const SESSION_TTL: i64 = 7 * 86400;
pub const MAX_SESSIONS: usize = 200;

pub const FAIL_LIMIT: usize = 0;
pub const FAIL_LOCK_SECONDS: i64 = 0;

const EXACT_ALLOW: &[&str] = &[
    "/",
    "/player",
    "/health",
    "/api/health",
    "/api/push",
    "/api/litepan/offline",
    "/api/litepan/list",
    "/api/search",
    "/api/actors",
    "/api/match",
    "/api/actor/dirs",
    "/api/actor/profiles",
    "/api/actor/by-code",
    "/api/actor",
    "/api/play/stream",
    "/userscript",
    "/userscript/latest.user.js",
];

const PREFIX_ALLOW: &[&str] = &["/api/auth/", "/api/ark/", "/avatar/", "/plugins/sdk.js"];

pub fn allowed_without_auth(path: &str) -> bool {
    EXACT_ALLOW.contains(&path) || PREFIX_ALLOW.iter().any(|p| path.starts_with(p))
}

fn hash_password(password: &str, salt: &[u8], iterations: u32) -> String {
    let mut dk = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, iterations, &mut dk);
    format!(
        "pbkdf2${iterations}${}${}",
        hex::encode(salt),
        hex::encode(dk)
    )
}

mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    }
    pub fn decode(s: &str) -> Option<Vec<u8>> {
        if s.len() % 2 != 0 {
            return None;
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
            .collect()
    }
}

pub struct AuthStore {
    state: Arc<StateStore>,
    ttl: i64,
    sessions: RwLock<HashMap<String, i64>>,
    fails: RwLock<HashMap<String, Vec<i64>>>,
    locked_until: RwLock<HashMap<String, i64>>,
}

impl AuthStore {
    pub fn new(state: Arc<StateStore>) -> Self {
        let now = chrono::Utc::now().timestamp();
        let mut sessions = HashMap::new();
        if let Some(map) = state.get_setting("auth_sessions", serde_json::json!({})).as_object() {
            for (token, exp) in map {
                if let Some(exp) = exp.as_i64() {
                    if exp > now {
                        sessions.insert(token.clone(), exp);
                    }
                }
            }
        }
        let store = Self {
            state,
            ttl: SESSION_TTL,
            sessions: RwLock::new(sessions),
            fails: RwLock::new(HashMap::new()),
            locked_until: RwLock::new(HashMap::new()),
        };
        if store.state.auth_creds().as_object().map(|m| m.is_empty()).unwrap_or(true) {
            let _ = store.state.set_auth_creds(Some(DEFAULT_USER), None);
        }
        store
    }

    pub fn username(&self) -> String {
        let u = self.state.get_str("auth_user", DEFAULT_USER);
        if u.is_empty() { DEFAULT_USER.to_string() } else { u }
    }

    fn stored_hash(&self) -> String {
        self.state.get_str("auth_password_hash", "")
    }

    pub fn using_default_password(&self) -> bool {
        self.stored_hash().is_empty()
    }

    pub fn verify(&self, username: &str, password: &str) -> bool {
        if username.trim() != self.username() {
            return false;
        }
        let stored = self.stored_hash();
        if stored.is_empty() {
            return constant_eq(password.as_bytes(), DEFAULT_PASSWORD.as_bytes());
        }
        let parts: Vec<&str> = stored.split('$').collect();
        if parts.len() != 4 || parts[0] != "pbkdf2" {
            return false;
        }
        let Ok(iters) = parts[1].parse::<u32>() else {
            return false;
        };
        let Some(salt) = hex::decode(parts[2]) else {
            return false;
        };
        let calc = hash_password(password, &salt, iters);
        constant_eq(calc.as_bytes(), stored.as_bytes())
    }

    pub fn verify_password(&self, password: &str) -> bool {
        self.verify(&self.username(), password)
    }

    fn new_token() -> String {
        let mut buf = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut buf);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
    }

    pub fn issue_session(&self, ttl: Option<i64>) -> String {
        let token = Self::new_token();
        let exp = chrono::Utc::now().timestamp() + ttl.unwrap_or(self.ttl);
        {
            let mut sessions = self.sessions.write();
            sessions.insert(token.clone(), exp);
            Self::prune(&mut sessions);
        }
        self.persist();
        token
    }

    pub fn login(&self, username: &str, password: &str, ip: &str) -> Option<String> {
        let ip = if ip.is_empty() { "unknown" } else { ip };
        let now = chrono::Utc::now().timestamp();
        if self.locked_until.read().get(ip).copied().unwrap_or(0) > now {
            return None;
        }
        if !self.verify(username, password) {
            if FAIL_LIMIT > 0 {
                let mut fails = self.fails.write();
                let list = fails.entry(ip.to_string()).or_default();
                list.retain(|t| now - *t < 300);
                list.push(now);
                if list.len() >= FAIL_LIMIT {
                    self.locked_until
                        .write()
                        .insert(ip.to_string(), now + FAIL_LOCK_SECONDS);
                    fails.insert(ip.to_string(), Vec::new());
                }
            }
            return None;
        }
        self.fails.write().remove(ip);
        self.locked_until.write().remove(ip);
        Some(self.issue_session(None))
    }

    pub fn logout(&self, token: &str) {
        self.sessions.write().remove(token);
        self.persist();
    }

    pub fn check(&self, token: Option<&str>) -> bool {
        let Some(token) = token.filter(|t| !t.is_empty()) else {
            return false;
        };
        let now = chrono::Utc::now().timestamp();
        let mut sessions = self.sessions.write();
        match sessions.get(token) {
            Some(exp) if *exp >= now => true,
            Some(_) => {
                sessions.remove(token);
                drop(sessions);
                self.persist();
                false
            }
            None => false,
        }
    }

    pub fn locked_remaining(&self, ip: &str) -> i64 {
        let now = chrono::Utc::now().timestamp();
        (self
            .locked_until
            .read()
            .get(if ip.is_empty() { "unknown" } else { ip })
            .copied()
            .unwrap_or(0)
            - now)
            .max(0)
    }

    pub fn set_credentials(&self, username: Option<&str>, password: Option<&str>) {
        let mut patch = serde_json::Map::new();
        if let Some(user) = username.map(str::trim).filter(|s| !s.is_empty()) {
            patch.insert("auth_user".into(), serde_json::Value::String(user.to_string()));
        }
        let mut new_hash = None;
        if let Some(pwd) = password.filter(|s| !s.is_empty()) {
            let mut salt = [0u8; 16];
            rand::rngs::OsRng.fill_bytes(&mut salt);
            let h = hash_password(pwd, &salt, PBKDF2_ITERATIONS);
            patch.insert(
                "auth_password_hash".into(),
                serde_json::Value::String(h.clone()),
            );
            new_hash = Some(h);
        }
        if patch.is_empty() {
            return;
        }
        self.state.set_settings(patch);
        let _ = self.state.set_auth_creds(username, new_hash.as_deref());
        self.sessions.write().clear();
        self.fails.write().clear();
        self.locked_until.write().clear();
        self.persist();
    }

    fn prune(sessions: &mut HashMap<String, i64>) {
        let now = chrono::Utc::now().timestamp();
        sessions.retain(|_, exp| *exp > now);
        if sessions.len() > MAX_SESSIONS {
            let mut items: Vec<(String, i64)> = sessions.drain().collect();
            items.sort_by_key(|(_, exp)| -(*exp));
            items.truncate(MAX_SESSIONS);
            sessions.extend(items);
        }
    }

    fn persist(&self) {
        let map = self.sessions.read().clone();
        let value = serde_json::to_value(map).unwrap_or_else(|_| serde_json::json!({}));
        self.state.set_setting("auth_sessions", value);
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

pub fn cookie_token(cookie_header: Option<&str>) -> Option<String> {
    let raw = cookie_header?;
    for part in raw.split(';') {
        let Some((k, v)) = part.trim().split_once('=') else {
            continue;
        };
        if k == "lps_session" && !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    None
}

pub fn token_from_headers(headers: &axum::http::HeaderMap, query_token: Option<&str>) -> Option<String> {
    if let Some(tok) = headers
        .get("X-Auth-Token")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(tok.to_string());
    }
    if let Some(tok) = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| cookie_token(Some(h)))
    {
        return Some(tok);
    }
    query_token
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}
