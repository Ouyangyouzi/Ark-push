use crate::util::{atomic_write, html_escape, nfkc, read_json, sha1_hex};
use parking_lot::RwLock;
use pinyin::ToPinyin;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchDoc {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub stem: String,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub nfo_title: String,
    #[serde(default)]
    pub plot: String,
    #[serde(default)]
    pub tagline: String,
    #[serde(default)]
    pub actors: Vec<String>,
    #[serde(default)]
    pub directors: Vec<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub studio: String,
    #[serde(default)]
    pub rating: Option<f64>,
    #[serde(default)]
    pub tmdb_id: String,
    #[serde(default)]
    pub year: Option<i64>,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub season: Option<i64>,
    #[serde(default)]
    pub episode: Option<i64>,
    #[serde(default)]
    pub netdisk: String,
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub ext: String,
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub mtime: i64,
    #[serde(default)]
    pub has_nfo: bool,
    #[serde(default)]
    pub poster: String,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedIndex {
    #[serde(default)]
    version: i64,
    #[serde(default)]
    built_at: i64,
    #[serde(default)]
    docs: Vec<SearchDoc>,
}

struct Inner {
    docs: Vec<SearchDoc>,
    by_id: HashMap<String, usize>,
    postings: HashMap<String, Vec<u32>>,
    doc_len: Vec<u32>,
    built_at: i64,
}

impl Inner {
    fn empty() -> Self {
        Self {
            docs: Vec::new(),
            by_id: HashMap::new(),
            postings: HashMap::new(),
            doc_len: Vec::new(),
            built_at: 0,
        }
    }

    fn build(docs: Vec<SearchDoc>) -> Self {
        let mut by_id = HashMap::new();
        for (i, d) in docs.iter().enumerate() {
            by_id.insert(d.id.clone(), i);
        }
        let mut postings: HashMap<String, Vec<u32>> = HashMap::new();
        let mut doc_len = Vec::with_capacity(docs.len());
        for (i, d) in docs.iter().enumerate() {
            let text = doc_search_text(d);
            let mut seen = HashSet::new();
            let mut count = 0u32;
            for token in tokenize(&text) {
                count += 1;
                if seen.insert(token.clone()) {
                    postings.entry(token).or_default().push(i as u32);
                }
            }
            doc_len.push(count);
        }
        Self {
            docs,
            by_id,
            postings,
            doc_len,
            built_at: chrono::Utc::now().timestamp(),
        }
    }
}

pub struct SearchStore {
    path: PathBuf,
    inner: RwLock<Inner>,
    building: AtomicBool,
}

impl SearchStore {
    pub fn new(state_dir: &str) -> Self {
        let path = Path::new(state_dir).join("search_index.json");
        let mut docs = Vec::new();
        let mut built_at = 0;
        if let Some(v) = read_json(&path) {
            if let Ok(p) = serde_json::from_value::<PersistedIndex>(v) {
                docs = p.docs;
                built_at = p.built_at;
            }
        }
        let mut inner = Inner::build(docs);
        if built_at > 0 {
            inner.built_at = built_at;
        }
        let store = Self {
            path,
            inner: RwLock::new(inner),
            building: AtomicBool::new(false),
        };
        if store.count() == 0 {
            if let Ok(raw) = std::fs::read(Path::new(state_dir).join("state.json")) {
                if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
                    if v.get("docs").and_then(Value::as_i64).unwrap_or(0) > 0 {
                        tracing::info!("search_index.json ,?STRM ");
                    }
                }
            }
        }
        store
    }

    pub fn count(&self) -> usize {
        self.inner.read().docs.len()
    }

    pub fn stats(&self) -> Value {
        let inner = self.inner.read();
        json!({
            "numberOfDocuments": inner.docs.len(),
            "isIndexing": self.building.load(Ordering::SeqCst),
            "builtAt": inner.built_at,
            "backend": "embedded",
        })
    }

    pub fn docs_snapshot(&self) -> Vec<SearchDoc> {
        self.inner.read().docs.clone()
    }

    pub fn set_docs(&self, docs: Vec<SearchDoc>) {
        let mut inner = Inner::build(docs);
        inner.built_at = chrono::Utc::now().timestamp();
        *self.inner.write() = inner;
        self.save();
    }

    pub fn upsert(&self, docs: Vec<SearchDoc>) {
        if docs.is_empty() {
            return;
        }
        let mut inner = self.inner.write();
        for doc in docs {
            if let Some(&idx) = inner.by_id.get(&doc.id) {
                inner.docs[idx] = doc;
            } else {
                let idx = inner.docs.len();
                inner.by_id.insert(doc.id.clone(), idx);
                inner.docs.push(doc);
            }
        }
        let docs = inner.docs.clone();
        *inner = Inner::build(docs);
        drop(inner);
        self.save();
    }

    pub fn delete(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let set: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut inner = self.inner.write();
        let docs: Vec<SearchDoc> = inner
            .docs
            .iter()
            .filter(|d| !set.contains(d.id.as_str()))
            .cloned()
            .collect();
        *inner = Inner::build(docs);
        drop(inner);
        self.save();
    }

    pub fn clear(&self) {
        *self.inner.write() = Inner::empty();
        self.save();
    }

    fn save(&self) {
        let payload = {
            let inner = self.inner.read();
            PersistedIndex {
                version: 1,
                built_at: inner.built_at,
                docs: inner.docs.clone(),
            }
        };
        let Ok(raw) = serde_json::to_vec(&payload) else { return };
        if let Err(e) = atomic_write(&self.path, &raw, None) {
            tracing::warn!(":{}", e);
        }
    }

    pub fn search(&self, payload: &Value) -> Value {
        let inner = self.inner.read();
        let q = payload.get("q").and_then(Value::as_str).unwrap_or("").trim();
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
        let crop_len = payload
            .get("cropLength")
            .and_then(Value::as_i64)
            .unwrap_or(200)
            .clamp(20, 2000) as usize;
        let show_score = payload
            .get("showRankingScore")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let q_tokens = tokenize(&q);
        let q_lower = nfkc(&q).to_lowercase();
        let mut candidates: Vec<(usize, f64)> = Vec::new();
        let mut matched_count = 0usize;

        if q_tokens.is_empty() {
            for (i, d) in inner.docs.iter().enumerate() {
                if filter.is_empty() || eval_filter(d, filter) {
                    matched_count += 1;
                    candidates.push((i, 0.0));
                }
            }
        } else {
            let mut scores: HashMap<usize, f64> = HashMap::new();
            let n = inner.docs.len().max(1) as f64;
            for token in &q_tokens {
                let ids = inner.postings.get(token);
                let df = ids.map(|v| v.len()).unwrap_or(0) as f64;
                let idf = (1.0 + n / (df + 1.0)).ln();
                if let Some(ids) = ids {
                    for id in ids {
                        *scores.entry(*id as usize).or_insert(0.0) += idf;
                    }
                }
            }
            // fuzzy candidates for latin tokens
            for token in &q_tokens {
                if token.chars().any(|c| c.is_ascii_alphabetic()) {
                    let max = if token.chars().count() >= 8 { 2 } else if token.chars().count() >= 4 { 1 } else { 0 };
                    if max > 0 {
                        for (vocab, ids) in &inner.postings {
                            if vocab == token || !vocab.chars().any(|c| c.is_ascii_alphabetic()) {
                                continue;
                            }
                            if levenshtein(token, vocab) <= max {
                                for id in ids {
                                    *scores.entry(*id as usize).or_insert(0.0) += 0.35;
                                }
                            }
                        }
                    }
                }
            }
            for (i, d) in inner.docs.iter().enumerate() {
                if !filter.is_empty() && !eval_filter(d, filter) {
                    continue;
                }
                if let Some(mut score) = scores.get(&i).copied() {
                    let title_l = nfkc(&d.title).to_lowercase();
                    if !q_lower.is_empty() && title_l.contains(&q_lower) {
                        score += 8.0;
                    }
                    if !d.code.is_empty() && q_lower.contains(&d.code.to_lowercase()) {
                        score += 6.0;
                    }
                    matched_count += 1;
                    candidates.push((i, score));
                }
            }
        }

        if !sort.is_empty() {
            sort_candidates(&inner.docs, &mut candidates, sort);
        } else {
            candidates.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| inner.docs[b.0].mtime.cmp(&inner.docs[a.0].mtime))
            });
        }

        let total = candidates.len();
        let max_score = candidates
            .iter()
            .map(|(_, s)| *s)
            .fold(0.0f64, f64::max)
            .max(0.0001);
        let page: Vec<(usize, f64)> = candidates.into_iter().skip(offset).take(limit).collect();
        let mut hits = Vec::with_capacity(page.len());
        for (idx, score) in page {
            let d = &inner.docs[idx];
            let mut obj = doc_to_json(d)
                .as_object()
                .cloned()
                .unwrap_or_default();
            if !highlight.is_empty() || !crop.is_empty() {
                let mut formatted = Map::new();
                for field in &highlight {
                    if let Some(text) = doc_field_text(d, field) {
                        formatted.insert(field.clone(), Value::String(highlight_text(&text, &q, &q_tokens)));
                    }
                }
                for field in &crop {
                    if let Some(text) = doc_field_text(d, field) {
                        formatted.insert(field.clone(), Value::String(crop_text(&text, &q, crop_len)));
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
                // _formatted/_rankingScore ?Meili 
                if let Some(v) = obj.get("_formatted") {
                    filtered.insert("_formatted".into(), v.clone());
                }
                if let Some(v) = obj.get("_rankingScore") {
                    filtered.insert("_rankingScore".into(), v.clone());
                }
                hits.push(Value::Object(filtered));
            } else {
                hits.push(Value::Object(obj));
            }
        }

        let mut facets_obj = Map::new();
        if !facet_fields.is_empty() {
            let mut counts: HashMap<String, HashMap<String, i64>> = HashMap::new();
            for field in &facet_fields {
                counts.entry(field.clone()).or_default();
            }
            for (i, d) in inner.docs.iter().enumerate() {
                if !filter.is_empty() && !eval_filter(d, filter) {
                    continue;
                }
                if !q_tokens.is_empty() && !candidates_contains(&inner, i, &q_tokens) {
                    //  facet
                    continue;
                }
                for field in &facet_fields {
                    for value in doc_facet_values(d, field) {
                        *counts
                            .get_mut(field)
                            .unwrap()
                            .entry(value)
                            .or_insert(0) += 1;
                    }
                }
            }
            for (field, values) in counts {
                let mut sorted: Vec<(String, i64)> = values.into_iter().collect();
                sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                let mut m = Map::new();
                for (k, v) in sorted {
                    m.insert(k, json!(v));
                }
                facets_obj.insert(field, Value::Object(m));
            }
        }

        let mut result = Map::new();
        result.insert("hits".into(), Value::Array(hits));
        result.insert("query".into(), Value::String(q.to_string()));
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

fn candidates_contains(inner: &Inner, idx: usize, tokens: &[String]) -> bool {
    tokens.iter().any(|t| {
        inner
            .postings
            .get(t)
            .map(|ids| ids.contains(&(idx as u32)))
            .unwrap_or(false)
    })
}

fn doc_search_text(d: &SearchDoc) -> String {
    let mut s = String::new();
    for field in [
        &d.title,
        &d.stem,
        &d.code,
        &d.nfo_title,
        &d.plot,
        &d.tagline,
        &d.studio,
        &d.path,
        &d.filename,
    ] {
        s.push_str(field);
        s.push(' ');
    }
    for list in [&d.aliases, &d.actors, &d.directors, &d.genres] {
        for item in list {
            s.push_str(item);
            s.push(' ');
        }
    }
    s.push_str(&pinyin_key(&s));
    s
}

pub fn tokenize(text: &str) -> Vec<String> {
    let s = nfkc(text).to_lowercase();
    let mut out = Vec::new();
    let mut latin = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_alphanumeric() {
            latin.push(c);
            i += 1;
            continue;
        }
        if !latin.is_empty() {
            out.push(std::mem::take(&mut latin));
        }
        if is_cjk(c) {
            out.push(c.to_string());
            if i + 1 < chars.len() && is_cjk(chars[i + 1]) {
                out.push(format!("{}{}", c, chars[i + 1]));
            }
        }
        i += 1;
    }
    if !latin.is_empty() {
        out.push(latin);
    }
    out
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

pub fn pinyin_key(text: &str) -> String {
    let mut full = String::new();
    let mut initials = String::new();
    for p in text.to_pinyin() {
        if let Some(p) = p {
            full.push_str(p.plain());
            initials.push_str(p.first_letter());
        }
    }
    if full.is_empty() {
        String::new()
    } else {
        format!("{full} {initials}")
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn doc_to_json(d: &SearchDoc) -> Value {
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
}

fn doc_field_text(d: &SearchDoc, field: &str) -> Option<String> {
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

pub(crate) fn highlight_text(text: &str, q: &str, tokens: &[String]) -> String {
    let escaped = html_escape(text);
    let q = q.trim();
    if !q.is_empty() {
        let lower = text.to_lowercase();
        if let Some(pos) = lower.find(&q.to_lowercase()) {
            let end = pos + q.len();
            if text.is_char_boundary(pos) && text.is_char_boundary(end) {
                return format!(
                    "{}<em>{}</em>{}",
                    html_escape(&text[..pos]),
                    html_escape(&text[pos..end]),
                    html_escape(&text[end..])
                );
            }
        }
    }
    let mut out = escaped;
    for token in tokens {
        if token.chars().all(|c| c.is_ascii_alphanumeric()) && token.len() >= 2 {
            let re = regex::RegexBuilder::new(&regex::escape(token))
                .case_insensitive(true)
                .build();
            if let Ok(re) = re {
                out = re
                    .replace_all(&out, |caps: &regex::Captures| format!("<em>{}</em>", &caps[0]))
                    .to_string();
            }
        }
    }
    out
}

fn crop_text(text: &str, q: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let lower = text.to_lowercase();
    let ql = q.to_lowercase();
    if let Some(pos) = lower.find(&ql) {
        let start = pos.saturating_sub(max / 3);
        let start = text
            .char_indices()
            .map(|(i, _)| i)
            .find(|i| *i >= start)
            .unwrap_or(0);
        let end = text
            .char_indices()
            .map(|(i, _)| i)
            .find(|i| *i >= start + max)
            .unwrap_or(text.len());
        format!("...{}...", &text[start..end])
    } else {
        let end = text
            .char_indices()
            .map(|(i, _)| i)
            .find(|i| *i >= max)
            .unwrap_or(text.len());
        format!("{}...", &text[..end])
    }
}

fn sort_candidates(docs: &[SearchDoc], candidates: &mut [(usize, f64)], sort: &str) {
    let mut parts = sort.split(':');
    let field = parts.next().unwrap_or("").trim();
    let asc = parts.next().map(|s| s.eq_ignore_ascii_case("asc")).unwrap_or(false);
    candidates.sort_by(|a, b| {
        let da = &docs[a.0];
        let db = &docs[b.0];
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

fn doc_facet_values(d: &SearchDoc, field: &str) -> Vec<String> {
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

pub fn eval_filter(d: &SearchDoc, filter: &str) -> bool {
    let f = filter.trim();
    if f.is_empty() {
        return true;
    }
    // OR ?AND
    let or_parts = split_top(f, " OR ");
    if or_parts.len() > 1 {
        return or_parts.iter().any(|p| eval_filter(d, p));
    }
    let and_parts = split_top(f, " AND ");
    if and_parts.len() > 1 {
        return and_parts.iter().all(|p| eval_filter(d, p));
    }
    let f = f.trim().trim_start_matches('(').trim_end_matches(')').trim();
    let re = regex::Regex::new(
        r#"(?i)^([a-zA-Z_][a-zA-Z0-9_]*)\s*(=|!=|>=|<=|>|<|IN|NOT IN|EXISTS)\s*(.*)$"#,
    )
    .unwrap();
    let Some(caps) = re.captures(f) else {
        return true;
    };
    let field = caps[1].to_lowercase();
    let op = caps[2].to_uppercase();
    let rhs = caps[3].trim();
    let values = field_values(d, &field);
    let rhs_clean = rhs.trim().trim_matches('"').trim_matches('\'').to_string();
    match op.as_str() {
        "EXISTS" => !values.is_empty(),
        "IN" | "NOT IN" => {
            let list = rhs
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_lowercase())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            let hit = values
                .iter()
                .any(|v| list.contains(&v.to_lowercase()));
            if op == "IN" { hit } else { !hit }
        }
        _ => values.iter().any(|v| {
            let vl = v.to_lowercase();
            let rl = rhs_clean.to_lowercase();
            match op.as_str() {
                "=" => vl == rl,
                "!=" => vl != rl,
                ">" | ">=" | "<" | "<=" => {
                    let (Ok(a), Ok(b)) = (vl.parse::<f64>(), rl.parse::<f64>()) else {
                        return false;
                    };
                    match op.as_str() {
                        ">" => a > b,
                        ">=" => a >= b,
                        "<" => a < b,
                        "<=" => a <= b,
                        _ => false,
                    }
                }
                _ => false,
            }
        }),
    }
}

fn field_values(d: &SearchDoc, field: &str) -> Vec<String> {
    match field {
        "type" => vec![d.r#type.clone()],
        "netdisk" => vec![d.netdisk.clone()],
        "ext" => vec![d.ext.clone()],
        "code" => vec![d.code.clone()],
        "title" => vec![d.title.clone()],
        "path" => vec![d.path.clone()],
        "year" => d.year.map(|v| v.to_string()).into_iter().collect(),
        "season" => d.season.map(|v| v.to_string()).into_iter().collect(),
        "episode" => d.episode.map(|v| v.to_string()).into_iter().collect(),
        "has_nfo" => vec![d.has_nfo.to_string()],
        "genres" => d.genres.clone(),
        "actors" => d.actors.clone(),
        _ => Vec::new(),
    }
}

fn split_top<'a>(s: &'a str, sep: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let bytes = s.as_bytes();
    let sep_b = sep.as_bytes();
    let mut in_quote = false;
    let mut i = 0usize;
    while i + sep_b.len() <= bytes.len() {
        let c = bytes[i] as char;
        if c == '"' || c == '\'' {
            in_quote = !in_quote;
        }
        if !in_quote && bytes[i..].starts_with(sep_b) {
            out.push(s[start..i].trim());
            start = i + sep_b.len();
            i = start;
            continue;
        }
        i += 1;
    }
    out.push(s[start..].trim());
    out
}

pub fn doc_id(rel_path: &str) -> String {
    sha1_hex(rel_path).chars().take(24).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SearchDoc {
        SearchDoc {
            id: "1".into(),
            title: "Test Movie".into(),
            stem: "Test.Movie.2024".into(),
            code: "ABC-123".into(),
            year: Some(2024),
            r#type: "movie".into(),
            netdisk: "Zpool".into(),
            path: "Zpool/movies/Test.Movie.2024.mkv".into(),
            filename: "Test.Movie.2024.mkv".into(),
            size: 100,
            mtime: 10,
            ..Default::default()
        }
    }

    #[test]
    fn embedded_search_finds_title_and_filter() {
        let dir = std::env::temp_dir().join(format!("ark-push-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let store = SearchStore::new(dir.to_str().unwrap());
        store.set_docs(vec![sample()]);
        let r = store.search(&json!({"q":"test","limit":10}));
        assert_eq!(r["estimatedTotalHits"], 1);
        assert_eq!(r["hits"][0]["title"], "Test Movie");
        let r2 = store.search(&json!({"q":"test","filter":"type = \"episode\""}));
        assert_eq!(r2["estimatedTotalHits"], 0);
        let r3 = store.search(&json!({"q":"abc 123","limit":10}));
        assert_eq!(r3["estimatedTotalHits"], 1);
    }

    #[test]
    fn keyword_filter_rejects_unrelated_folders() {
        assert!(match_keyword("波多野结衣", "波多野结衣"));
        assert!(match_keyword("波多野结衣", "Zpool/movies/步兵/mov-2/波多野结衣"));
        assert!(match_keyword("波多野结衣", "波多野结衣无码合集.torrent"));
        assert!(!match_keyword("波多野结衣", "かすみゆら"));
        assert!(!match_keyword("波多野结衣", "Zpool/movies/步兵/mov-3/かすみゆら"));
    }

    #[test]
    fn tokenizes_cjk_and_highlight() {
        let toks = tokenize("三浦なお");
        assert!(toks.contains(&"三".to_string()));
        let h = highlight_text("三浦なお", "三浦", &toks);
        assert!(h.contains("<em>"));
    }
}

pub fn match_keyword(keyword: &str, text: &str) -> bool {
    let kw = keyword.trim();
    if kw.is_empty() {
        return false;
    }
    let hay = text.trim();
    if hay.to_lowercase().contains(&kw.to_lowercase()) {
        return true;
    }
    let q = nfkc(kw)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || (*c as u32) >= 0x4e00)
        .collect::<String>();
    let h = nfkc(hay)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || (*c as u32) >= 0x4e00)
        .collect::<String>();
    if !q.is_empty() && h.contains(&q) {
        return true;
    }
    if kw.chars().any(|c| (c as u32) >= 0x4e00) {
        let qp = pinyin_key(kw);
        let hp = pinyin_key(hay);
        if !qp.is_empty() && hp.contains(&qp) {
            return true;
        }
    }
    false
}

pub fn normalize_key(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| {
            p.trim()
                .to_uppercase()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || is_cjk(*c))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("|")
}
