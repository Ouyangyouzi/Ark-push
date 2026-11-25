use crate::litepan::LitePanClient;
use crate::search::SearchDoc;
use crate::search_fts::SearchStore;
use crate::search::doc_id;
use crate::state::StateStore;
use crate::util::{json_bool, json_i64, json_str, nfkc, stem};
use fancy_regex::Regex as FRegex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "avi", "wmv", "mov", "m4v", "ts", "flv", "rmvb", "webm", "m2ts", "iso",
];

pub fn is_video(name: &str) -> bool {
    VIDEO_EXTS.contains(&crate::util::ext_lower(name).as_str())
}

fn env_plot_max() -> usize {
    std::env::var("PLOT_MAX_CHARS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600)
}

pub fn clean_title(stem: &str) -> String {
    static BRACKET: std::sync::OnceLock<FRegex> = std::sync::OnceLock::new();
    static SEP: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let bracket = BRACKET.get_or_init(|| FRegex::new(r"[\[\(\{【（{][^\]\)\}】）}]*[\]\)\}】）}]").unwrap());
    let sep = SEP.get_or_init(|| regex::Regex::new(r"[._]+|-{2,}").unwrap());
    let mut text = bracket.replace_all(stem, " ").to_string();
    text = sep.replace_all(&text, " ").to_string();
    text = text.replace('_', " ");
    let res_tags: HashSet<&str> = [
        "4k", "2160p", "1080p", "1080i", "720p", "576p", "480p", "hdr", "hdr10",
        "dolbyvision", "dv", "sdr", "x264", "x265", "h264", "h265", "hevc", "avc", "av1",
        "aac", "ac3", "dts", "truehd", "atmos", "flac", "bluray", "bdrip", "webrip",
        "web-dl", "webdl", "hdtv", "remux", "dvdrip", "10bit", "8bit", "60fps", "hq",
        "repack", "proper",
    ]
    .into_iter()
    .collect();
    let noise: HashSet<&str> = [
        "中字", "简中", "繁中", "双语", "无码", "有码", "破解", "流出", "无码破解",
        "中文字幕", "字幕", "高清", "超清", "完结", "全集", "合集",
    ]
    .into_iter()
    .collect();
    let year_re = regex::Regex::new(r"^(19\d{2}|20\d{2})$").unwrap();
    let mut tokens = Vec::new();
    for raw in text.split_whitespace() {
        let token = raw.trim_matches(|c: char| matches!(c, ' ' | '-' | '–' | '—'));
        let low = token.to_lowercase();
        if low.is_empty() || token.contains('@') {
            continue;
        }
        if res_tags.contains(low.as_str()) || noise.contains(low.as_str()) {
            continue;
        }
        let head = low.split('-').next().unwrap_or("");
        if low.contains('-') && res_tags.contains(head) {
            continue;
        }
        if token.len() == 4 && year_re.is_match(token) {
            continue;
        }
        tokens.push(token.to_string());
    }
    let cleaned = tokens.join(" ");
    if cleaned.is_empty() { stem.to_string() } else { cleaned }
}

pub fn detect_year(text: &str) -> Option<i64> {
    static RE: std::sync::OnceLock<FRegex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| FRegex::new(r"(?<!\d)(19\d{2}|20\d{2})(?!\d)").unwrap());
    let mut out = None;
    let mut start = 0;
    while let Ok(Some(c)) = re.captures_from_pos(text, start) {
        if let Some(m) = c.get(1) {
            if let Ok(y) = m.as_str().parse::<i64>() {
                out = Some(out.map_or(y, |old: i64| old.max(y)));
            }
        }
        start = c.get(0).map(|m| m.end()).unwrap_or(text.len()).max(start + 1);
        if start >= text.len() {
            break;
        }
    }
    out
}

pub fn detect_episode(text: &str) -> (Option<i64>, Option<i64>) {
    static SE: std::sync::OnceLock<FRegex> = std::sync::OnceLock::new();
    static EP: std::sync::OnceLock<FRegex> = std::sync::OnceLock::new();
    let se = SE.get_or_init(|| {
        FRegex::new(r"(?i)(?<![a-z0-9])s(\d{1,2})[\s._-]*e(\d{1,3})(?!\d)").unwrap()
    });
    if let Ok(Some(c)) = se.captures(text) {
        let s = c.get(1).and_then(|m| m.as_str().parse().ok());
        let e = c.get(2).and_then(|m| m.as_str().parse().ok());
        return (s, e);
    }
    let ep = EP.get_or_init(|| {
        FRegex::new(r"(?i)(?<![a-z0-9])(?:ep|e|第)\s*0*(\d{1,3})\s*(?:集|话|話)?(?!\d)").unwrap()
    });
    if let Ok(Some(c)) = ep.captures(text) {
        return (None, c.get(1).and_then(|m| m.as_str().parse().ok()));
    }
    (None, None)
}

pub fn detect_code(text: &str) -> Option<String> {
    static RE: std::sync::OnceLock<FRegex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        FRegex::new(r"(?<![A-Z0-9])([A-Z]{2,6})[-_\s](\d{2,7})(?!\d)").unwrap()
    });
    let up = text.to_uppercase();
    if let Ok(Some(c)) = re.captures(&up) {
        let a = c.get(1)?.as_str();
        let b = c.get(2)?.as_str();
        return Some(format!("{a}-{b}"));
    }
    None
}

fn first_text(xml: &str, tag: &str) -> String {
    let re = regex::RegexBuilder::new(&format!(r"(?is)<{tag}[^>]*>(.*?)</{tag}>"))
        .build()
        .ok();
    re.and_then(|r| r.captures(xml))
        .and_then(|c| c.get(1).map(|m| strip_tags(m.as_str()).trim().to_string()))
        .unwrap_or_default()
}

fn strip_tags(s: &str) -> String {
    regex::Regex::new(r"(?is)<[^>]+>")
        .map(|r| r.replace_all(s, "").to_string())
        .unwrap_or_else(|_| s.to_string())
}

fn all_text(xml: &str, tag: &str) -> Vec<String> {
    let Ok(re) = regex::RegexBuilder::new(&format!(r"(?is)<{tag}[^>]*>(.*?)</{tag}>")).build() else {
        return Vec::new();
    };
    re.captures_iter(xml)
        .filter_map(|c| c.get(1).map(|m| strip_tags(m.as_str()).trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_nfo(path: &Path) -> Map<String, Value> {
    let mut out = Map::new();
    let Ok(xml) = fs::read_to_string(path) else { return out };
    let actors: Vec<String> = regex::Regex::new(r"(?is)<actor[^>]*>(.*?)</actor>")
        .ok()
        .map(|re| {
            re.captures_iter(&xml)
                .filter_map(|c| c.get(1))
                .map(|m| first_text(m.as_str(), "name"))
                .filter(|s| !s.is_empty())
                .take(20)
                .collect()
        })
        .unwrap_or_default();
    let tmdb = regex::Regex::new(r#"(?is)<uniqueid[^>]*type=["']tmdb["'][^>]*>(.*?)</uniqueid>"#)
        .ok()
        .and_then(|re| re.captures(&xml))
        .and_then(|c| c.get(1).map(|m| m.as_str().trim().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| first_text(&xml, "tmdbid"));
    out.insert("nfo_title".into(), json!(first_text(&xml, "title")));
    out.insert("original_title".into(), json!(first_text(&xml, "originaltitle")));
    let plot = {
        let a = first_text(&xml, "plot");
        if a.is_empty() { first_text(&xml, "outline") } else { a }
    };
    out.insert("plot".into(), json!(plot));
    out.insert("tagline".into(), json!(first_text(&xml, "tagline")));
    out.insert("year".into(), json!(first_text(&xml, "year")));
    out.insert("premiered".into(), json!(first_text(&xml, "premiered")));
    out.insert("rating".into(), json!(first_text(&xml, "rating")));
    out.insert("genres".into(), json!(all_text(&xml, "genre").into_iter().take(8).collect::<Vec<_>>()));
    out.insert("actors".into(), json!(actors));
    out.insert("directors".into(), json!(all_text(&xml, "director").into_iter().take(5).collect::<Vec<_>>()));
    out.insert("studio".into(), json!(first_text(&xml, "studio")));
    out.insert("tmdb_id".into(), json!(tmdb));
    out.insert("nfo_season".into(), json!(first_text(&xml, "season")));
    out.insert("nfo_episode".into(), json!(first_text(&xml, "episode")));
    out
}

fn poster_in_dir(dir: &Path) -> String {
    for name in ["poster.jpg", "poster.png", "folder.jpg", "cover.jpg", "fanart.jpg", "thumb.jpg"] {
        if dir.join(name).is_file() {
            return name.to_string();
        }
    }
    String::new()
}

pub fn build_strm_doc(abs: &Path, root: &Path) -> Option<SearchDoc> {
    let rel = abs.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
    let filename = abs.file_name()?.to_string_lossy().to_string();
    let stemv = stem(&filename);
    let dir = abs.parent()?;
    let mut nfo = Map::new();
    for name in [format!("{stemv}.nfo"), format!("{}.nfo", dir.file_name()?.to_string_lossy())] {
        let p = dir.join(&name);
        if p.is_file() {
            nfo = parse_nfo(&p);
            break;
        }
    }
    let (season0, episode0) = detect_episode(stemv);
    let mut year0 = detect_year(stemv);
    if year0.is_none() {
        year0 = json_str(&Value::Object(nfo.clone()), "year").parse().ok();
    }
    if year0.is_none() {
        let premiered = json_str(&Value::Object(nfo.clone()), "premiered");
        if premiered.len() >= 4 {
            year0 = premiered[..4].parse().ok();
        }
    }
    let season = json_str(&Value::Object(nfo.clone()), "nfo_season").parse().ok().or(season0);
    let episode = json_str(&Value::Object(nfo.clone()), "nfo_episode").parse().ok().or(episode0);
    let code = detect_code(stemv).unwrap_or_default();
    let netdisk = rel.split('/').next().unwrap_or("").to_string();
    let media_type = if episode.is_some() || season.is_some() {
        "episode"
    } else if year0.is_some() || rel.contains("电影") || rel.to_lowercase().contains("movie") {
        "movie"
    } else {
        "other"
    };
    let mut aliases = Vec::new();
    for v in [
        json_str(&Value::Object(nfo.clone()), "nfo_title"),
        json_str(&Value::Object(nfo.clone()), "original_title"),
        code.clone(),
    ] {
        if !v.is_empty() && !aliases.contains(&v) {
            aliases.push(v);
        }
    }
    let title = clean_title(stemv);
    aliases.extend(crate::search::pinyin_key(&title).split_whitespace().map(str::to_string));
    aliases.extend(crate::search::pinyin_key(&rel).split_whitespace().map(str::to_string));
    aliases.truncate(10);
    let plot_max = env_plot_max();
    let mut plot = json_str(&Value::Object(nfo.clone()), "plot");
    plot.truncate(plot.char_indices().nth(plot_max).map(|(i, _)| i).unwrap_or(plot.len()));
    let meta = fs::metadata(abs).ok();
    let mut doc = SearchDoc::default();
    doc.id = doc_id(&rel);
    doc.title = title;
    doc.stem = stemv.to_string();
    doc.code = code;
    doc.aliases = aliases;
    doc.nfo_title = json_str(&Value::Object(nfo.clone()), "nfo_title");
    doc.plot = plot;
    doc.tagline = json_str(&Value::Object(nfo.clone()), "tagline");
    doc.actors = nfo
        .get("actors")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).take(12).collect())
        .unwrap_or_default();
    doc.directors = nfo
        .get("directors")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    doc.genres = nfo
        .get("genres")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    doc.studio = json_str(&Value::Object(nfo.clone()), "studio");
    doc.rating = json_str(&Value::Object(nfo.clone()), "rating").parse::<f64>().ok();
    doc.tmdb_id = json_str(&Value::Object(nfo.clone()), "tmdb_id");
    doc.year = year0;
    doc.r#type = media_type.to_string();
    doc.season = season;
    doc.episode = episode;
    doc.netdisk = netdisk;
    doc.dir = rel.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
    doc.path = rel;
    doc.filename = filename;
    doc.ext = crate::util::ext_lower(&doc.filename);
    doc.size = meta.as_ref().map(|m| m.len() as i64).unwrap_or(0);
    doc.mtime = meta
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    doc.has_nfo = !nfo.is_empty();
    doc.poster = poster_in_dir(dir);
    doc.source = Some("strm".into());
    Some(doc)
}

pub fn build_netdisk_doc(account_id: i64, dir_path: &str, item: &Value) -> SearchDoc {
    let filename = json_str(item, "name");
    let stemv = stem(&filename);
    let title = clean_title(stemv);
    let (season, episode) = detect_episode(stemv);
    let year = detect_year(stemv);
    let code = detect_code(stemv).unwrap_or_default();
    let rel = format!("{}/{}", dir_path.trim_matches('/'), filename);
    let media_type = if episode.is_some() || season.is_some() {
        "episode"
    } else if year.is_some() || !code.is_empty() {
        "movie"
    } else {
        "other"
    };
    let mut aliases = Vec::new();
    if !code.is_empty() {
        aliases.push(code.clone());
    }
    aliases.extend(crate::search::pinyin_key(&title).split_whitespace().map(str::to_string));
    aliases.extend(crate::search::pinyin_key(dir_path).split_whitespace().map(str::to_string));
    aliases.truncate(10);
    let mut doc = SearchDoc::default();
    doc.id = doc_id(&format!("nd:{account_id}:{rel}"));
    doc.title = title;
    doc.stem = stemv.to_string();
    doc.code = code;
    doc.aliases = aliases;
    doc.year = year;
    doc.r#type = media_type.to_string();
    doc.season = season;
    doc.episode = episode;
    doc.netdisk = dir_path.trim_matches('/').split('/').next().unwrap_or("").to_string();
    doc.dir = dir_path.trim_matches('/').to_string();
    doc.path = rel;
    doc.filename = filename;
    doc.ext = crate::util::ext_lower(&doc.filename);
    doc.size = json_i64(item, "size", 0);
    doc.has_nfo = false;
    doc.source = Some("netdisk".into());
    doc
}

pub fn netdisk_max_dirs() -> usize {
    std::env::var("NETDISK_MAX_DIRS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(20_000)
        .clamp(100, 100_000)
}

pub async fn run_netdisk_sync(
    client: &LitePanClient,
    roots: &[Value],
    search: &SearchStore,
    state: &StateStore,
    max_dirs: usize,
) -> Value {
    let started = std::time::Instant::now();
    let mut docs = Vec::new();
    let mut scanned = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for root in roots {
        let account_id = root.get("account_id").and_then(Value::as_i64).unwrap_or(0);
        let account_id = if account_id == 0 {
            client.pick_account(None).await.ok().and_then(|a| a.get("id").and_then(Value::as_i64)).unwrap_or(0)
        } else {
            account_id
        };
        let path = root.get("path").and_then(Value::as_str).unwrap_or("").trim_matches('/').to_string();
        if path.is_empty() {
            continue;
        }
        let resolved = match client.resolve_dir(account_id, &path, false).await {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("{path}: {e}"));
                continue;
            }
        };
        let mut queue: Vec<(String, String, usize)> = vec![(
            resolved.get("parent_id").and_then(Value::as_str).unwrap_or("").to_string(),
            format!("/{}", path.trim_matches('/')),
            0,
        )];
        let mut head = 0;
        while head < queue.len() && scanned < max_dirs {
            let (pid, cur, depth) = queue[head].clone();
            head += 1;
            scanned += 1;
            let listed = match client.list_dir(account_id, &pid, "", None).await {
                Ok(v) => v,
                Err(e) => {
                    errors.push(format!("{cur}: {e}"));
                    continue;
                }
            };
            let items = listed.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            for it in items {
                let name = json_str(&it, "name");
                if name.is_empty() {
                    continue;
                }
                if json_bool(&it, "is_dir", false) {
                    if depth + 1 < 8 {
                        let id = json_str(&it, "id");
                        if !id.is_empty() {
                            queue.push((id, format!("{}/{}", cur.trim_end_matches('/'), name), depth + 1));
                        }
                    }
                } else if is_video(&name) {
                    docs.push(build_netdisk_doc(account_id, &cur, &it));
                }
            }
        }
    }
    search.set_docs(docs.clone());
    let report = json!({
        "mode": "netdisk_full",
        "scanned": scanned,
        "docs_total": docs.len(),
        "duration_ms": started.elapsed().as_millis() as i64,
        "errors": errors,
    });
    let files = Map::new();
    state.record_sync(files, report.clone());
    report
}

pub fn has_fileindex(state_dir: &str) -> bool {
    std::fs::read_dir(std::path::Path::new(state_dir).join("fileindex"))
        .map(|mut d| d.any(|e| e.map(|x| x.path().extension().and_then(|s| s.to_str()) == Some("json")).unwrap_or(false)))
        .unwrap_or(false)
}

pub fn sync_from_fileindex(state_dir: &str, search: &SearchStore) -> Value {
    let dir = std::path::Path::new(state_dir).join("fileindex");
    let started = std::time::Instant::now();
    let mut docs = Vec::new();
    let mut accounts = 0usize;
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return json!({"mode":"fileindex","docs_total":search.count(),"note":"fileindex directory not found"});
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(account_id) = name.strip_suffix(".json").and_then(|s| s.parse::<i64>().ok()) else {
            continue;
        };
        let Some(v) = crate::util::read_json(&entry.path()) else { continue };
        let Some(items) = v.get("items").and_then(Value::as_array) else { continue };
        accounts += 1;
        for it in items {
            if json_bool(it, "is_dir", false) {
                continue;
            }
            let filename = json_str(it, "name");
            if filename.is_empty() || !is_video(&filename) {
                continue;
            }
            let path = json_str(it, "path").trim_matches('/').to_string();
            let dir_path = path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
            docs.push(build_netdisk_doc(account_id, &dir_path, it));
        }
    }
    let total = docs.len();
    search.set_docs(docs);
    let report = json!({
        "mode": "fileindex",
        "accounts": accounts,
        "docs_total": total,
        "duration_ms": started.elapsed().as_millis() as i64,
    });
    tracing::info!("FTS5 rebuilt from fileindex: {} docs", total);
    report
}

pub async fn run_strm_sync(
    root: &str,
    search: &SearchStore,
    state: &StateStore,
    full: bool,
) -> Value {
    let started = std::time::Instant::now();
    let root_path = PathBuf::from(root);
    if !root_path.is_dir() {
        return json!({"mode":"incremental","scanned":0,"docs_total":search.count(),"errors":[format!("strm root not found: {root}")]});
    }
    let skip_names: HashSet<&str> = [".git", "@eaDir", "#recycle", "$RECYCLE.BIN", "node_modules"].into_iter().collect();
    let mut docs = Vec::new();
    let mut files = Map::new();
    let mut stack = vec![root_path.clone()];
    let mut errors = Vec::new();
    while let Some(cur) = stack.pop() {
        let entries = match fs::read_dir(&cur) {
            Ok(e) => e,
            Err(e) => {
                errors.push(format!("{}: {e}", cur.display()));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if skip_names.contains(name.as_str()) {
                    continue;
                }
                stack.push(path);
            } else if name.to_lowercase().ends_with(".strm") {
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let rel = path.strip_prefix(&root_path).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                files.insert(rel.clone(), json!([mtime, meta.len() as i64]));
                match build_strm_doc(&path, &root_path) {
                    Some(d) => docs.push(d),
                    None => errors.push(format!("{rel}: parse failed")),
                }
            }
        }
    }
    let report = json!({
        "mode": if full { "full" } else { "incremental" },
        "scanned": files.len(),
        "added": docs.len(),
        "changed": 0,
        "removed": 0,
        "docs_total": docs.len(),
        "errors": errors,
        "duration_ms": started.elapsed().as_millis() as i64,
    });
    search.set_docs(docs);
    state.record_sync(files, report.clone());
    report
}

pub fn infer_roots_from_settings(state: &StateStore) -> Vec<Value> {
    let raw = state.get_setting("netdisk_roots", json!([]));
    raw.as_array().cloned().unwrap_or_default()
}

pub async fn refresh_dir_docs(
    client: &LitePanClient,
    account_id: i64,
    path: &str,
    search: &SearchStore,
) -> Value {
    let resolved = match client.resolve_dir(account_id, path, false).await {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    let pid = resolved.get("parent_id").and_then(Value::as_str).unwrap_or("").to_string();
    let items = match client.list_dir(account_id, &pid, "", None).await {
        Ok(v) => v.get("items").and_then(Value::as_array).cloned().unwrap_or_default(),
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    let docs: Vec<SearchDoc> = items
        .iter()
        .filter(|it| !json_bool(it, "is_dir", false) && is_video(&json_str(it, "name")))
        .map(|it| build_netdisk_doc(account_id, path, it))
        .collect();
    let n = docs.len();
    search.upsert(docs);
    json!({"ok": true, "docs": n})
}

pub fn doc_summary(docs: &[SearchDoc]) -> HashMap<String, i64> {
    let mut m = HashMap::new();
    for d in docs {
        *m.entry(d.r#type.clone()).or_insert(0) += 1;
    }
    m
}
