use crate::search_fts::SearchStore;
use crate::state::StateStore;
use crate::util::{actor_from_path, norm_actor_name, read_json, sha1_hex};
use chrono::Datelike;
use parking_lot::{Mutex, RwLock};
use reqwest::Client;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

// 与用户油猴脚本 We.token() 同源：数组逐项 ^ 37
const TOKEN_NUMS: &[u8] = &[
    107, 77, 29, 99, 68, 73, 100, 102, 111, 103, 102, 72, 104, 95, 105, 111, 103, 65, 127, 73,
    64, 64, 76, 97, 108, 16, 115, 18, 73, 20, 21, 95, 66, 28, 19, 81, 68, 112, 104, 74, 74, 83,
    66,
];

const AVATAR_API: &str = "https://javxy.cc.cd/api/actors/avatar";
const PROFILE_API: &str = "https://javxy.cc.cd/api/actors";
const PROFILE_FALLBACK: &str = "https://worker.javxy.cc.cd/api/actors";
const TTL: i64 = 7 * 86400;
const NEG_TTL: i64 = 30 * 60;

fn javxy_token() -> String {
    TOKEN_NUMS.iter().map(|n| (n ^ 37) as char).collect()
}

fn user_agent() -> &'static str {
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36"
}

fn norm_lookup(s: &str) -> String {
    norm_actor_name(s).to_lowercase()
}

fn format_profile(p: &Value) -> String {
    let mut bits: Vec<String> = Vec::new();
    if let Some(bd) = p.get("birthday").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        let re = regex::Regex::new(r"(\d{4})[-/.](\d{1,2})[-/.](\d{1,2})").ok();
        if let Some(c) = re.as_ref().and_then(|r| r.captures(bd)) {
            let y: i32 = c[1].parse().unwrap_or(0);
            let m: u32 = c[2].parse().unwrap_or(1);
            let d: u32 = c[3].parse().unwrap_or(1);
            bits.push(format!("{y:04}.{m:02}.{d:02}"));
            let now = chrono::Local::now();
            let age = now.year() - y - if (now.month(), now.day()) < (m, d) { 1 } else { 0 };
            bits.push(format!("{age}岁"));
        } else {
            bits.push(bd.to_string());
        }
    }
    for (key, unit) in [("heightCm", "cm"), ("weightKg", "kg")] {
        if let Some(v) = p.get(key).and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))) {
            bits.push(format!("{v}{unit}"));
        }
    }
    let b = p.get("bustCm").and_then(Value::as_i64);
    let w = p.get("waistCm").and_then(Value::as_i64);
    let h = p.get("hipCm").and_then(Value::as_i64);
    if let (Some(b), Some(w), Some(h)) = (b, w, h) {
        bits.push(format!("{b}-{w}-{h}"));
    }
    if let Some(cup) = p.get("cup").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        bits.push(format!("{cup}罩杯"));
    }
    if let Some(bt) = p.get("bloodType").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        bits.push(if bt.ends_with('血') { bt.to_string() } else { format!("{bt}血") });
    }
    bits.join(" ")
}

#[derive(Clone)]
struct MetaEntry {
    value: Value,
    at: i64,
}

pub struct ActorService {
    state: Arc<StateStore>,
    search: Arc<SearchStore>,
    avatar_dir: RwLock<PathBuf>,
    http: Client,
    meta: RwLock<HashMap<String, MetaEntry>>,
    profiles: RwLock<HashMap<String, MetaEntry>>,
    inflight: Mutex<HashSet<String>>,
    javxy_last: Mutex<Instant>,
    ban_until: RwLock<Option<i64>>,
    ban_note: RwLock<String>,
}

impl ActorService {
    pub fn new(state: Arc<StateStore>, search: Arc<SearchStore>, default_avatar_dir: &str) -> Self {
        let dir = default_avatar_dir.to_string();
        let _ = std::fs::create_dir_all(&dir);
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(user_agent())
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            state,
            search,
            avatar_dir: RwLock::new(PathBuf::from(dir)),
            http,
            meta: RwLock::new(HashMap::new()),
            profiles: RwLock::new(HashMap::new()),
            inflight: Mutex::new(HashSet::new()),
            javxy_last: Mutex::new(Instant::now() - Duration::from_secs(5)),
            ban_until: RwLock::new(None),
            ban_note: RwLock::new(String::new()),
        }
    }

    pub fn set_avatar_dir(&self, dir: &str) {
        let _ = std::fs::create_dir_all(dir);
        *self.avatar_dir.write() = PathBuf::from(dir);
    }

    fn avatar_dir(&self) -> PathBuf {
        let saved = self.state.get_str("avatar_dir", "");
        if saved.is_empty() {
            self.avatar_dir.read().clone()
        } else {
            PathBuf::from(saved)
        }
    }

    fn cache_dir(&self) -> PathBuf {
        let saved = self.state.get_str("cache_dir", "");
        if saved.is_empty() {
            PathBuf::from(format!("{}/cache", self.state.dir.display()))
        } else {
            PathBuf::from(saved)
        }
    }

    fn key_of(&self, name: &str) -> String {
        sha1_hex(name.trim()).chars().take(20).collect()
    }

    pub fn avatar_path(&self, key: &str) -> PathBuf {
        self.avatar_dir().join(format!("{key}.jpg"))
    }

    pub fn banned(&self) -> Value {
        let until = (*self.ban_until.read()).unwrap_or(0);
        let left = until - chrono::Utc::now().timestamp();
        if left <= 0 {
            json!({"banned": false})
        } else {
            json!({
                "banned": true,
                "remaining": left,
                "until": chrono::DateTime::from_timestamp(until, 0)
                    .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default(),
                "note": self.ban_note.read().clone(),
            })
        }
    }

    async fn throttle(&self) {
        let wait = {
            let mut last = self.javxy_last.lock();
            let elapsed = last.elapsed();
            if elapsed < Duration::from_secs(1) {
                Some(Duration::from_secs(1) - elapsed)
            } else {
                *last = Instant::now();
                None
            }
        };
        if let Some(w) = wait {
            tokio::time::sleep(w).await;
            *self.javxy_last.lock() = Instant::now();
        }
    }

    fn note_ban(&self, body: &str) -> bool {
        let low = body.to_lowercase();
        if !low.contains("banned") && !low.contains("ip_banned") {
            return false;
        }
        let mut left = 3600i64;
        if let Ok(re) = regex::Regex::new(r#""remainingSeconds"\s*:\s*(\d+)"#) {
            if let Some(c) = re.captures(body) {
                left = c[1].parse().unwrap_or(3600).max(60);
            }
        }
        *self.ban_until.write() = Some(chrono::Utc::now().timestamp() + left);
        *self.ban_note.write() = "javxy 接口因请求过于频繁,封禁了本机公网 IP".into();
        true
    }

    async fn query_javxy(&self, name: &str) -> (Vec<String>, String) {
        if self.banned().get("banned").and_then(Value::as_bool).unwrap_or(false) {
            return (Vec::new(), String::new());
        }
        self.throttle().await;
        let token = javxy_token();
        let resp = self
            .http
            .get(AVATAR_API)
            .query(&[("client", "laosiji-new"), ("names", name)])
            .header("X-Javxy-Token", token)
            .header("X-Javxy-Principal", "litepan-sidecar")
            .header("Accept", "application/json,text/plain,*/*")
            .send()
            .await;
        let Ok(resp) = resp else { return (Vec::new(), String::new()) };
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if self.note_ban(&body) {
            return (Vec::new(), String::new());
        }
        if !status.is_success() {
            return (Vec::new(), String::new());
        }
        let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let info = data
            .get("items")
            .and_then(Value::as_array)
            .and_then(|a| {
                a.iter()
                    .find(|it| it.get("name").and_then(Value::as_str).map(str::trim) == Some(name))
                    .or_else(|| a.first())
            })
            .cloned()
            .unwrap_or(Value::Null);
        let urls = [
            info.get("avatar").and_then(Value::as_str).unwrap_or(""),
            info.get("avatarAlt").and_then(Value::as_str).unwrap_or(""),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
        let extra = info
            .get("info")
            .or_else(|| info.get("description"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        (urls, extra)
    }

    async fn download_avatar(&self, urls: &[String], dest: &Path) -> bool {
        if dest.exists() && std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0) > 1024 {
            return true;
        }
        for url in urls {
            let candidates = if url.starts_with("https://raw.githubusercontent.com/") {
                vec![
                    url.clone(),
                    format!("https://ghproxy.net/{url}"),
                    format!("https://gh-proxy.com/{url}"),
                    format!("https://cdn.jsdelivr.net/gh/{}", url.trim_start_matches("https://raw.githubusercontent.com/")),
                ]
            } else {
                vec![url.clone()]
            };
            for u in candidates {
                let Ok(resp) = self.http.get(&u).send().await else { continue };
                if !resp.status().is_success() {
                    continue;
                }
                let Ok(bytes) = resp.bytes().await else { continue };
                if bytes.len() > 1024 && (bytes.starts_with(&[0xff, 0xd8]) || bytes.starts_with(&[0x89, 0x50])) {
                    if let Some(parent) = dest.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::write(dest, &bytes).is_ok() {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub async fn get(&self, name: &str) -> Value {
        let name = name.trim();
        if name.is_empty() {
            return json!({"ok": true, "name": "", "found": false});
        }
        let key = self.key_of(name);
        let now = chrono::Utc::now().timestamp();
        if let Some(hit) = self.meta.read().get(name).cloned() {
            let age = now - hit.at;
            let found = hit.value.get("found").and_then(Value::as_bool).unwrap_or(false);
            if (found && age < TTL) || (!found && age < NEG_TTL) {
                return hit.value;
            }
        }
        let dest = self.avatar_path(&key);
        if dest.exists() && std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0) > 1024 {
            let info = json!({
                "ok": true, "key": key, "name": name, "found": true,
                "avatar_url": format!("/avatar/{key}.jpg"), "info": "", "facts": "",
                "profile": {}, "source": "cache",
            });
            self.meta.write().insert(name.to_string(), MetaEntry { value: info.clone(), at: now });
            return info;
        }
        // 同一演员并发去重
        let already = {
            let set = self.inflight.lock();
            set.contains(name)
        };
        if already {
            for _ in 0..30 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Some(v) = self.meta.read().get(name).cloned() {
                    return v.value;
                }
            }
            return json!({"ok": true, "name": name, "found": false});
        }
        self.inflight.lock().insert(name.to_string());
        let (urls, info_text) = self.query_javxy(name).await;
        let ok = self.download_avatar(&urls, &dest).await;
        let profile = self.profile(name).await;
        let facts = format_profile(&profile);
        let info = json!({
            "ok": true, "key": key, "name": name, "found": ok,
            "avatar_url": if ok { format!("/avatar/{key}.jpg") } else { String::new() },
            "info": if facts.is_empty() { info_text } else { facts.clone() },
            "facts": facts, "profile": profile,
            "source": if ok { "javxy" } else { "" },
        });
        self.meta.write().insert(name.to_string(), MetaEntry { value: info.clone(), at: now });
        self.inflight.lock().remove(name);
        info
    }

    async fn query_profiles(&self, names: &[String]) -> HashMap<String, Value> {
        let mut out = HashMap::new();
        if names.is_empty() || self.banned().get("banned").and_then(Value::as_bool).unwrap_or(false) {
            return out;
        }
        self.throttle().await;
        let token = javxy_token();
        for host in [PROFILE_API, PROFILE_FALLBACK] {
            let mut req = self
                .http
                .post(host)
                .query(&[("client", "laosiji-new")])
                .json(&json!({"names": names}))
                .header("Accept", "application/json,text/plain,*/*")
                .header("Content-Type", "application/json");
            if host == PROFILE_API {
                req = req.header("X-Javxy-Token", token.clone());
            }
            let Ok(resp) = req.send().await else { continue };
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if self.note_ban(&body) {
                break;
            }
            if !status.is_success() {
                continue;
            }
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            if let Some(profiles) = data.get("profiles").and_then(Value::as_array) {
                for it in profiles {
                    let Some(p) = it.get("profile").filter(|p| p.is_object()) else { continue };
                    let name = p
                        .get("name")
                        .or_else(|| it.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if !name.is_empty() {
                        out.insert(name.to_string(), p.clone());
                    }
                }
            }
            if !out.is_empty() {
                break;
            }
        }
        out
    }

    pub async fn profile(&self, name: &str) -> Value {
        let name = name.trim();
        if name.is_empty() {
            return json!({});
        }
        let now = chrono::Utc::now().timestamp();
        if let Some(hit) = self.profiles.read().get(name).cloned() {
            let age = now - hit.at;
            let empty = hit.value.as_object().map(|m| m.is_empty()).unwrap_or(true);
            if (!empty && age < TTL) || (empty && age < NEG_TTL) {
                return hit.value;
            }
        }
        let got = self.query_profiles(&[name.to_string()]).await;
        let p = got
            .get(name)
            .cloned()
            .or_else(|| {
                let n = name.replace([' ', '\u{3000}'], "");
                got.iter()
                    .find(|(k, _)| k.replace([' ', '\u{3000}'], "") == n)
                    .map(|(_, v)| v.clone())
            })
            .unwrap_or_else(|| json!({}));
        self.profiles
            .write()
            .insert(name.to_string(), MetaEntry { value: p.clone(), at: now });
        p
    }

    pub async fn profiles(&self, names: &[String]) -> (Value, Value) {
        let mut out = Map::new();
        let mut todo = Vec::new();
        let now = chrono::Utc::now().timestamp();
        for n in names.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if let Some(hit) = self.profiles.read().get(n).cloned() {
                let age = now - hit.at;
                let empty = hit.value.as_object().map(|m| m.is_empty()).unwrap_or(true);
                if (!empty && age < TTL) || (empty && age < NEG_TTL) {
                    if !empty {
                        out.insert(n.to_string(), json!({"facts": format_profile(&hit.value), "profile": hit.value}));
                    }
                    continue;
                }
            }
            todo.push(n.to_string());
        }
        if !todo.is_empty() {
            let got = self.query_profiles(&todo).await;
            for n in todo {
                let p = got
                    .get(&n)
                    .cloned()
                    .or_else(|| {
                        let norm = n.replace([' ', '\u{3000}'], "");
                        got.iter()
                            .find(|(k, _)| k.replace([' ', '\u{3000}'], "") == norm)
                            .map(|(_, v)| v.clone())
                    })
                    .unwrap_or_else(|| json!({}));
                let empty = p.as_object().map(|m| m.is_empty()).unwrap_or(true);
                self.profiles
                    .write()
                    .insert(n.clone(), MetaEntry { value: p.clone(), at: now });
                if !empty {
                    out.insert(n, json!({"facts": format_profile(&p), "profile": p}));
                }
            }
        }
        (Value::Object(out), self.banned())
    }

    // ------------------------------------------------------------- cache actors
    fn load_movie_actors(&self) -> Map<String, Value> {
        let p = self.cache_dir().join("movie_index.json");
        read_json(&p)
            .and_then(|v| v.get("actors").and_then(Value::as_object).cloned())
            .unwrap_or_default()
    }

    fn load_usage_actors(&self) -> Map<String, Value> {
        let p = self.cache_dir().join("usage.json");
        read_json(&p)
            .and_then(|v| v.get("actors").and_then(Value::as_object).cloned())
            .unwrap_or_default()
    }

    fn load_actor_dirs(&self) -> Vec<(String, Vec<String>)> {
        let p = self.state.dir.join("actor_dirs.json");
        let Some(entries) = read_json(&p).and_then(|v| v.get("entries").and_then(Value::as_object).cloned()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for ent in entries.values() {
            let name = ent.get("name").and_then(Value::as_str).unwrap_or("").trim();
            if name.is_empty() {
                continue;
            }
            let paths = ent
                .get("paths")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_else(Vec::new);
            out.push((name.to_string(), paths));
        }
        out
    }

    fn usable_name(name: &str) -> bool {
        !name.contains('\u{fffd}') && name.matches('?').count() < 2 && !norm_actor_name(name).is_empty()
    }

    pub fn actors_list(&self) -> Vec<Value> {
        let movies = self.load_movie_actors();
        let usage = self.load_usage_actors();
        let actor_dirs: HashMap<String, Vec<String>> = self
            .load_actor_dirs()
            .into_iter()
            .map(|(name, paths)| (norm_actor_name(&name).to_lowercase(), paths))
            .collect();
        let mut out: HashMap<String, Value> = HashMap::new();
        for (name, entry) in &movies {
            if !Self::usable_name(name) {
                continue;
            }
            let codes = entry.get("codes").and_then(Value::as_array).cloned().unwrap_or_default();
            let folders = entry.get("folders").and_then(Value::as_array).cloned().unwrap_or_default();
            out.insert(name.clone(), json!({
                "name": name,
                "count": codes.len(),
                "works": codes.len(),
                "codes": codes,
                "folders": folders,
                "path": folders.first().and_then(Value::as_str).unwrap_or(""),
                "scanned": true,
                "downloads": 0,
                "matches": 0,
                "pushes": 0,
                "facts": "",
                "avatar_url": "",
            }));
        }
        // NFO/search docs 里的真实演员（没有演员缓存时也能显示，但不会把目录名当演员）
        for doc in self.search.docs_snapshot() {
            for actor in &doc.actors {
                let name = actor.trim();
                if !Self::usable_name(name) {
                    continue;
                }
                let ent = out.entry(name.to_string()).or_insert_with(|| json!({
                    "name": name, "count": 0, "works": 0, "codes": [], "folders": [],
                    "path": doc.dir.clone(), "scanned": false, "downloads": 0, "matches": 0,
                    "pushes": 0, "facts": "", "avatar_url": "",
                }));
                ent["count"] = json!(ent["count"].as_i64().unwrap_or(0) + 1);
                ent["works"] = ent["count"].clone();
                if ent["path"].as_str().unwrap_or("").is_empty() {
                    ent["path"] = json!(doc.dir);
                }
            }
        }
        for (name, u) in &usage {
            if !Self::usable_name(name) {
                continue;
            }
            let paths = actor_dirs
                .get(&norm_actor_name(name).to_lowercase())
                .cloned()
                .unwrap_or_default();
            let ent = out.entry(name.clone()).or_insert_with(|| json!({
                "name": name, "count": 0, "works": 0, "codes": [], "folders": paths.clone(),
                "path": paths.first().cloned().unwrap_or_default(), "scanned": false, "downloads": 0, "matches": 0,
                "pushes": 0, "facts": "", "avatar_url": "",
            }));
            ent["downloads"] = u.get("downloads").cloned().unwrap_or(json!(0));
            ent["matches"] = u.get("matches").cloned().unwrap_or(json!(0));
            ent["pushes"] = u.get("downloads").cloned().unwrap_or(json!(0));
            if let Some(codes) = u.get("recent_codes").and_then(Value::as_array) {
                if ent["works"].as_i64().unwrap_or(0) == 0 && !codes.is_empty() {
                    ent["codes"] = Value::Array(codes.clone());
                    ent["works"] = json!(codes.len());
                    ent["count"] = json!(codes.len());
                }
            }
        }
        let mut items: Vec<Value> = out.into_values().collect();
        items.sort_by(|a, b| {
            b.get("works").and_then(Value::as_i64).unwrap_or(0)
                .cmp(&a.get("works").and_then(Value::as_i64).unwrap_or(0))
                .then_with(|| a.get("name").and_then(Value::as_str).unwrap_or("").cmp(b.get("name").and_then(Value::as_str).unwrap_or("")))
        });
        items
    }

    pub fn ranking(&self, limit: usize) -> Value {
        let actors = self.actors_list();
        let mut items: Vec<Value> = actors
            .into_iter()
            .map(|a| {
                let downloads = a.get("downloads").and_then(Value::as_i64).unwrap_or(0);
                let matches = a.get("matches").and_then(Value::as_i64).unwrap_or(0);
                let works = a.get("works").and_then(Value::as_i64).unwrap_or(0);
                let heat = downloads * 5 + matches * 2 + works;
                json!({
                    "name": a.get("name"), "works": works, "downloads": downloads,
                    "matches": matches, "heat": heat, "path": a.get("path"),
                    "avatar_url": "", "facts": "", "count": works,
                })
            })
            .collect();
        items.sort_by(|a, b| b.get("heat").and_then(Value::as_i64).unwrap_or(0).cmp(&a.get("heat").and_then(Value::as_i64).unwrap_or(0)));
        let total = items.len();
        items.truncate(limit);
        json!({"ok": true, "items": items, "total": total})
    }

    pub fn cache_status(&self) -> Value {
        let movies = self.load_movie_actors();
        let actors_total = movies.len();
        let works: i64 = movies
            .values()
            .map(|v| v.get("codes").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0) as i64)
            .sum();
        json!({"actors_total": actors_total, "movies_total": works})
    }
}

// 判断一个名字是否适合作为演员展示，过滤结构目录。
pub fn is_actor_name(name: &str) -> bool {
    let n = norm_actor_name(name);
    if n.is_empty() {
        return false;
    }
    let bad = [
        "记录文件", "云下载", "电影", "电视剧", "动漫", "综艺", "资源", "下载", "已整理",
        "未整理", "Zpool", "movies", "movie", "tv", "anime", "AI", "others", "other",
    ];
    if bad.iter().any(|b| n.eq_ignore_ascii_case(b)) {
        return false;
    }
    // 纯 ASCII 1-2 个字符通常不是演员名（AI/AV 等）
    if n.chars().all(|c| c.is_ascii_alphanumeric()) && n.chars().count() <= 2 {
        return false;
    }
    true
}

pub fn actor_from_path_safe(path: &str) -> String {
    let a = actor_from_path(path);
    if is_actor_name(&a) { a } else { String::new() }
}
