use crate::config::now_secs;
use crate::util::{atomic_write, read_json_or_default};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateData {
    #[serde(default = "one")]
    pub version: i64,
    #[serde(default)]
    pub files: Map<String, Value>,
    #[serde(default)]
    pub last_sync: i64,
    #[serde(default)]
    pub last_report: Value,
    #[serde(default)]
    pub search_key: String,
    #[serde(default)]
    pub docs: i64,
    #[serde(default)]
    pub settings: Map<String, Value>,
}

fn one() -> i64 {
    1
}

impl Default for StateData {
    fn default() -> Self {
        Self {
            version: 1,
            files: Map::new(),
            last_sync: 0,
            last_report: json!({}),
            search_key: String::new(),
            docs: 0,
            settings: Map::new(),
        }
    }
}

pub struct StateStore {
    pub dir: PathBuf,
    pub path: PathBuf,
    pub creds_dir: PathBuf,
    data: RwLock<StateData>,
}

impl StateStore {
    pub fn new(state_dir: &str) -> Self {
        let dir = PathBuf::from(state_dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("state.json");
        let creds_dir = dir.join("creds");
        let _ = fs::create_dir_all(&creds_dir);
        let mut loaded: StateData = serde_json::from_value(read_json_or_default(
            &path,
            serde_json::to_value(StateData::default()).unwrap(),
        ))
        .unwrap_or_default();
        if loaded.version == 0 {
            loaded.version = 1;
        }
        let store = Self {
            dir,
            path,
            creds_dir,
            data: RwLock::new(loaded),
        };
        store.load_litepan_creds();
        store.load_auth_creds();
        store
    }

    pub fn save(&self) {
        let payload = {
            let d = self.data.read();
            serde_json::to_value(&*d).unwrap_or_else(|_| json!({}))
        };
        if let Err(e) = crate::util::write_json(&self.path, &payload) {
            tracing::error!("???:{e}");
        }
    }

    pub fn snapshot(&self) -> StateData {
        self.data.read().clone()
    }

    pub fn files(&self) -> Map<String, Value> {
        self.data.read().files.clone()
    }

    pub fn docs(&self) -> i64 {
        self.data.read().docs
    }

    pub fn report(&self) -> Value {
        self.data.read().last_report.clone()
    }

    pub fn summary(&self) -> Value {
        let d = self.data.read();
        json!({
            "tracked_files": d.files.len(),
            "docs": d.docs,
            "last_sync": d.last_sync,
            "last_sync_iso": if d.last_sync > 0 { chrono::DateTime::from_timestamp(d.last_sync, 0).map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default() } else { String::new() },
            "last_report": d.last_report,
        })
    }

    pub fn settings_snapshot(&self) -> Map<String, Value> {
        self.data.read().settings.clone()
    }

    pub fn get_setting(&self, key: &str, default: Value) -> Value {
        self.data
            .read()
            .settings
            .get(key)
            .cloned()
            .unwrap_or(default)
    }

    pub fn get_str(&self, key: &str, default: &str) -> String {
        self.get_setting(key, Value::String(default.to_string()))
            .as_str()
            .unwrap_or(default)
            .to_string()
    }

    pub fn get_bool(&self, key: &str, default: Option<bool>) -> Option<bool> {
        match self.data.read().settings.get(key) {
            Some(Value::Bool(b)) => Some(*b),
            Some(Value::Null) => default,
            Some(Value::Number(n)) => Some(n.as_i64().unwrap_or(0) != 0),
            _ => default,
        }
    }

    pub fn set_settings(&self, patch: Map<String, Value>) -> Map<String, Value> {
        let merged = {
            let mut d = self.data.write();
            for (k, v) in patch {
                d.settings.insert(k, v);
            }
            d.settings.clone()
        };
        self.save();
        merged
    }

    pub fn set_setting(&self, key: &str, value: Value) {
        let mut patch = Map::new();
        patch.insert(key.to_string(), value);
        self.set_settings(patch);
    }

    pub fn record_sync(&self, files: Map<String, Value>, report: Value) {
        {
            let mut d = self.data.write();
            d.files = files;
            d.last_sync = now_secs();
            d.last_report = report.clone();
            d.docs = report
                .get("docs_total")
                .and_then(Value::as_i64)
                .unwrap_or(0);
        }
        self.save();
    }

    pub fn set_docs(&self, docs: i64) {
        self.data.write().docs = docs;
    }

    // ---------------------------------------------------------------- creds
    fn creds_path(&self, name: &str) -> PathBuf {
        self.creds_dir.join(name)
    }

    fn read_creds(&self, name: &str) -> Option<Value> {
        crate::util::read_json(&self.creds_path(name))
    }

    fn write_creds(&self, name: &str, payload: &Value) {
        let path = self.creds_path(name);
        let raw = serde_json::to_vec(payload).unwrap_or_default();
        if let Err(e) = atomic_write(&path, &raw, Some(0o600)) {
            tracing::error!("({}):{e}", path.display());
        }
    }

    fn load_litepan_creds(&self) {
        let Some(saved) = self.read_creds("litepan.json") else {
            // ?state.json 
            let url = self.get_str("litepan_url", "");
            let pwd = self.get_str("litepan_password", "");
            if !url.is_empty() && !pwd.is_empty() {
                let user = self.get_str("litepan_username", "admin");
                let insecure = self.get_setting("litepan_insecure", Value::Null);
                self.set_litepan_creds(
                    &url,
                    if user.is_empty() { "admin" } else { &user },
                    &pwd,
                    insecure.as_bool(),
                );
                tracing::info!(" state.json  LitePan ?creds/litepan.json");
            }
            return;
        };
        let mut patch = Map::new();
        for (src, dst) in [
            ("url", "litepan_url"),
            ("username", "litepan_username"),
            ("password", "litepan_password"),
        ] {
            if let Some(v) = saved.get(src).and_then(Value::as_str) {
                if !v.trim().is_empty() {
                    patch.insert(
                        dst.to_string(),
                        Value::String(if src == "password" {
                            v.to_string()
                        } else {
                            v.trim().to_string()
                        }),
                    );
                }
            }
        }
        if let Some(v) = saved.get("insecure").and_then(Value::as_bool) {
            patch.insert("litepan_insecure".to_string(), Value::Bool(v));
        }
        if !patch.is_empty() {
            self.set_settings(patch);
            tracing::info!("?LitePan (creds/litepan.json)");
        }
    }

    pub fn set_litepan_creds(&self, url: &str, username: &str, password: &str, insecure: Option<bool>) {
        let mut patch = Map::new();
        patch.insert("litepan_url".into(), Value::String(url.to_string()));
        patch.insert("litepan_username".into(), Value::String(username.to_string()));
        patch.insert("litepan_password".into(), Value::String(password.to_string()));
        if let Some(v) = insecure {
            patch.insert("litepan_insecure".into(), Value::Bool(v));
        }
        self.set_settings(patch);
        let mut payload = json!({
            "version": 1,
            "url": url,
            "username": username,
            "password": password,
            "updated_at": now_secs(),
        });
        if let Some(v) = insecure {
            payload["insecure"] = Value::Bool(v);
        }
        self.write_creds("litepan.json", &payload);
    }

    pub fn match_config(&self) -> Value {
        self.read_creds("match.json")
            .and_then(|v| v.get("match").cloned())
            .unwrap_or_else(|| json!({}))
    }

    pub fn set_match_config(&self, match_cfg: Value) {
        self.write_creds(
            "match.json",
            &json!({"version": 1, "match": match_cfg, "updated_at": now_secs()}),
        );
    }

    pub fn auth_creds(&self) -> Value {
        self.read_creds("auth.json").unwrap_or_else(|| json!({}))
    }

    pub fn set_auth_creds(&self, username: Option<&str>, password_hash: Option<&str>) {
        let cur = self.auth_creds();
        let user = username
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| cur.get("username").and_then(Value::as_str).map(str::to_string))
            .or_else(|| {
                let s = self.get_str("auth_user", "");
                if s.is_empty() { None } else { Some(s) }
            })
            .unwrap_or_default();
        let hash = password_hash
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| cur.get("password_hash").and_then(Value::as_str).map(str::to_string))
            .or_else(|| {
                let s = self.get_str("auth_password_hash", "");
                if s.is_empty() { None } else { Some(s) }
            });
        let mut payload = json!({
            "version": 1,
            "username": user,
            "updated_at": now_secs(),
        });
        if let Some(h) = hash {
            payload["password_hash"] = Value::String(h);
        }
        self.write_creds("auth.json", &payload);
    }

    fn load_auth_creds(&self) {
        let data = self.auth_creds();
        let mut patch = Map::new();
        if let Some(user) = data.get("username").and_then(Value::as_str) {
            if !user.trim().is_empty() {
                patch.insert("auth_user".into(), Value::String(user.trim().to_string()));
            }
        }
        if let Some(hash) = data.get("password_hash").and_then(Value::as_str) {
            if !hash.trim().is_empty() {
                patch.insert("auth_password_hash".into(), Value::String(hash.trim().to_string()));
            }
        }
        if !patch.is_empty() {
            self.set_settings(patch);
            tracing::info!("(creds/auth.json)");
            return;
        }
        let user = self.get_str("auth_user", "");
        let hash = self.get_str("auth_password_hash", "");
        if !user.is_empty() || !hash.is_empty() {
            self.set_auth_creds(
                if user.is_empty() { None } else { Some(&user) },
                if hash.is_empty() { None } else { Some(&hash) },
            );
        }
    }

    pub fn creds_path_for(&self, name: &str) -> PathBuf {
        self.creds_path(name)
    }

    pub fn read_creds_value(&self, name: &str) -> Option<Value> {
        self.read_creds(name)
    }

    pub fn state_dir(&self) -> &Path {
        &self.dir
    }
}
