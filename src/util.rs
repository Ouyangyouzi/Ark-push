use serde_json::Value;
use sha1::{Digest, Sha1};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

pub fn sha1_hex(s: &str) -> String {
    let mut h = Sha1::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

pub fn short_hash(s: &str, n: usize) -> String {
    sha1_hex(s).chars().take(n).collect()
}

pub fn atomic_write(path: &Path, data: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let pid = std::process::id();
    let tmp = path.with_extension(format!("tmp{pid}"));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.flush()?;
        let _ = f.sync_all();
    }
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(mode));
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn write_json(path: &Path, value: &Value) -> std::io::Result<()> {
    let s = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    atomic_write(path, &s, None)
}

pub fn read_json(path: &Path) -> Option<Value> {
    let raw = fs::read(path).ok()?;
    serde_json::from_slice(&raw).ok()
}

pub fn read_json_or_default(path: &Path, default: Value) -> Value {
    read_json(path).unwrap_or(default)
}

pub fn json_str(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

pub fn json_i64(v: &Value, key: &str, default: i64) -> i64 {
    v.get(key)
        .and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse::<i64>().ok())))
        .unwrap_or(default)
}

pub fn json_bool(v: &Value, key: &str, default: bool) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(Value::String(s)) => matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "on" | "yes"),
        _ => default,
    }
}

pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn nfkc(s: &str) -> String {
    s.nfkc().collect()
}

pub fn norm_actor_name(v: &str) -> String {
    let s = nfkc(v.trim());
    let filtered: String = s
        .chars()
        .filter(|c| {
            !c.is_whitespace()
                && !matches!(
                    c,
                    '-' | '_' | '\u{00b7}' | '\u{2026}' | '\u{3001}' | ',' | '\u{ff0c}' | ':'
                        | '\u{ff1a}' | ';' | '\u{ff1b}' | '\'' | '"' | '\u{201c}' | '\u{201d}'
                        | '\u{2018}' | '\u{2019}' | '(' | ')' | '\u{ff08}' | '\u{ff09}' | '['
                        | ']' | '\u{3010}' | '\u{3011}' | '\u{300c}' | '\u{300d}' | '\u{300e}'
                        | '\u{300f}' | '<' | '>' | '\u{300a}' | '\u{300b}' | '/' | '\\' | '|'
                )
        })
        .collect();
    if filtered.is_empty() || code_like(&filtered) {
        String::new()
    } else {
        filtered
    }
}

fn code_like(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 5 {
        return false;
    }
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_uppercase() {
        i += 1;
    }
    if !(2..=6).contains(&i) || i >= b.len() || b[i] != b'-' {
        return false;
    }
    let d = &b[i + 1..];
    (2..=7).contains(&d.len()) && d.iter().all(u8::is_ascii_digit)
}

pub fn actor_from_path(path: &str) -> String {
    let skip = regex::Regex::new(r"(?i)^(?:\u{4e91}\u{4e0b}\u{8f7d}|\u{8bb0}\u{5f55}\u{6587}\u{4ef6}|Zpool|movies|mov(?:-\d+)?|\d+|[A-Z]{2,}-\d+)$").unwrap();
    let normalized = path.replace('\\', "/");
    let parts: Vec<String> = normalized
        .split('/')
        .filter(|p| !p.trim().is_empty())
        .map(|p| p.trim().to_string())
        .collect();
    for s in parts.iter().rev() {
        if s.is_empty() {
            continue;
        }
        // 演员名只能来自目录。平铺整理后文件直接在演员目录下，
        // 不能把 `PFES-119.mp4` 这种文件名当成演员名返回。
        if is_file_name(s) {
            continue;
        }
        if skip.is_match(s) {
            continue;
        }
        return s.clone();
    }
    String::new()
}

fn is_file_name(name: &str) -> bool {
    matches!(
        ext_lower(name).as_str(),
        "mp4" | "mkv" | "avi" | "wmv" | "mov" | "m4v" | "ts" | "flv" | "rmvb" | "webm"
            | "m2ts" | "iso" | "strm" | "nfo" | "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp"
            | "srt" | "ass" | "ssa" | "sub" | "vtt" | "txt" | "url" | "torrent" | "zip" | "rar"
            | "7z" | "mp3" | "flac" | "aac" | "m4a"
    )
}

pub fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

pub fn ext_lower(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

pub fn posix_basename(url: &str) -> String {
    let no_q = url.split(['?', '#']).next().unwrap_or(url);
    no_q.rsplit('/').next().unwrap_or(no_q).to_string()
}

pub fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = rel.replace('\\', "/");
    let mut out = root.to_path_buf();
    for comp in Path::new(&rel).components() {
        match comp {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if out.starts_with(root) { Some(out) } else { None }
}

pub fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}
