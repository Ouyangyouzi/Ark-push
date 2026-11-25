use crate::config::Config;
use crate::state::StateStore;
use crate::util::{atomic_write, posix_basename, read_json};
use futures::StreamExt;
use parking_lot::RwLock;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::{Client as HttpClient, Method, Response, StatusCode};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum LitePanError {
    #[error("{0}")]
    Message(String),
    #[error("鉴权失败:{0}")]
    Auth(String),
    #[error("目录不存在:{0}")]
    NotFound(String),
}

pub type LpResult<T> = Result<T, LitePanError>;

/// 115 CDN 直链签名与 UA 绑定，LitePan 解析与后续 CDN 请求必须使用同一个 UA。
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

fn dig<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let obj = v.as_object()?;
    for k in keys {
        if let Some(val) = obj.get(*k) {
            if !val.is_null() && val.as_str() != Some("") {
                if let Some(arr) = val.as_array() {
                    if arr.is_empty() {
                        continue;
                    }
                }
                if let Some(o) = val.as_object() {
                    if o.is_empty() {
                        continue;
                    }
                }
                return Some(val);
            }
        }
    }
    None
}

fn ark_self_hint(body: &str) -> Option<&'static str> {
    if body.contains("请求体不是合法 JSON")
        || body.contains("未登录或会话已过期")
        || body.contains("\"index_alias\"")
    {
        Some(
            "这个地址指向的是 Ark-push 自己,不是 LitePan。请填写 LitePan 地址(默认 http://host.docker.internal:5211 或 http://宿主机IP:5211);如果用反向代理,反代必须指向 LitePan 的 5211 端口,而不是 Ark-push 的 7701。",
        )
    } else {
        None
    }
}

pub struct LitePanClient {
    pub base: String,
    pub username: String,
    pub password: String,
    pub state_dir: PathBuf,
    pub session_path: PathBuf,
    http: HttpClient,
    jar: Arc<Jar>,
    logged_in: AtomicBool,
    login_lock: tokio::sync::Mutex<()>,
    timeout: Duration,
}

impl LitePanClient {
    pub fn new(
        base: &str,
        username: &str,
        password: &str,
        state_dir: &str,
        verify: bool,
    ) -> LpResult<Self> {
        let base = base.trim_end_matches('/').to_string();
        let jar = Arc::new(Jar::default());
        let http = HttpClient::builder()
            .danger_accept_invalid_certs(!verify)
            .cookie_provider(jar.clone())
            // 115 的 CDN 直链签名绑定 User-Agent。LitePan 解析直链时会把这里的 UA
            // 带进签名，后续跟随 302 到 CDN 必须用同一个 UA，否则会 403 invalid signature。
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(10))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| LitePanError::Message(format!("HTTP ?{e}")))?;
        let session_path = PathBuf::from(state_dir).join("litepan_session.json");
        let client = Self {
            base,
            username: username.to_string(),
            password: password.to_string(),
            state_dir: PathBuf::from(state_dir),
            session_path,
            http,
            jar,
            logged_in: AtomicBool::new(false),
            login_lock: tokio::sync::Mutex::new(()),
            timeout: Duration::from_secs(30),
        };
        client.load_session();
        Ok(client)
    }

    fn base_url(&self) -> Option<Url> {
        Url::parse(&format!("{}/", self.base)).ok()
    }

    fn load_session(&self) {
        let Some(data) = read_json(&self.session_path) else {
            return;
        };
        if data.get("base").and_then(Value::as_str) != Some(self.base.as_str()) {
            return;
        }
        if data.get("username").and_then(Value::as_str) != Some(self.username.as_str()) {
            return;
        }
        let Some(cookies) = data.get("cookies").and_then(Value::as_object) else {
            return;
        };
        let Some(url) = self.base_url() else { return };
        for (name, value) in cookies {
            if let Some(v) = value.as_str() {
                self.jar
                    .add_cookie_str(&format!("{name}={v}; Path=/"), &url);
            }
        }
        self.logged_in.store(true, Ordering::SeqCst);
    }

    fn save_session(&self) {
        let Some(url) = self.base_url() else { return };
        let mut cookies = serde_json::Map::new();
        if let Some(header) = self.jar.cookies(&url) {
            if let Ok(s) = header.to_str() {
                for part in s.split(';') {
                    if let Some((k, v)) = part.trim().split_once('=') {
                        cookies.insert(k.to_string(), Value::String(v.to_string()));
                    }
                }
            }
        }
        let payload = json!({
            "base": self.base,
            "username": self.username,
            "saved_at": chrono::Utc::now().timestamp(),
            "cookies": cookies,
        });
        let raw = serde_json::to_vec(&payload).unwrap_or_default();
        let _ = atomic_write(&self.session_path, &raw, Some(0o600));
    }

    pub async fn login(&self, force: bool) -> LpResult<Value> {
        let _guard = self.login_lock.lock().await;
        if self.logged_in.load(Ordering::SeqCst) && !force {
            return Ok(json!({"cached": true}));
        }
        if force {
            if let Some(url) = self.base_url() {
                let _ = self.jar.cookies(&url);
            }
        }
        tracing::info!(" LitePan:{}", self.base);
        let resp = self
            .http
            .post(format!("{}/api/auth/login", self.base))
            .form(&[
                ("username", self.username.as_str()),
                ("password", self.password.as_str()),
                ("remember", "1"),
            ])
            .send()
            .await
            .map_err(|e| LitePanError::Message(format!(":{e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED {
            if let Some(hint) = ark_self_hint(&text) {
                return Err(LitePanError::Message(hint.into()));
            }
            return Err(LitePanError::Auth("(401)".into()));
        }
        if !status.is_success() {
            let body = text.chars().take(300).collect::<String>();
            if let Some(hint) = ark_self_hint(&body) {
                return Err(LitePanError::Message(hint.into()));
            }
            return Err(LitePanError::Message(format!(
                " HTTP {}:{}",
                status.as_u16(),
                body
            )));
        }
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if let Some(data) = payload.get("data") {
            if data
                .get("two_factor_required")
                .or_else(|| data.get("twoFactorRequired"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Err(LitePanError::Auth(
                    "LitePan account has 2FA enabled; disable 2FA or use another account".into(),
                ));
            }
        }
        if payload.get("success").and_then(Value::as_bool) == Some(false) {
            let msg = dig(&payload, &["message"]).and_then(Value::as_str).unwrap_or("");
            return Err(LitePanError::Auth(format!("?{msg}")));
        }
        self.logged_in.store(true, Ordering::SeqCst);
        self.save_session();
        Ok(json!({"logged_in": true}))
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        query: Option<&[(String, String)]>,
        body: Option<Value>,
    ) -> LpResult<Value> {
        let mut attempt = 0;
        loop {
            let mut url = format!("{}{}", self.base, path);
            if let Some(q) = query {
                if !q.is_empty() {
                    let mut u = Url::parse(&url)
                        .map_err(|e| LitePanError::Message(format!("URL :{e}")))?;
                    {
                        let mut pairs = u.query_pairs_mut();
                        for (k, v) in q {
                            pairs.append_pair(k, v);
                        }
                    }
                    url = u.to_string();
                }
            }
            let mut req = self.http.request(method.clone(), &url);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| LitePanError::Message(format!("{method} {path} :{e}")))?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                if attempt == 0 {
                    tracing::info!("session expired (HTTP {}), retrying login", status.as_u16());
                    self.login(true).await?;
                    attempt += 1;
                    continue;
                }
                return Err(LitePanError::Auth(format!(
                    "{method} {path}  HTTP {}",
                    status.as_u16()
                )));
            }
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                let body = text.chars().take(300).collect::<String>();
                if let Some(hint) = ark_self_hint(&body) {
                    return Err(LitePanError::Message(hint.into()));
                }
                let low = body.to_lowercase();
                // 115/合盘常用 502 + “文件不存在或已删除” 表示对象已删除，
                // 必须当成 NotFound，否则搜索会把已删除文件一直留在索引里。
                if status == StatusCode::NOT_FOUND
                    || low.contains("not found")
                    || low.contains("not exist")
                    || low.contains("no such")
                    || body.contains("不存在")
                    || body.contains("已删除")
                {
                    return Err(LitePanError::NotFound(format!(
                        "{method} {path}  HTTP {}:{body}",
                        status.as_u16()
                    )));
                }
                return Err(LitePanError::Message(format!(
                    "{method} {path}  HTTP {}:{body}",
                    status.as_u16()
                )));
            }
            let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if payload.is_null() {
                return Ok(Value::Null);
            }
            if payload.get("success").and_then(Value::as_bool) == Some(false) {
                let msg = dig(&payload, &["message", "error"])
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let code = dig(&payload, &["code", "error_code"])
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                if msg.contains("不存在") || msg.contains("已删除") || msg.to_lowercase().contains("not found") {
                    return Err(LitePanError::NotFound(format!(
                        "{method} {path} :{msg} ({code})"
                    )));
                }
                return Err(LitePanError::Message(format!(
                    "{method} {path} :{msg} ({code})"
                )));
            }
            if let Some(obj) = payload.as_object() {
                if obj.contains_key("data") {
                    return Ok(obj.get("data").cloned().unwrap_or(Value::Null));
                }
            }
            return Ok(payload);
        }
    }

    pub async fn accounts(&self) -> LpResult<Vec<Value>> {
        let data = self.request(Method::GET, "/api/public/accounts", None, None).await?;
        let value = if let Some(obj) = data.as_object() {
            dig(&data, &["accounts", "list", "items"]).cloned().unwrap_or_else(|| Value::Array(vec![]))
        } else {
            data
        };
        Ok(match value {
            Value::Array(a) => a,
            _ => Vec::new(),
        })
    }

    pub async fn pick_account(&self, account_id: Option<i64>) -> LpResult<Value> {
        let accounts = self.accounts().await?;
        if accounts.is_empty() {
            return Err(LitePanError::Message(
                "LitePan ? 115 ".into(),
            ));
        }
        let norm = |item: &Value| {
            json!({
                "id": dig(item, &["id", "account_id", "accountId"]).and_then(Value::as_i64).unwrap_or(0),
                "name": dig(item, &["name", "account_name", "nickname", "remark"]).and_then(Value::as_str).unwrap_or(""),
                "driver": dig(item, &["driver", "kind", "type", "provider"]).and_then(Value::as_str).unwrap_or(""),
            })
        };
        let normalized: Vec<Value> = accounts.iter().map(norm).collect();
        if let Some(want) = account_id {
            if let Some(acc) = normalized
                .iter()
                .find(|a| a.get("id").and_then(Value::as_i64) == Some(want))
            {
                return Ok(acc.clone());
            }
            return Err(LitePanError::Message(format!("account_id={want} not found")));
        }
        if let Some(acc) = normalized.iter().find(|a| {
            a.get("driver")
                .and_then(Value::as_str)
                .unwrap_or("")
                .contains("115")
        }) {
            return Ok(acc.clone());
        }
        Ok(normalized[0].clone())
    }

    pub async fn net_search(
        &self,
        keyword: &str,
        account_id: i64,
        limit: usize,
        dirs_only: bool,
        timeout: Option<u64>,
    ) -> LpResult<Vec<Value>> {
        let body = json!({
            "method": "search",
            "params": {"keyword": keyword.trim(), "account_id": account_id, "limit": limit}
        });
        let mut req = self.http.post(format!("{}/api/admin/plugins/net-search/rpc", self.base)).json(&body);
        if let Some(t) = timeout {
            req = req.timeout(Duration::from_secs(t));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| LitePanError::Message(format!(":{e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(LitePanError::Message(format!(
                " HTTP {}:{}",
                status.as_u16(),
                text.chars().take(200).collect::<String>()
            )));
        }
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let mut node = payload.get("data").cloned().unwrap_or(payload);
        for _ in 0..4 {
            if !node.is_object() || node.get("items").is_some() {
                break;
            }
            let next = node.get("result").cloned().or_else(|| node.get("data").cloned());
            match next {
                Some(n) => node = n,
                None => break,
            }
        }
        let mut out = Vec::new();
        if let Some(items) = node.get("items").and_then(Value::as_array) {
            for it in items {
                let Some(path) = it.get("path").and_then(Value::as_str) else {
                    continue;
                };
                if path.is_empty() {
                    continue;
                }
                let is_dir = it.get("is_dir").and_then(Value::as_bool).unwrap_or(false);
                if dirs_only && !is_dir {
                    continue;
                }
                out.push(json!({
                    "name": it.get("name").and_then(Value::as_str).unwrap_or(""),
                    "path": path.trim_matches('/'),
                    "id": it.get("id").map(|v| v.to_string().trim_matches('"').to_string()).unwrap_or_default(),
                    "is_dir": is_dir,
                    "size": it.get("size").and_then(Value::as_i64).unwrap_or(0),
                }));
            }
        }
        Ok(out)
    }

    pub async fn offline_capabilities(&self, account_id: i64) -> LpResult<Value> {
        self.request(
            Method::GET,
            &format!("/api/files/offline-download/capabilities?account_id={account_id}"),
            None,
            None,
        )
        .await
    }

    pub async fn add_offline_urls(
        &self,
        urls: &[String],
        account_id: i64,
        provider_kind: Option<&str>,
        target_display_path: Option<&str>,
        target_parent_id: Option<&str>,
        file_name: Option<&str>,
    ) -> LpResult<Value> {
        let mut body = json!({"account_id": account_id, "urls": urls});
        if let Some(v) = provider_kind {
            body["provider_kind"] = Value::String(v.to_string());
        }
        if let Some(v) = target_display_path {
            body["target_display_path"] = Value::String(v.to_string());
        }
        if let Some(v) = target_parent_id {
            body["target_parent_id"] = Value::String(v.to_string());
        }
        if let Some(v) = file_name {
            body["file_name"] = Value::String(v.to_string());
        }
        self.request(Method::POST, "/api/files/offline-download/urls", None, Some(body))
            .await
    }

    pub async fn offline_tasks(&self) -> LpResult<Vec<Value>> {
        let data = self
            .request(Method::GET, "/api/files/offline-download/tasks", None, None)
            .await?;
        let value = if data.is_object() {
            dig(&data, &["tasks", "list", "items"]).cloned().unwrap_or_else(|| Value::Array(vec![]))
        } else {
            data
        };
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    pub async fn refresh_offline_tasks(&self) -> LpResult<Value> {
        self.request(
            Method::POST,
            "/api/files/offline-download/tasks/refresh",
            None,
            Some(json!({})),
        )
        .await
    }

    pub async fn strm_tasks(&self) -> LpResult<Vec<Value>> {
        let data = self.request(Method::GET, "/api/admin/strm/tasks", None, None).await?;
        let value = if data.is_object() {
            dig(&data, &["tasks", "list", "items"]).cloned().unwrap_or_else(|| Value::Array(vec![]))
        } else {
            data
        };
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    pub async fn run_strm_task(&self, task_id: i64) -> LpResult<Value> {
        self.request(
            Method::POST,
            &format!("/api/admin/strm/tasks/{task_id}/run?mode=auto"),
            None,
            Some(json!({"mode": "auto"})),
        )
        .await
    }

    pub async fn list_dir(
        &self,
        account_id: i64,
        parent_id: &str,
        path: &str,
        timeout: Option<u64>,
    ) -> LpResult<Value> {
        let mut query = vec![
            ("account_id".to_string(), account_id.to_string()),
            ("parent_id".to_string(), parent_id.to_string()),
        ];
        if !path.is_empty() {
            query.push(("path".to_string(), path.to_string()));
        }
        let mut url = Url::parse(&format!("{}/api/files/list", self.base))
            .map_err(|e| LitePanError::Message(format!("URL :{e}")))?;
        {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in &query {
                pairs.append_pair(k, v);
            }
        }
        let mut req = self.http.get(url);
        if let Some(t) = timeout {
            req = req.timeout(Duration::from_secs(t));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| LitePanError::Message(format!("?{e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(LitePanError::Message(format!(
                "?HTTP {}:{}",
                status.as_u16(),
                text.chars().take(200).collect::<String>()
            )));
        }
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let data = payload.get("data").cloned().unwrap_or(payload);
        let items = data
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let normalized: Vec<Value> = items
            .into_iter()
            .map(|it| {
                json!({
                    "id": it.get("id").map(|v| v.to_string().trim_matches('"').to_string()).unwrap_or_default(),
                    "name": it.get("name").and_then(Value::as_str).unwrap_or(""),
                    "size": it.get("size").and_then(Value::as_i64).unwrap_or(0),
                    "is_dir": it.get("is_dir").and_then(Value::as_bool).unwrap_or(false),
                    "modified": it.get("mod_time").or_else(|| it.get("modified")).or_else(|| it.get("mtime")).cloned().unwrap_or(Value::String(String::new())),
                })
            })
            .collect();
        Ok(json!({
            "parent_id": data.get("parent_id").cloned().unwrap_or(Value::String(parent_id.to_string())),
            "items": normalized,
        }))
    }

    pub async fn directory_strm_status(
        &self,
        account_id: i64,
        parent_id: &str,
        path: &str,
        dirs: &[String],
        items: &[Value],
    ) -> LpResult<Value> {
        let body = json!({
            "account_id": account_id, "parent_id": parent_id, "path": path,
            "dirs": dirs, "items": wire_items(items)
        });
        self.request(Method::POST, "/api/admin/strm/directory-status", None, Some(body))
            .await
    }

    pub async fn generate_directory_strm(
        &self,
        account_id: i64,
        parent_id: &str,
        path: &str,
        dirs: &[String],
        items: &[Value],
    ) -> LpResult<Value> {
        let body = json!({
            "account_id": account_id, "parent_id": parent_id, "path": path,
            "dirs": dirs, "items": wire_items(items)
        });
        self.request(
            Method::POST,
            "/api/admin/strm/generate-current-directory",
            None,
            Some(body),
        )
        .await
    }

    pub async fn localfs_account_id(&self) -> LpResult<i64> {
        for acc in self.accounts().await? {
            let driver = dig(&acc, &["driver_type", "driver"])
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            if driver.contains("local") {
                return Ok(dig(&acc, &["id"]).and_then(Value::as_i64).unwrap_or(0));
            }
        }
        Ok(0)
    }

    pub async fn list_abs(&self, account_id: i64, abs_path: &str) -> Vec<Value> {
        if !abs_path.starts_with('/') {
            return Vec::new();
        }
        match self
            .list_dir(account_id, abs_path.trim_end_matches('/'), "", None)
            .await
        {
            Ok(v) => v
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    pub async fn strm_output_roots(&self, account_id: i64) -> Vec<String> {
        let mut out = Vec::new();
        for base in ["/mnt/strm/litepan", "/strm/litepan"] {
            for it in self.list_abs(account_id, base).await {
                if it.get("is_dir").and_then(Value::as_bool).unwrap_or(false) {
                    if let Some(name) = it.get("name").and_then(Value::as_str) {
                        out.push(format!("{base}/{name}"));
                    }
                }
            }
            if !out.is_empty() {
                return out;
            }
        }
        out
    }

    pub async fn resolve_dir(&self, account_id: i64, path: &str, create: bool) -> LpResult<Value> {
        let normalized = path.replace('\\', "/");
        let parts: Vec<&str> = normalized
            .split('/')
            .filter(|p| !p.trim().is_empty())
            .collect();
        let mut parent_id = String::new();
        let mut walked: Vec<&str> = Vec::new();
        for seg in parts {
            let listed = self
                .list_dir(account_id, &parent_id, &walked.join("/"), None)
                .await?;
            let items = listed.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            let hit = items.iter().find(|i| {
                i.get("is_dir").and_then(Value::as_bool).unwrap_or(false)
                    && i.get("name").and_then(Value::as_str) == Some(seg)
            });
            if let Some(hit) = hit {
                parent_id = hit
                    .get("id")
                    .map(|v| v.to_string().trim_matches('"').to_string())
                    .unwrap_or_default();
            } else if create {
                let data = self
                    .request(
                        Method::POST,
                        "/api/files/create-folder",
                        None,
                        Some(json!({"account_id": account_id, "parent_id": parent_id, "name": seg})),
                    )
                    .await?;
                parent_id = data
                    .get("folder_id")
                    .or_else(|| data.get("id"))
                    .map(|v| v.to_string().trim_matches('"').to_string())
                    .unwrap_or_default();
                if parent_id.is_empty() {
                    return Err(LitePanError::Message(format!(
                        ":{}",
                        walked.iter().chain([&seg]).cloned().collect::<Vec<_>>().join("/")
                    )));
                }
            } else {
                return Err(LitePanError::NotFound(
                    walked.iter().chain([&seg]).cloned().collect::<Vec<_>>().join("/"),
                ));
            }
            walked.push(seg);
        }
        Ok(json!({
            "parent_id": parent_id,
            "path": if walked.is_empty() { String::new() } else { format!("/{}", walked.join("/")) },
        }))
    }

    pub async fn rename_file(&self, account_id: i64, file_id: &str, new_name: &str, parent_id: &str) -> LpResult<Value> {
        self.request(
            Method::PUT,
            "/api/files/rename",
            None,
            Some(json!({"account_id": account_id, "file_id": file_id, "new_name": new_name, "parent_id": parent_id})),
        )
        .await
    }

    pub async fn move_files(&self, account_id: i64, file_ids: &[String], target_parent_id: &str, source_parent_id: &str) -> LpResult<Value> {
        let mut body = json!({"account_id": account_id, "file_ids": file_ids, "target_parent_id": target_parent_id});
        if !source_parent_id.is_empty() {
            body["source_parent_id"] = Value::String(source_parent_id.to_string());
        }
        self.request(Method::POST, "/api/files/move", None, Some(body)).await
    }

    pub async fn copy_files(&self, account_id: i64, file_ids: &[String], target_parent_id: &str, source_parent_id: &str) -> LpResult<Value> {
        let mut body = json!({"account_id": account_id, "file_ids": file_ids, "target_parent_id": target_parent_id});
        if !source_parent_id.is_empty() {
            body["source_parent_id"] = Value::String(source_parent_id.to_string());
        }
        self.request(Method::POST, "/api/files/copy", None, Some(body)).await
    }

    pub async fn delete_files(&self, account_id: i64, file_ids: &[String], parent_id: &str) -> LpResult<Value> {
        let mut body = json!({"account_id": account_id, "file_ids": file_ids});
        if !parent_id.is_empty() {
            body["parent_id"] = Value::String(parent_id.to_string());
        }
        self.request(Method::DELETE, "/api/files/delete", None, Some(body)).await
    }

    pub async fn create_folder(&self, account_id: i64, parent_id: &str, name: &str) -> LpResult<Value> {
        self.request(
            Method::POST,
            "/api/files/create-folder",
            None,
            Some(json!({"account_id": account_id, "parent_id": parent_id, "name": name})),
        )
        .await
    }

    pub async fn upload_file(
        &self,
        account_id: i64,
        path: &str,
        filename: &str,
        data: Vec<u8>,
    ) -> LpResult<Value> {
        let part = reqwest::multipart::Part::bytes(data)
            .file_name(filename.to_string())
            .mime_str("application/octet-stream")
            .map_err(|e| LitePanError::Message(format!(":{e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("account_id", account_id.to_string())
            .text("path", path.to_string())
            .text("conflict_policy", "overwrite")
            .text("client_task_id", format!("ark-push-{}", uuid::Uuid::new_v4().simple()))
            .text("display_name", filename.to_string())
            .part("file", part);
        let resp = self
            .http
            .post(format!("{}/api/files/upload-task", self.base))
            .multipart(form)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| LitePanError::Message(format!(" {filename} :{e}")))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(LitePanError::Message(format!(
                " {filename}  HTTP {}:{}",
                status.as_u16(),
                text.chars().take(200).collect::<String>()
            )));
        }
        let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(payload.get("data").cloned().unwrap_or(Value::Null))
    }

    pub async fn download_file(&self, account_id: i64, file_id: &str, file_name: &str, max_bytes: usize) -> LpResult<Vec<u8>> {
        let mut url = Url::parse(&format!("{}/api/files/download", self.base))
            .map_err(|e| LitePanError::Message(format!("URL :{e}")))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("account_id", &account_id.to_string());
            q.append_pair("file_id", file_id);
            q.append_pair("force_proxy", "1");
            if !file_name.is_empty() {
                q.append_pair("file_name", file_name);
            }
        }
        let mut resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| LitePanError::Message(format!(":{e}")))?;
        if resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN {
            self.login(true).await?;
            let mut url = Url::parse(&format!("{}/api/files/download", self.base))
                .map_err(|e| LitePanError::Message(format!("URL :{e}")))?;
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("account_id", &account_id.to_string());
                q.append_pair("file_id", file_id);
                q.append_pair("force_proxy", "1");
                if !file_name.is_empty() {
                    q.append_pair("file_name", file_name);
                }
            }
            resp = self
                .http
                .get(url)
                .send()
                .await
                .map_err(|e| LitePanError::Message(format!(":{e}")))?;
        }
        if !resp.status().is_success() {
            return Err(LitePanError::Message(format!(
                " HTTP {}",
                resp.status().as_u16()
            )));
        }
        let mut buf = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else { break };
            buf.extend_from_slice(&chunk);
            if buf.len() > max_bytes {
                buf.truncate(max_bytes);
                break;
            }
        }
        Ok(buf)
    }

    pub async fn open_stream(&self, account_id: i64, file_id: &str, file_name: &str, range: &str) -> LpResult<Response> {
        let mut url = Url::parse(&format!("{}/api/files/download", self.base))
            .map_err(|e| LitePanError::Message(format!("URL :{e}")))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("account_id", &account_id.to_string());
            q.append_pair("file_id", file_id);
            q.append_pair("inline", "1");
            if !file_name.is_empty() {
                q.append_pair("file_name", file_name);
            }
        }
        let mut req = self.http.get(url).timeout(Duration::from_secs(600));
        if !range.is_empty() {
            req = req.header(reqwest::header::RANGE, range);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| LitePanError::Message(format!("?{e}")))?;
        if resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN {
            self.login(true).await?;
            let mut url = Url::parse(&format!("{}/api/files/download", self.base))
                .map_err(|e| LitePanError::Message(format!("URL :{e}")))?;
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("account_id", &account_id.to_string());
                q.append_pair("file_id", file_id);
                q.append_pair("inline", "1");
                if !file_name.is_empty() {
                    q.append_pair("file_name", file_name);
                }
            }
            let mut req = self.http.get(url).timeout(Duration::from_secs(600));
            if !range.is_empty() {
                req = req.header(reqwest::header::RANGE, range);
            }
            return req
                .send()
                .await
                .map_err(|e| LitePanError::Message(format!("?{e}")));
        }
        Ok(resp)
    }

    pub async fn probe(&self) -> Value {
        let mut out = serde_json::Map::new();
        for (name, result) in [
            ("accounts", self.request(Method::GET, "/api/public/accounts", None, None).await),
            ("strm_tasks", self.request(Method::GET, "/api/admin/strm/tasks", None, None).await),
            ("offline_tasks", self.request(Method::GET, "/api/files/offline-download/tasks", None, None).await),
        ] {
            out.insert(
                name.to_string(),
                match result {
                    Ok(v) => v,
                    Err(e) => json!({"error": e.to_string()}),
                },
            );
        }
        Value::Object(out)
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

pub fn wire_items(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .filter_map(|it| {
            let obj = it.as_object()?;
            Some(json!({
                "id": obj.get("id").map(|v| v.to_string().trim_matches('"').to_string()).unwrap_or_default(),
                "name": obj.get("name").and_then(Value::as_str).unwrap_or(""),
                "size": obj.get("size").and_then(Value::as_i64).unwrap_or(0),
                "is_dir": obj.get("is_dir").and_then(Value::as_bool).unwrap_or(false),
            }))
        })
        .collect()
}

pub struct LitePanPool {
    cfg: Config,
    state: Arc<StateStore>,
    clients: RwLock<HashMap<String, Arc<LitePanClient>>>,
    creds_lock: tokio::sync::Mutex<()>,
}

impl LitePanPool {
    pub fn new(cfg: Config, state: Arc<StateStore>) -> Self {
        Self {
            cfg,
            state,
            clients: RwLock::new(HashMap::new()),
            creds_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn creds(&self) -> (String, String, String) {
        let url = {
            let s = self.state.get_str("litepan_url", "");
            if s.is_empty() { self.cfg.litepan_base() } else { s.trim_end_matches('/').to_string() }
        };
        let user = {
            let s = self.state.get_str("litepan_username", "");
            if s.is_empty() { self.cfg.litepan_username.clone() } else { s }
        };
        let pwd = {
            let s = self.state.get_str("litepan_password", "");
            if s.is_empty() { self.cfg.litepan_password.clone() } else { s }
        };
        (url, user, pwd)
    }

    pub fn insecure(&self) -> bool {
        self.state
            .get_bool("litepan_insecure", None)
            .unwrap_or_else(|| self.cfg.insecure_tls())
    }

    pub async fn client_for(&self, payload: &Value) -> Result<Arc<LitePanClient>, LitePanError> {
        let (srv_url, srv_user, srv_pwd) = self.creds();
        let url = payload
            .get("litepan_url")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&srv_url)
            .trim_end_matches('/')
            .to_string();
        let user = payload
            .get("username")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&srv_user)
            .to_string();
        let pwd = payload
            .get("password")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&srv_pwd)
            .to_string();
        if pwd.is_empty() {
            return Err(LitePanError::Message(String::from(
                "?? LitePan ",
            )));
        }
        let key = crate::util::sha1_hex(&format!("{url}|{user}|{pwd}"));
        let existing = { self.clients.read().get(&key).cloned() };
        let client = if let Some(c) = existing {
            c
        } else {
            let _guard = self.creds_lock.lock().await;
            let existing = { self.clients.read().get(&key).cloned() };
            if let Some(c) = existing {
                c
            } else {
                let c = Arc::new(LitePanClient::new(
                    &url,
                    &user,
                    &pwd,
                    &self.cfg.state_dir,
                    !self.insecure(),
                )?);
                let mut guard = self.clients.write();
                if guard.len() > 8 {
                    guard.clear();
                }
                guard.insert(key, c.clone());
                c
            }
        };
        client.login(false).await?;
        Ok(client)
    }

    pub fn clear(&self) {
        self.clients.write().clear();
    }

    pub fn default_client(&self) -> impl std::future::Future<Output = Result<Arc<LitePanClient>, LitePanError>> + '_ {
        self.client_for(&Value::Null)
    }
}

pub fn provider_from_caps(caps: &Value) -> Option<String> {
    for key in ["default_provider", "provider_kind", "provider", "default", "kind"] {
        if let Some(v) = caps.get(key).and_then(Value::as_str) {
            if !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        }
    }
    for key in ["native", "native_offline", "cloud_offline", "offline_native"] {
        if caps.get(key).and_then(Value::as_bool) == Some(true) {
            return Some("native".into());
        }
    }
    for key in ["builtin", "builtin_downloader", "magnet"] {
        if caps.get(key).and_then(Value::as_bool) == Some(true) && caps.get("native").and_then(Value::as_bool) != Some(true) {
            return Some("builtin".into());
        }
    }
    None
}

pub fn name_from_url(url: &str) -> String {
    let raw = url.trim();
    if raw.starts_with("magnet:") {
        return raw.chars().take(80).collect();
    }
    let base = posix_basename(raw);
    if base.is_empty() {
        raw.chars().take(80).collect()
    } else {
        base
    }
}
