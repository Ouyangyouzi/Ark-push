use crate::litepan::LitePanPool;
use crate::search::pinyin_key;
use crate::state::StateStore;
use crate::util::{atomic_write, nfkc, read_json};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const MAX_ITEMS: usize = 80_000;
const MAX_DEPTH: usize = 10;
const MAX_CALLS: usize = 12_000;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileItem {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_dir: bool,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub account_id: i64,
}

#[derive(Default, Clone)]
pub struct AccountIndex {
    pub items: Vec<FileItem>,
    pub ts: i64,
    pub roots: Vec<String>,
    pub error: String,
    pub scanned: usize,
    pub building: bool,
}

pub struct FileIndex {
    state_dir: PathBuf,
    accounts: Arc<RwLock<HashMap<i64, Arc<AccountIndex>>>>,
    seeded: Arc<RwLock<HashSet<i64>>>,
    building: Arc<RwLock<HashSet<i64>>>,
    pool: Arc<LitePanPool>,
    /// 搜索用的小写/归一化文件名缓存，避免每次查询都对 5 万条重新计算
    names: Arc<RwLock<HashMap<i64, NameCache>>>,
    /// 目录列表缓存，避免 actor/match 每次重建几万个 JSON 对象
    dirs_cache: Arc<RwLock<HashMap<String, Arc<Vec<Value>>>>>,
}

#[derive(Clone)]
struct NameCache {
    ts: i64,
    len: usize,
    names: Arc<Vec<(String, String)>>,
}

impl FileIndex {
    pub fn new(state_dir: &str, pool: Arc<LitePanPool>) -> Self {
        let _ = std::fs::create_dir_all(Path::new(state_dir).join("fileindex"));
        Self {
            state_dir: PathBuf::from(state_dir),
            accounts: Arc::new(RwLock::new(HashMap::new())),
            seeded: Arc::new(RwLock::new(HashSet::new())),
            building: Arc::new(RwLock::new(HashSet::new())),
            pool,
            names: Arc::new(RwLock::new(HashMap::new())),
            dirs_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn path_for(&self, account_id: i64) -> PathBuf {
        self.state_dir.join("fileindex").join(format!("{account_id}.json"))
    }

    fn load(&self, account_id: i64) -> Option<AccountIndex> {
        let v = read_json(&self.path_for(account_id))?;
        let items: Vec<FileItem> = serde_json::from_value(v.get("items")?.clone()).ok()?;
        Some(AccountIndex {
            items,
            ts: v.get("ts").and_then(Value::as_i64).unwrap_or(0),
            roots: v
                .get("roots")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
            error: String::new(),
            scanned: 0,
            building: false,
        })
    }

    fn save(&self, account_id: i64, idx: &AccountIndex) {
        let payload = json!({
            "items": idx.items,
            "ts": idx.ts,
            "roots": idx.roots,
        });
        if let Ok(raw) = serde_json::to_vec(&payload) {
            let _ = atomic_write(&self.path_for(account_id), &raw, None);
        }
    }

    fn seed(&self, account_id: i64) {
        let mut seeded = self.seeded.write();
        if seeded.contains(&account_id) {
            return;
        }
        seeded.insert(account_id);
        drop(seeded);
        if let Some(mut loaded) = self.load(account_id) {
            loaded.scanned = loaded.items.len();
            self.accounts.write().insert(account_id, Arc::new(loaded));
        }
    }

    pub fn ensure_known(&self) {
        let dir = self.state_dir.join("fileindex");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(stem) = name.strip_suffix(".json") {
                if let Ok(id) = stem.parse::<i64>() {
                    self.seed(id);
                }
            }
        }
    }

    pub fn status(&self) -> Vec<Value> {
        self.accounts
            .read()
            .iter()
            .map(|(acc, idx)| {
                json!({
                    "account_id": acc,
                    "items": idx.scanned.max(idx.items.len()),
                    "building": idx.building || self.building.read().contains(acc),
                    "roots": idx.roots,
                    "age": if idx.ts > 0 { chrono::Utc::now().timestamp() - idx.ts } else { -1 },
                    "error": idx.error,
                })
            })
            .collect()
    }

    pub fn signature(&self) -> String {
        let mut parts: Vec<String> = self
            .accounts
            .read()
            .iter()
            .map(|(acc, idx)| format!("{acc}:{}:{}", idx.scanned.max(idx.items.len()), idx.ts))
            .collect();
        parts.sort();
        parts.join("|")
    }

    pub fn ensure(&self, account_id: i64, roots: &[String]) -> Value {
        let account_id = account_id.max(0);
        self.seed(account_id);
        let st = self.accounts.read().get(&account_id).cloned();
        let (items, ts, error, building) = match st {
            Some(st) => (st.items.len(), st.ts, st.error.clone(), st.building),
            None => (0, 0, String::new(), false),
        };
        let roots_owned: Vec<String> = if roots.is_empty() {
            vec![String::new()]
        } else {
            roots.iter().map(|r| r.trim_matches('/').to_string()).collect()
        };
        let mut start = false;
        if items == 0 && !building {
            let mut b = self.building.write();
            if !b.contains(&account_id) {
                b.insert(account_id);
                start = true;
            }
        }
        if start {
            let pool = self.pool.clone();
            let state_dir = self.state_dir.clone();
            let accounts = self.accounts.clone();
            let building_set = self.building.clone();
            let roots_clone = roots_owned.clone();
            tokio::spawn(async move {
                let result = build_account(&pool, account_id, &roots_clone).await;
                let mut map = accounts.write();
                match result {
                    Ok((items, calls)) => {
                        let idx = AccountIndex {
                            scanned: items.len(),
                            items,
                            ts: chrono::Utc::now().timestamp(),
                            roots: roots_clone,
                            error: String::new(),
                            building: false,
                        };
                        let payload = json!({"items": idx.items, "ts": idx.ts, "roots": idx.roots});
                        if let Ok(raw) = serde_json::to_vec(&payload) {
                            let _ = atomic_write(
                                &state_dir.join("fileindex").join(format!("{account_id}.json")),
                                &raw,
                                None,
                            );
                        }
                        tracing::info!("fileindex rebuilt account={account_id}: {} items, {calls} calls", idx.items.len());
                        map.insert(account_id, Arc::new(idx));
                    }
                    Err(e) => {
                        if let Some(old) = map.get(&account_id) {
                            let mut new = AccountIndex {
                                items: old.items.clone(),
                                ts: old.ts,
                                roots: old.roots.clone(),
                                error: e.to_string(),
                                scanned: old.scanned,
                                building: false,
                            };
                            new.building = false;
                            map.insert(account_id, Arc::new(new));
                        } else {
                            map.insert(account_id, Arc::new(AccountIndex {
                                error: e.to_string(),
                                ..Default::default()
                            }));
                        }
                        tracing::warn!("fileindex rebuild failed account={account_id}: {e}");
                    }
                }
                building_set.write().remove(&account_id);
            });
        }
        json!({
            "items": items,
            "building": building || self.building.read().contains(&account_id),
            "scanned": items,
            "age": if ts > 0 { chrono::Utc::now().timestamp() - ts } else { -1 },
            "error": error,
        })
    }

    pub fn force(&self, account_id: i64, roots: &[String]) {
        self.accounts.write().remove(&account_id);
        self.seeded.write().remove(&account_id);
        let _ = self.ensure(account_id, roots);
    }

    pub fn all_items(&self) -> Vec<FileItem> {
        let mut out = Vec::new();
        for idx in self.accounts.read().values() {
            out.extend(idx.items.iter().cloned());
        }
        out
    }

    pub fn all_dirs(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for (acc, idx) in self.accounts.read().iter() {
            for it in &idx.items {
                if it.is_dir {
                    out.push(json!({
                        "account_id": *acc, "id": it.id, "name": it.name,
                        "path": it.path.trim_matches('/'),
                    }));
                }
            }
        }
        out
    }

    pub fn dirs(&self, account_id: i64, roots: &[String], depth: usize, limit: usize) -> Vec<Value> {
        self.seed(account_id);
        let Some(idx) = self.accounts.read().get(&account_id).cloned() else {
            return Vec::new();
        };
        let norm_roots: Vec<String> = roots
            .iter()
            .map(|r| r.trim_matches('/').to_string())
            .filter(|r| !r.is_empty())
            .collect();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for it in &idx.items {
            if !it.is_dir {
                continue;
            }
            let p = it.path.trim_matches('/');
            if p.is_empty() || !seen.insert(p.to_string()) {
                continue;
            }
            let mut rel = p;
            if !norm_roots.is_empty() {
                let mut matched = false;
                for r in &norm_roots {
                    if p == r || p.starts_with(&format!("{r}/")) {
                        rel = p[r.len()..].trim_matches('/');
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    continue;
                }
            }
            let level = rel.split('/').filter(|s| !s.is_empty()).count();
            if level > depth.max(1) {
                continue;
            }
            out.push(json!({"path": p, "name": it.name}));
            if out.len() >= limit.max(1) {
                break;
            }
        }
        out
    }

    /// 目录列表（带签名缓存）：5 万条索引时避免每次 actor/match 都重建 JSON。
    pub fn all_dirs_cached(&self) -> Arc<Vec<Value>> {
        let sig = self.signature();
        if let Some(v) = self.dirs_cache.read().get(&sig) {
            return v.clone();
        }
        let v = Arc::new(self.all_dirs());
        let mut w = self.dirs_cache.write();
        w.clear();
        w.insert(sig, v.clone());
        v
    }

    fn names_for(&self, account_id: i64, idx: &AccountIndex) -> Arc<Vec<(String, String)>> {
        if let Some(c) = self.names.read().get(&account_id) {
            if c.ts == idx.ts && c.len == idx.items.len() {
                return c.names.clone();
            }
        }
        let names: Vec<(String, String)> = idx
            .items
            .iter()
            .map(|it| (it.name.to_lowercase(), norm(&it.name)))
            .collect();
        let names = Arc::new(names);
        self.names.write().insert(
            account_id,
            NameCache {
                ts: idx.ts,
                len: idx.items.len(),
                names: names.clone(),
            },
        );
        names
    }

    pub fn search(&self, keyword: &str, limit: usize, account_id: i64) -> Vec<Value> {
        self.seed(account_id);
        let q = norm(keyword);
        if q.is_empty() {
            return Vec::new();
        }
        let kw = keyword.trim().to_lowercase();
        let accounts: Vec<i64> = if account_id > 0 {
            vec![account_id]
        } else {
            self.accounts.read().keys().copied().collect()
        };
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        // 第一遍：小写包含 / 归一化包含。命中后直接返回，不再做拼音计算。
        for acc in &accounts {
            let Some(idx) = self.accounts.read().get(acc).cloned() else {
                continue;
            };
            let names = self.names_for(*acc, &idx);
            for (i, it) in idx.items.iter().enumerate() {
                let (nlow, nn) = &names[i];
                let hit = (!kw.is_empty() && nlow.contains(&kw)) || (!q.is_empty() && nn.contains(&q));
                if !hit {
                    continue;
                }
                if !seen.insert((it.path.clone(), it.account_id)) {
                    continue;
                }
                out.push(json!({
                    "path": it.path, "name": it.name, "is_dir": it.is_dir,
                    "size": it.size, "id": it.id, "account_id": it.account_id,
                }));
                if out.len() >= limit {
                    return out;
                }
            }
        }
        // 第二遍：只有完全没命中且关键词含中文时，才对目录做拼音兜底（波多野结衣 → 波多野結衣）。
        if out.is_empty() && has_cjk(keyword) {
            let qp = pinyin_key(keyword);
            if !qp.is_empty() {
                for acc in &accounts {
                    let Some(idx) = self.accounts.read().get(acc).cloned() else {
                        continue;
                    };
                    for it in &idx.items {
                        if !it.is_dir {
                            continue;
                        }
                        let hp = pinyin_key(&it.name);
                        if hp.is_empty() || !hp.contains(&qp) {
                            continue;
                        }
                        if !seen.insert((it.path.clone(), it.account_id)) {
                            continue;
                        }
                        out.push(json!({
                            "path": it.path, "name": it.name, "is_dir": it.is_dir,
                            "size": it.size, "id": it.id, "account_id": it.account_id,
                        }));
                        if out.len() >= limit {
                            return out;
                        }
                    }
                }
            }
        }
        out
    }

    pub fn remove_paths(&self, account_id: i64, paths: &HashSet<String>) -> usize {
        if account_id == 0 || paths.is_empty() {
            return 0;
        }
        self.seed(account_id);
        let mut map = self.accounts.write();
        let Some(entry) = map.get(&account_id) else {
            return 0;
        };
        let mut idx = (**entry).clone();
        let before = idx.items.len();
        idx.items.retain(|it| !paths.contains(it.path.trim_matches('/')));
        let removed = before.saturating_sub(idx.items.len());
        if removed > 0 {
            idx.scanned = idx.items.len();
            idx.ts = chrono::Utc::now().timestamp();
            let payload = json!({"items": idx.items, "ts": idx.ts, "roots": idx.roots});
            if let Ok(raw) = serde_json::to_vec(&payload) {
                let _ = atomic_write(&self.path_for(account_id), &raw, None);
            }
            map.insert(account_id, Arc::new(idx));
            tracing::info!("fileindex removed {} stale entries account={}", removed, account_id);
        }
        removed
    }

    pub fn upsert(&self, account_id: i64, items: &[Value]) -> usize {
        if account_id == 0 || items.is_empty() {
            return 0;
        }
        self.seed(account_id);
        let mut map = self.accounts.write();
        let entry = map.entry(account_id).or_insert_with(|| Arc::new(AccountIndex::default()));
        let mut idx = (**entry).clone();
        let mut keys: HashSet<(String, String)> =
            idx.items.iter().map(|i| (i.id.clone(), i.path.clone())).collect();
        let mut added = 0;
        for it in items {
            let id = it.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let name = it.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let path = it.get("path").and_then(Value::as_str).unwrap_or("").trim_matches('/').to_string();
            if id.is_empty() || name.is_empty() || path.is_empty() {
                continue;
            }
            if !keys.insert((id.clone(), path.clone())) {
                continue;
            }
            idx.items.push(FileItem {
                path,
                name,
                is_dir: it.get("is_dir").and_then(Value::as_bool).unwrap_or(false),
                size: it.get("size").and_then(Value::as_i64).unwrap_or(0),
                id,
                account_id,
            });
            added += 1;
        }
        if added > 0 {
            idx.scanned = idx.items.len();
            idx.ts = chrono::Utc::now().timestamp();
            let payload = json!({"items": idx.items, "ts": idx.ts, "roots": idx.roots});
            if let Ok(raw) = serde_json::to_vec(&payload) {
                let _ = atomic_write(&self.path_for(account_id), &raw, None);
            }
            *entry = Arc::new(idx);
        }
        added
    }
}

async fn build_account(
    pool: &LitePanPool,
    account_id: i64,
    roots: &[String],
) -> Result<(Vec<FileItem>, usize), String> {
    let client = pool.client_for(&Value::Null).await.map_err(|e| e.to_string())?;
    let localfs = client.localfs_account_id().await.unwrap_or(0) == account_id;
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let mut calls = 0usize;
    for root in roots {
        let base = format!("/{}", root.trim_matches('/'));
        let pid = if localfs {
            base.clone()
        } else {
            match client.resolve_dir(account_id, &base, false).await {
                Ok(v) => v.get("parent_id").and_then(Value::as_str).unwrap_or("").to_string(),
                Err(e) => {
                    tracing::warn!("fileindex root {root} failed: {e}");
                    continue;
                }
            }
        };
        if pid.is_empty() && base != "/" {
            continue;
        }
        let mut queue: Vec<(String, String, usize)> = vec![(pid, base, 0)];
        let mut head = 0;
        while head < queue.len() && items.len() < MAX_ITEMS && calls < MAX_CALLS {
            let (cpid, cur, depth) = queue[head].clone();
            head += 1;
            calls += 1;
            let listed = match client.list_dir(account_id, &cpid, "", None).await {
                Ok(v) => v,
                Err(_) => continue,
            };
            let rows = listed.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            for it in rows {
                let name = it.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                let full = format!("{}/{}", cur.trim_end_matches('/'), name)
                    .trim_matches('/')
                    .to_string();
                if !seen.insert(full.clone()) {
                    continue;
                }
                let is_dir = it.get("is_dir").and_then(Value::as_bool).unwrap_or(false);
                let id = it
                    .get("id")
                    .map(|v| v.to_string().trim_matches('"').to_string())
                    .unwrap_or_default();
                items.push(FileItem {
                    path: full.clone(),
                    name,
                    is_dir,
                    size: it.get("size").and_then(Value::as_i64).unwrap_or(0),
                    id: id.clone(),
                    account_id,
                });
                if is_dir && !id.is_empty() && depth + 1 <= MAX_DEPTH {
                    queue.push((id, format!("/{full}"), depth + 1));
                }
            }
        }
    }
    Ok((items, calls))
}

fn norm(v: &str) -> String {
    nfkc(v)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || (*c as u32) >= 0x4e00)
        .collect()
}

fn has_cjk(v: &str) -> bool {
    v.chars().any(|c| (c as u32) >= 0x4e00)
}
