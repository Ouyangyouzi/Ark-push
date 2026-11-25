use crate::indexer::detect_code;
use crate::litepan::LitePanClient;
use crate::util::ext_lower;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct OrganizeOptions {
    pub cleanup: bool,
    pub rename: bool,
    pub keep_code_folder: bool,
    pub min_size: i64,
}

pub fn options_from(v: &Value) -> OrganizeOptions {
    let mut o = OrganizeOptions {
        cleanup: true,
        rename: true,
        keep_code_folder: false,
        // 与旧 Python 版一致：默认丢弃 150MB 以下的广告/预告小视频
        min_size: 150 * 1024 * 1024,
    };
    if let Some(opts) = v.get("organize_opts").and_then(Value::as_object) {
        if let Some(x) = opts.get("cleanup").and_then(Value::as_bool) {
            o.cleanup = x;
        }
        if let Some(x) = opts.get("rename").and_then(Value::as_bool) {
            o.rename = x;
        }
        if let Some(x) = opts.get("keep_code_folder").and_then(Value::as_bool) {
            o.keep_code_folder = x;
        }
        if let Some(x) = opts.get("min_size").and_then(Value::as_i64) {
            // 脚本/界面传的是 MB，这里统一换算成字节，否则 150 会被当成 150 字节，
            // 导致广告小视频、预告片永远不会被判定为垃圾。
            o.min_size = x.max(0).saturating_mul(1024 * 1024);
        }
    }
    o
}

#[derive(Debug, Clone)]
struct Entry {
    id: String,
    name: String,
    parent_id: String,
    is_dir: bool,
    size: i64,
    depth: usize,
}

fn is_video(name: &str) -> bool {
    matches!(
        ext_lower(name).as_str(),
        "mp4" | "mkv" | "avi" | "wmv" | "mov" | "m4v" | "ts" | "flv" | "rmvb" | "webm" | "m2ts"
    )
}

fn is_subtitle(name: &str) -> bool {
    matches!(ext_lower(name).as_str(), "srt" | "ass" | "ssa" | "vtt" | "sub")
}

fn is_image(name: &str) -> bool {
    matches!(ext_lower(name).as_str(), "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif")
}

fn is_junk_ext(name: &str) -> bool {
    matches!(
        ext_lower(name).as_str(),
        "txt" | "url" | "html" | "htm" | "mht" | "mhtml" | "nfo" | "md" | "json" | "xml"
            | "log" | "cue" | "sfv" | "exe" | "msi" | "bat" | "cmd" | "lnk" | "db" | "torrent"
            | "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "ini" | "bak" | "tmp"
            | "diz" | "webloc" | "website" | "apk" | "swf" | "vbs" | "scr" | "hta" | "zip"
            | "rar" | "7z" | "chm"
    )
}

fn has_ad_marker(name: &str) -> bool {
    let low = name.to_lowercase();
    low.contains("最新地址")
        || low.contains("最新网址")
        || low.contains("论坛地址")
        || low.contains("永久地址")
        || low.contains("地址发布")
        || low.contains("扫码")
        || low.contains("赌场")
        || low.contains("秀场")
        || low.contains("1024")
        || low.contains("2048")
        || low.contains("qr")
}

fn classify(name: &str, size: i64, min_size: i64) -> (bool, &'static str) {
    if is_subtitle(name) {
        return (true, "subtitle");
    }
    if has_ad_marker(name) {
        return (false, "ad");
    }
    if is_image(name) {
        let artwork = regex::Regex::new(r"(?i)poster|cover|fanart|folder|thumb|海报|封面|预览")
            .map(|r| r.is_match(name))
            .unwrap_or(false);
        if detect_code(name).is_some() || artwork {
            return (true, "image");
        }
        return (false, "image-no-code");
    }
    if is_junk_ext(name) {
        return (false, "not-video");
    }
    if is_video(name) {
        if min_size > 0 && size > 0 && size < min_size {
            return (false, "too-small");
        }
        return (true, "video");
    }
    (true, "unknown")
}

fn suffixes(name: &str, code: &str) -> Vec<String> {
    let low = name.to_lowercase();
    let mut tags = Vec::new();
    if regex::Regex::new(r"中字|中文|字幕|中文字幕|chs|cht|big5|chinese|subtitle|subbed|(^|[-_.\s])c($|[-_.\s])")
        .map(|r| r.is_match(&low))
        .unwrap_or(false)
    {
        tags.push("C".to_string());
    }
    if regex::Regex::new(r"破解|hack|crack|uncensored|无码|無碼|restored")
        .map(|r| r.is_match(&low))
        .unwrap_or(false)
    {
        tags.push("破解".to_string());
    }
    if regex::Regex::new(r"流出|leak|leaked")
        .map(|r| r.is_match(&low))
        .unwrap_or(false)
    {
        tags.push("流出".to_string());
    }
    if regex::Regex::new(r"(^|[^a-z0-9])vr([^a-z0-9]|$)")
        .map(|r| r.is_match(&low))
        .unwrap_or(false)
    {
        tags.push("VR".to_string());
    }
    if regex::Regex::new(r"(^|[^a-z0-9])8k([^a-z0-9]|$)")
        .map(|r| r.is_match(&low))
        .unwrap_or(false)
    {
        tags.push("8K".to_string());
    } else if regex::Regex::new(r"(^|[^a-z0-9])(4k|2160p|uhd)([^a-z0-9]|$)")
        .map(|r| r.is_match(&low))
        .unwrap_or(false)
    {
        tags.push("4K".to_string());
    } else if regex::Regex::new(r"(^|[^a-z0-9])1080p([^a-z0-9]|$)")
        .map(|r| r.is_match(&low))
        .unwrap_or(false)
    {
        tags.push("1080P".to_string());
    } else if regex::Regex::new(r"(^|[^a-z0-9])720p([^a-z0-9]|$)")
        .map(|r| r.is_match(&low))
        .unwrap_or(false)
    {
        tags.push("720P".to_string());
    }
    if regex::Regex::new(r"(^|[^a-z0-9])60\s*fps([^a-z0-9]|$)")
        .map(|r| r.is_match(&low))
        .unwrap_or(false)
    {
        tags.push("60FPS".to_string());
    }
    if regex::Regex::new(r"(^|[^a-z0-9])120\s*fps([^a-z0-9]|$)")
        .map(|r| r.is_match(&low))
        .unwrap_or(false)
    {
        tags.push("120FPS".to_string());
    }
    if low.contains("ltx") {
        tags.push("LTX".to_string());
    }
    if let Some(c) = regex::Regex::new(r"(?:part|pt|cd|disc|vol|volume|ep)\s*(\d{1,3}|[ab])")
        .ok()
        .and_then(|r| r.captures(&low))
    {
        tags.push(format!("cd{}", &c[1].to_lowercase()));
    }
    // 保持稳定顺序，去重
    let order = ["C", "8K", "4K", "1080P", "720P", "VR", "60FPS", "120FPS", "LTX", "破解", "流出"];
    tags.sort_by_key(|t| order.iter().position(|x| x == t).unwrap_or(if t.starts_with("cd") { 20 } else { 30 }));
    tags.dedup();
    // 如果文件名已经完全等于 code，则不再重复加 code
    if name.to_uppercase().starts_with(&code.to_uppercase()) {
        tags.retain(|t| t != "C" || !name.to_uppercase().starts_with(&format!("{code}-C")));
    }
    tags
}

fn target_name(original: &str, code: &str) -> String {
    let ext = match original.rfind('.') {
        Some(i) if i + 1 < original.len() => &original[i..],
        _ => "",
    };
    let tags = suffixes(original, code);
    if tags.is_empty() {
        format!("{code}{ext}")
    } else {
        format!("{code}-{}{ext}", tags.join("-"))
    }
}

async fn walk(
    client: &LitePanClient,
    account_id: i64,
    parent_id: &str,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<Entry>,
) {
    if depth > max_depth || out.len() >= 300 {
        return;
    }
    let Ok(listed) = client.list_dir(account_id, parent_id, "", None).await else {
        return;
    };
    let items = listed.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut dirs = Vec::new();
    for it in items {
        if out.len() >= 300 {
            break;
        }
        let name = it.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let id = it.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let is_dir = it.get("is_dir").and_then(Value::as_bool).unwrap_or(false);
        out.push(Entry {
            id: id.clone(),
            name,
            parent_id: parent_id.to_string(),
            is_dir,
            size: it.get("size").and_then(Value::as_i64).unwrap_or(0),
            depth,
        });
        if is_dir && !id.is_empty() {
            dirs.push(id);
        }
    }
    for id in dirs {
        Box::pin(walk(client, account_id, &id, depth + 1, max_depth, out)).await;
    }
}

pub struct OrganizeReport {
    pub target_path: String,
    pub moved: usize,
    pub cleaned: usize,
    pub renamed: usize,
    pub removed_folders: usize,
}

pub async fn organize_dir(
    client: &LitePanClient,
    account_id: i64,
    path: &str,
    code: &str,
    opts: &OrganizeOptions,
) -> Result<OrganizeReport, String> {
    let path = path.trim_matches('/').to_string();
    let code = code.trim().to_uppercase();
    if code.is_empty() {
        return Err("整理需要番号".into());
    }
    let root = client
        .resolve_dir(account_id, &path, false)
        .await
        .map_err(|e| e.to_string())?;
    let root_id = root.get("parent_id").and_then(Value::as_str).unwrap_or("").to_string();
    if root_id.is_empty() {
        return Err(format!("目录不存在:{path}"));
    }
    let mut top = Vec::new();
    walk(client, account_id, &root_id, 0, 1, &mut top).await;
    let residue: Vec<Entry> = top
        .iter()
        .filter(|e| e.is_dir && detect_code(&e.name).map(|c| c.to_uppercase()) == Some(code.clone()))
        .cloned()
        .collect();
    let residue_ids: HashSet<String> = residue.iter().map(|e| e.id.clone()).collect();
    let mut entries = Vec::new();
    if residue_ids.is_empty() {
        // 没有残留番号文件夹：只处理目标目录本身
        entries = top;
    } else {
        for d in &residue {
            walk(client, account_id, &d.id, 0, 4, &mut entries).await;
        }
        // 残留番号文件夹本身也要参与最后的“搬空即删”
        entries.extend(residue.iter().cloned());
    }

    let scope_ids: HashSet<String> = if residue_ids.is_empty() {
        HashSet::from([root_id.clone()])
    } else {
        let mut ids = residue_ids.clone();
        for e in &entries {
            if e.is_dir {
                ids.insert(e.id.clone());
            }
        }
        ids
    };

    let mut target_id = root_id.clone();
    let mut target_path = path.clone();
    if opts.keep_code_folder {
        let folder = code.clone();
        let resolved = client
            .resolve_dir(account_id, &format!("{path}/{folder}"), true)
            .await
            .map_err(|e| e.to_string())?;
        target_id = resolved.get("parent_id").and_then(Value::as_str).unwrap_or("").to_string();
        target_path = format!("{path}/{folder}");
    }

    let mut keep: Vec<Entry> = Vec::new();
    let mut junk: Vec<Entry> = Vec::new();
    for e in &entries {
        if e.is_dir || !scope_ids.contains(&e.parent_id) {
            continue;
        }
        let (ok, _reason) = classify(&e.name, e.size, opts.min_size);
        if ok {
            // 平铺模式只碰与本片番号相关的文件，避免误动演员目录里的其他影片
            if residue_ids.is_empty() {
                let belongs = detect_code(&e.name)
                    .map(|c| c.to_uppercase() == code)
                    .unwrap_or(false)
                    || e.name.to_uppercase().contains(&code);
                if !belongs {
                    continue;
                }
            }
            keep.push(e.clone());
        } else {
            // 没有残留文件夹时，只清理明显的广告图/非视频，且必须与本片有关
            if residue_ids.is_empty() {
                let belongs = detect_code(&e.name)
                    .map(|c| c.to_uppercase() == code)
                    .unwrap_or(false);
                if !belongs && !has_ad_marker(&e.name) {
                    continue;
                }
            }
            junk.push(e.clone());
        }
    }

    let mut moved = 0usize;
    let mut by_src: HashMap<String, Vec<String>> = HashMap::new();
    for f in &keep {
        if f.parent_id != target_id {
            by_src.entry(f.parent_id.clone()).or_default().push(f.id.clone());
        }
    }
    for (src, ids) in by_src {
        if client.move_files(account_id, &ids, &target_id, &src).await.is_ok() {
            moved += ids.len();
        } else {
            for id in ids {
                if client.move_files(account_id, &[id], &target_id, &src).await.is_ok() {
                    moved += 1;
                }
            }
        }
    }

    let mut cleaned = 0usize;
    if opts.cleanup {
        let mut by_parent: HashMap<String, Vec<String>> = HashMap::new();
        for j in &junk {
            by_parent.entry(j.parent_id.clone()).or_default().push(j.id.clone());
        }
        for (pid, ids) in by_parent {
            if client.delete_files(account_id, &ids, &pid).await.is_ok() {
                cleaned += ids.len();
            } else {
                for id in ids {
                    if client.delete_files(account_id, &[id], &pid).await.is_ok() {
                        cleaned += 1;
                    }
                }
            }
        }
    }

    let mut renamed = 0usize;
    if opts.rename {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for f in &keep {
            let want = target_name(&f.name, &code);
            if want != f.name {
                counts.entry(want.clone()).or_insert(0);
            }
        }
        let mut seen: HashMap<String, usize> = HashMap::new();
        for f in &keep {
            if !is_video(&f.name) && !is_subtitle(&f.name) {
                continue;
            }
            let mut want = target_name(&f.name, &code);
            if want == f.name {
                continue;
            }
            let total = counts.get(&want).copied().unwrap_or(1);
            if total > 1 {
                let n = seen.entry(want.clone()).or_insert(0);
                *n += 1;
                let (stem, ext) = match want.rfind('.') {
                    Some(i) => (want[..i].to_string(), want[i..].to_string()),
                    None => (want.clone(), String::new()),
                };
                want = format!("{stem}.{:02}{ext}", n);
            }
            if client.rename_file(account_id, &f.id, &want, &target_id).await.is_ok() {
                renamed += 1;
            }
        }
    }

    // 删除已搬空的残留番号文件夹（最深优先）
    let mut removed_folders = 0usize;
    let mut dirs: Vec<Entry> = entries.into_iter().filter(|e| e.is_dir).collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.depth));
    for d in dirs {
        if d.id == target_id || !residue_ids.contains(&d.id) {
            continue;
        }
        if let Ok(listed) = client.list_dir(account_id, &d.id, "", None).await {
            let empty = listed.get("items").and_then(Value::as_array).map(|a| a.is_empty()).unwrap_or(false);
            if empty
                && client
                    .delete_files(account_id, &[d.id.clone()], &d.parent_id)
                    .await
                    .is_ok()
            {
                removed_folders += 1;
            }
        }
    }

    Ok(OrganizeReport {
        target_path,
        moved,
        cleaned,
        renamed,
        removed_folders,
    })
}

pub fn organize_summary(report: &OrganizeReport) -> Value {
    json!({
        "target_path": report.target_path,
        "moved": report.moved,
        "cleaned": report.cleaned,
        "renamed": report.renamed,
        "removed_folders": report.removed_folders,
    })
}
