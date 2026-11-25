use crate::search::{
    doc_id, eval_filter, highlight_text, match_keyword, pinyin_key, tokenize, SearchDoc,
};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

const SCHEMA: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS docs USING fts5(
    id UNINDEXED,
    title,
    aliases,
    nfo_title,
    code,
    stem,
    plot,
    tagline,
    actors,
    directors,
    studio,
    genres,
    path,
    filename,
    doc UNINDEXED,
    type UNINDEXED,
    year UNINDEXED,
    netdisk UNINDEXED,
    ext UNINDEXED,
    season UNINDEXED,
    has_nfo UNINDEXED,
    size UNINDEXED,
    mtime UNINDEXED,
    rating UNINDEXED,
    tokenize='trigram'
);
"#;

pub struct SearchStore {
    path: PathBuf,
    conn: Mutex<Connection>,
}

impl SearchStore {
    pub fn new(state_dir: &str) -> Self {
        let path = Path::new(state_dir).join("search_fts.db");
        let conn = Connection::open(&path).expect("open search fts db");
        // WAL + NORMAL 大幅降低 Windows 挂载盘上的 fsync 开销（否则整理后写入索引会卡很久）。
        let _ = conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=8000;",
        );
        conn.execute_batch(SCHEMA).expect("create fts schema");
        let store = Self {
            path,
            conn: Mutex::new(conn),
        };
        store.migrate_legacy(state_dir);
        store
    }

    fn migrate_legacy(&self, state_dir: &str) {
        if self.count() > 0 {
            return;
        }
        let old = Path::new(state_dir).join("search_index.json");
        let Some(v) = crate::util::read_json(&old) else {
            return;
        };
        let Some(arr) = v.get("docs").and_then(Value::as_array) else {
            return;
        };
        let docs: Vec<SearchDoc> = arr
            .iter()
            .filter_map(|x| serde_json::from_value(x.clone()).ok())
            .collect();
        if !docs.is_empty() {
            tracing::info!("migrating {} docs from search_index.json to FTS5", docs.len());
            self.set_docs(docs);
        }
    }

    pub fn count(&self) -> usize {
        let conn = self.conn.lock();
        conn.query_row("SELECT count(*) FROM docs", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    pub fn stats(&self) -> Value {
        let count = self.count();
        let bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        json!({
            "numberOfDocuments": count,
            "isIndexing": false,
            "backend": "sqlite-fts5",
            "dbBytes": bytes,
        })
    }

    pub fn docs_snapshot(&self) -> Vec<SearchDoc> {
        let conn = self.conn.lock();
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare("SELECT doc FROM docs") {
            if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                for raw in rows.flatten() {
                    if let Ok(d) = serde_json::from_str::<SearchDoc>(&raw) {
                        out.push(d);
                    }
                }
            }
        }
        out
    }

    pub fn set_docs(&self, docs: Vec<SearchDoc>) {
        let mut conn = self.conn.lock();
        let Ok(tx) = conn.transaction() else { return };
        let _ = tx.execute("DELETE FROM docs", []);
        for d in &docs {
            let _ = insert_doc(&tx, d);
        }
        let _ = tx.commit();
        tracing::info!("FTS5 index rebuilt: {} docs", docs.len());
    }

    pub fn upsert(&self, docs: Vec<SearchDoc>) {
        if docs.is_empty() {
            return;
        }
        let mut conn = self.conn.lock();
        let Ok(tx) = conn.transaction() else { return };
        for d in &docs {
            let _ = tx.execute("DELETE FROM docs WHERE id=?", params![d.id]);
            let _ = insert_doc(&tx, d);
        }
        let _ = tx.commit();
    }

    pub fn delete(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let mut conn = self.conn.lock();
        let Ok(tx) = conn.transaction() else { return };
        for id in ids {
            let _ = tx.execute("DELETE FROM docs WHERE id=?", params![id]);
        }
        let _ = tx.commit();
    }

    pub fn clear(&self) {
        let conn = self.conn.lock();
        let _ = conn.execute("DELETE FROM docs", []);
    }

    pub fn search(&self, payload: &Value) -> Value {
        let q = payload.get("q").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let limit = payload
            .get("limit")
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(20)
            .clamp(1, 5000) as usize;
        let offset = payload
            .get("offset")
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0)
            .max(0) as usize;
        let filter = payload.get("filter").and_then(Value::as_str).unwrap_or("");
        let sort = payload.get("sort").and_then(Value::as_str).unwrap_or("");
        let facet_fields: Vec<String> = payload
            .get("facets")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let retrieve: Option<Vec<String>> = payload
            .get("attributesToRetrieve")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect());
        let highlight: Vec<String> = payload
            .get("attributesToHighlight")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let crop: Vec<String> = payload
            .get("attributesToCrop")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let crop_len = payload.get("cropLength").and_then(Value::as_i64).unwrap_or(200).clamp(20, 2000) as usize;
        let show_score = payload.get("showRankingScore").and_then(Value::as_bool).unwrap_or(false);
        let q_tokens = tokenize(&q);

        let mut candidates: Vec<(SearchDoc, f64)> = Vec::new();
        {
            let conn = self.conn.lock();
            let rows: Vec<(String, f64)> = if q.is_empty() {
                let mut stmt = match conn.prepare("SELECT doc FROM docs LIMIT 20000") {
                    Ok(s) => s,
                    Err(_) => return empty_result(&q, limit, offset),
                };
                let iter = match stmt.query_map([], |r| r.get::<_, String>(0)) {
                    Ok(v) => v,
                    Err(_) => return empty_result(&q, limit, offset),
                };
                iter.flatten().map(|s| (s, 0.0)).collect()
            } else {
                let expr = fts_expr(&q);
                let mut out = Vec::new();
                if let Ok(mut stmt) = conn.prepare(
                    "SELECT doc, bm25(docs) FROM docs WHERE docs MATCH ? ORDER BY bm25(docs) LIMIT 20000",
                ) {
                    if let Ok(iter) = stmt.query_map(params![expr], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
                    }) {
                        out.extend(iter.flatten());
                    }
                }
                // trigram 对 1-2 字短查询可能无结果，用 LIKE 兜底
                if out.is_empty() {
                    let like = format!("%{}%", q.replace('%', "").replace('_', ""));
                    if let Ok(mut stmt) = conn.prepare(
                        "SELECT doc, 0.0 FROM docs WHERE title LIKE ? OR aliases LIKE ? OR path LIKE ? OR filename LIKE ? LIMIT 20000",
                    ) {
                        if let Ok(iter) = stmt.query_map(params![like, like, like, like], |r| {
                            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
                        }) {
                            out.extend(iter.flatten());
                        }
                    }
                }
                // 简繁/异体：中文查询再用拼音别名兜底（波多野结衣 → 波多野結衣）
                if out.is_empty() && q.chars().any(|c| (c as u32) >= 0x4e00) {
                    if let Some(py) = pinyin_key(&q).split_whitespace().next() {
                        let like = format!("%{}%", py);
                        if let Ok(mut stmt) = conn.prepare(
                            "SELECT doc, 0.0 FROM docs WHERE aliases LIKE ? OR title LIKE ? OR path LIKE ? LIMIT 20000",
                        ) {
                            if let Ok(iter) = stmt.query_map(params![like, like, like], |r| {
                                Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
                            }) {
                                out.extend(iter.flatten());
                            }
                        }
                    }
                }
                let mut seen = std::collections::HashSet::new();
                out.retain(|(raw, _)| {
                    let id = serde_json::from_str::<SearchDoc>(raw)
                        .map(|d| d.id)
                        .unwrap_or_else(|_| raw.clone());
                    seen.insert(id)
                });
                out
            };
            for (raw, rank) in rows {
                let Ok(d) = serde_json::from_str::<SearchDoc>(&raw) else { continue };
                if !filter.is_empty() && !eval_filter(&d, filter) {
                    continue;
                }
                // FTS5 的 bm25 越小越相关；转成越大越好
                candidates.push((d, -rank));
            }
        }

        if !sort.is_empty() {
            sort_docs(&mut candidates, sort);
        }
        let total = candidates.len();
        let max_score = candidates.iter().map(|(_, s)| *s).fold(0.0f64, f64::max).max(0.0001);
        let page: Vec<(SearchDoc, f64)> = candidates.into_iter().skip(offset).take(limit).collect();
        let mut hits = Vec::with_capacity(page.len());
        for (d, score) in page {
            let mut obj = doc_json(&d);
            if !highlight.is_empty() || !crop.is_empty() {
                let mut formatted = Map::new();
                for field in &highlight {
                    if let Some(text) = field_text(&d, field) {
                        formatted.insert(field.clone(), Value::String(highlight_text(&text, &q, &q_tokens)));
                    }
                }
                for field in &crop {
                    if let Some(text) = field_text(&d, field) {
                        formatted.insert(field.clone(), Value::String(if text.chars().count() > crop_len {
                            format!("{}...", text.chars().take(crop_len).collect::<String>())
                        } else {
                            text
                        }));
                    }
                }
                obj.insert("_formatted".into(), Value::Object(formatted));
            }
            if show_score {
                obj.insert("_rankingScore".into(), json!((score / max_score).clamp(0.0, 1.0)));
            }
            if let Some(retrieve) = &retrieve {
                let mut filtered = Map::new();
                for key in retrieve {
                    if let Some(v) = obj.get(key) {
                        filtered.insert(key.clone(), v.clone());
                    }
                }
                for key in ["_formatted", "_rankingScore"] {
                    if let Some(v) = obj.get(key) {
                        filtered.insert(key.into(), v.clone());
                    }
                }
                hits.push(Value::Object(filtered));
            } else {
                hits.push(Value::Object(obj));
            }
        }

        let mut facets_obj = Map::new();
        if !facet_fields.is_empty() {
            let conn = self.conn.lock();
            for field in &facet_fields {
                let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
                if let Ok(mut stmt) = conn.prepare("SELECT doc FROM docs") {
                    if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                        for raw in rows.flatten() {
                            let Ok(d) = serde_json::from_str::<SearchDoc>(&raw) else { continue };
                            if !filter.is_empty() && !eval_filter(&d, filter) {
                                continue;
                            }
                            for v in facet_values(&d, field) {
                                *counts.entry(v).or_insert(0) += 1;
                            }
                        }
                    }
                }
                let mut sorted: Vec<(String, i64)> = counts.into_iter().collect();
                sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                let mut m = Map::new();
                for (k, v) in sorted {
                    m.insert(k, json!(v));
                }
                facets_obj.insert(field.clone(), Value::Object(m));
            }
        }

        let mut result = Map::new();
        result.insert("hits".into(), Value::Array(hits));
        result.insert("query".into(), Value::String(q));
        result.insert("processingTimeMs".into(), json!(0));
        result.insert("limit".into(), json!(limit));
        result.insert("offset".into(), json!(offset));
        result.insert("estimatedTotalHits".into(), json!(total));
        result.insert("nbHits".into(), json!(total));
        if !facet_fields.is_empty() {
            result.insert("facetDistribution".into(), Value::Object(facets_obj));
        }
        Value::Object(result)
    }
}

fn insert_doc(conn: &Connection, d: &SearchDoc) -> rusqlite::Result<()> {
    let doc = serde_json::to_string(d).unwrap_or_default();
    let aliases = d.aliases.join(" ");
    let actors = d.actors.join(" ");
    let directors = d.directors.join(" ");
    let genres = d.genres.join(" ");
    conn.execute(
        "INSERT INTO docs(id,title,aliases,nfo_title,code,stem,plot,tagline,actors,directors,studio,genres,path,filename,doc,type,year,netdisk,ext,season,has_nfo,size,mtime,rating)
         VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            d.id, d.title, aliases, d.nfo_title, d.code, d.stem, d.plot, d.tagline,
            actors, directors, d.studio, genres, d.path, d.filename, doc,
            d.r#type, d.year, d.netdisk, d.ext, d.season,
            if d.has_nfo { 1 } else { 0 }, d.size, d.mtime, d.rating
        ],
    )?;
    Ok(())
}

fn fts_expr(q: &str) -> String {
    // 引号包裹成 phrase，避免用户输入被解释成 FTS5 语法；trigram 支持子串。
    let cleaned = q.replace('"', " ");
    format!("\"{}\"", cleaned.trim())
}

fn empty_result(q: &str, limit: usize, offset: usize) -> Value {
    json!({"hits": [], "query": q, "processingTimeMs": 0, "limit": limit, "offset": offset,
           "estimatedTotalHits": 0, "nbHits": 0})
}

fn doc_json(d: &SearchDoc) -> Map<String, Value> {
    json!({
        "id": d.id, "title": d.title, "stem": d.stem, "code": d.code,
        "aliases": d.aliases, "nfo_title": d.nfo_title, "plot": d.plot,
        "tagline": d.tagline, "actors": d.actors, "directors": d.directors,
        "genres": d.genres, "studio": d.studio, "rating": d.rating,
        "tmdb_id": d.tmdb_id, "year": d.year, "type": d.r#type,
        "season": d.season, "episode": d.episode, "netdisk": d.netdisk,
        "dir": d.dir, "path": d.path, "filename": d.filename, "ext": d.ext,
        "size": d.size, "mtime": d.mtime, "has_nfo": d.has_nfo, "poster": d.poster,
        "source": d.source,
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

fn field_text(d: &SearchDoc, field: &str) -> Option<String> {
    match field {
        "title" => Some(d.title.clone()),
        "plot" => Some(d.plot.clone()),
        "tagline" => Some(d.tagline.clone()),
        "nfo_title" => Some(d.nfo_title.clone()),
        "code" => Some(d.code.clone()),
        "stem" => Some(d.stem.clone()),
        "path" => Some(d.path.clone()),
        "filename" => Some(d.filename.clone()),
        "actors" => Some(d.actors.join(" ")),
        "genres" => Some(d.genres.join(" ")),
        _ => None,
    }
}

fn facet_values(d: &SearchDoc, field: &str) -> Vec<String> {
    match field {
        "type" => vec![d.r#type.clone()],
        "netdisk" => vec![d.netdisk.clone()],
        "ext" => vec![d.ext.clone()],
        "year" => d.year.map(|v| v.to_string()).into_iter().collect(),
        "season" => d.season.map(|v| v.to_string()).into_iter().collect(),
        "has_nfo" => vec![d.has_nfo.to_string()],
        "genres" => d.genres.clone(),
        _ => Vec::new(),
    }
}

fn sort_docs(candidates: &mut [(SearchDoc, f64)], sort: &str) {
    let mut parts = sort.split(':');
    let field = parts.next().unwrap_or("").trim();
    let asc = parts.next().map(|s| s.eq_ignore_ascii_case("asc")).unwrap_or(false);
    candidates.sort_by(|a, b| {
        let da = &a.0;
        let db = &b.0;
        let ord = match field {
            "year" => da.year.unwrap_or(0).cmp(&db.year.unwrap_or(0)),
            "mtime" => da.mtime.cmp(&db.mtime),
            "size" => da.size.cmp(&db.size),
            "rating" => da
                .rating
                .unwrap_or(0.0)
                .partial_cmp(&db.rating.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal),
            _ => b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal),
        };
        if asc { ord } else { ord.reverse() }
    });
}

// 让调用方仍可复用旧的关键词匹配工具
pub fn keyword_match(k: &str, t: &str) -> bool {
    match_keyword(k, t)
}

pub fn pinyin(k: &str) -> String {
    pinyin_key(k)
}

pub fn id_for(path: &str) -> String {
    doc_id(path)
}
