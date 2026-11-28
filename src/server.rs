use crate::actors::{is_actor_name, ActorService};
use crate::auth::{allowed_without_auth, token_from_headers, AuthStore};
use crate::backup;
use crate::config::{iso_now, now_secs, Config};
use crate::fileindex::FileIndex;
use crate::indexer;
use crate::jobs::JobStore;
use crate::litepan::{name_from_url, provider_from_caps, LitePanPool};
use crate::organize::{options_from, organize_dir, organize_summary};
use crate::pending::PendingStore;
use crate::indexer::detect_code;
use crate::search::{doc_id, match_keyword, normalize_key};
use crate::search_fts::SearchStore;
use crate::state::StateStore;
use crate::util::{
    actor_from_path, atomic_write, content_type_for, ext_lower, html_escape, json_bool, json_i64,
    json_str, nfkc, norm_actor_name, read_json, safe_join, sha1_hex, stem,
};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower_http::cors::{Any, CorsLayer};

/// 目录列表内存缓存：搜索校验/补搜时同一目录只请求 LitePan 一次。
#[derive(Default)]
pub struct DirCache {
    inner: parking_lot::RwLock<HashMap<(i64, String), DirCacheEntry>>,
}

struct DirCacheEntry {
    names: Arc<HashSet<String>>,
    rows: Arc<Vec<Value>>,
    at: Instant,
}

impl DirCache {
    const TTL_SECS: u64 = 120;
    const MAX_ENTRIES: usize = 512;

    pub fn get(
        &self,
        account_id: i64,
        parent: &str,
    ) -> Option<(Arc<HashSet<String>>, Arc<Vec<Value>>)> {
        let map = self.inner.read();
        let e = map.get(&(account_id, parent.to_string()))?;
        if e.at.elapsed().as_secs() <= Self::TTL_SECS {
            Some((e.names.clone(), e.rows.clone()))
        } else {
            None
        }
    }

    pub fn put(
        &self,
        account_id: i64,
        parent: &str,
        names: Arc<HashSet<String>>,
        rows: Arc<Vec<Value>>,
    ) {
        let mut map = self.inner.write();
        if map.len() >= Self::MAX_ENTRIES {
            map.retain(|_, e| e.at.elapsed().as_secs() <= Self::TTL_SECS);
        }
        map.insert(
            (account_id, parent.to_string()),
            DirCacheEntry {
                names,
                rows,
                at: Instant::now(),
            },
        );
    }

    pub fn clear(&self) {
        self.inner.write().clear();
    }
}

pub struct AppState {
    pub cfg: Config,
    pub state: Arc<StateStore>,
    pub auth: Arc<AuthStore>,
    pub pool: Arc<LitePanPool>,
    pub search: Arc<SearchStore>,
    pub fileindex: Arc<FileIndex>,
    pub dir_cache: Arc<DirCache>,
    pub pending: Arc<PendingStore>,
    pub jobs: Arc<JobStore>,
    pub actors: Arc<ActorService>,
    pub started_at: String,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub body: Value,
}

impl ApiError {
    pub fn new(status: StatusCode, error: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({"ok": false, "error": error.into()}),
        }
    }
    pub fn value(status: StatusCode, body: Value) -> Self {
        Self { status, body }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

type ApiResult = Result<Response, ApiError>;

fn ok(v: Value) -> Response {
    Json(v).into_response()
}

fn ok_status(status: StatusCode, v: Value) -> Response {
    (status, Json(v)).into_response()
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("内部错误:{e}"))
}

fn bad(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, e.to_string())
}

fn parse_body(body: &Bytes) -> Result<Value, ApiError> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    let v: Value = serde_json::from_slice(body).map_err(|e| bad(format!("请求体不是合法 JSON:{e}")))?;
    if !v.is_object() {
        return Err(bad("请求体必须是 JSON 对象"));
    }
    Ok(v)
}

fn query_map(q: Option<&str>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(q) = q {
        for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
            if !v.is_empty() && !out.contains_key(k.as_ref()) {
                out.insert(k.to_string(), v.to_string());
            }
        }
    }
    out
}

fn bool_from(v: Option<&Value>, default: bool) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(Value::String(s)) => matches!(s.to_lowercase().as_str(), "1" | "true" | "on" | "yes"),
        _ => default,
    }
}

async fn auth_middleware(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if !allowed_without_auth(&path) {
        let q = query_map(req.uri().query());
        let token = token_from_headers(req.headers(), q.get("token").map(String::as_str));
        if !st.auth.check(token.as_deref()) {
            return ApiError::new(StatusCode::UNAUTHORIZED, "未登录或会话已过期").into_response();
        }
    }
    next.run(req).await
}

pub fn build_router(st: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS, Method::PUT, Method::DELETE])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            header::HeaderName::from_static("x-auth-token"),
            header::HeaderName::from_static("x-ark-key"),
            header::HeaderName::from_static("x-plugin-token"),
        ]);

    let router = Router::new()
        .route("/", get(index_page))
        .route("/player", get(player_page))
        .route("/health", get(health))
        .route("/api/health", get(health))
        .route("/userscript", get(userscript_page))
        .route("/userscript/latest.user.js", get(userscript_latest))
        .route("/plugins/sdk.js", get(plugin_sdk))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/logout", post(auth_logout))
        .route("/api/auth/session", get(auth_session))
        .route("/api/auth/update", post(auth_update))
        .route("/search", get(search_handler).post(search_handler))
        .route("/api/search", get(search_handler).post(search_handler))
        .route("/stats", get(stats))
        .route("/api/stats", get(stats))
        .route("/api/config", get(config))
        .route("/api/settings", get(get_settings).post(post_settings))
        .route("/api/push", post(push).get(list_jobs))
        .route("/api/push/{id}", get(get_job))
        .route("/jobs", get(list_jobs))
        .route("/api/jobs", get(list_jobs))
        .route("/sync", post(trigger_sync))
        .route("/api/sync", post(trigger_sync))
        .route("/link", post(trigger_link))
        .route("/api/link", post(trigger_link))
        .route("/api/pending", get(pending_list).post(pending_add))
        .route("/api/pending/generate", post(pending_generate))
        .route("/api/pending/verify", post(pending_verify))
        .route("/api/pending/dismiss", post(pending_dismiss))
        .route("/api/litepan/creds", get(litepan_creds_get).post(litepan_creds_post))
        .route("/api/litepan/status", get(litepan_status))
        .route("/api/match/config", get(match_config_get).post(match_config_post))
        .route("/api/fs/list", get(fs_list))
        .route("/api/favorites", get(favorites_get).post(favorites_post))
        .route("/api/backup/export", post(backup_export))
        .route("/api/backup/import", post(backup_import))
        .route("/api/litepan/accounts", post(lp_accounts))
        .route("/api/litepan/offline/tasks", get(lp_offline_tasks))
        .route("/api/litepan/offline/tasks/delete", post(lp_offline_tasks_delete))
        .route("/api/litepan/list", post(lp_list))
        .route("/api/litepan/search", post(lp_search))
        .route("/api/litepan/offline", post(lp_offline))
        .route("/api/litepan/organize", post(lp_organize))
        .route("/api/litepan/dirs", post(lp_dirs))
        .route("/api/match", post(match_dir))
        .route("/api/litepan/strm-status", post(lp_strm_status))
        .route("/api/litepan/strm-generate", post(lp_strm_generate))
        .route("/api/litepan/delete", post(lp_delete))
        .route("/api/litepan/move", post(lp_move))
        .route("/api/litepan/copy", post(lp_copy))
        .route("/api/litepan/rename", post(lp_rename))
        .route("/api/litepan/create-folder", post(lp_create_folder))
        .route("/api/litepan/create-file", post(lp_create_file))
        .route("/api/litepan/play", post(lp_play))
        .route("/api/litepan/open", post(lp_open))
        .route("/api/play/stream", get(play_stream))
        .route("/api/ark/search", post(ark_search))
        .route("/api/ark/index/scan", post(ark_index_scan))
        .route("/api/ark/index/status", get(ark_index_status))
        .route("/api/ark/cache/clear", post(ark_cache_clear))
        .route("/api/ark/play", post(ark_play))
        .route("/api/ark/find-by-code", post(ark_find_by_code))
        .route("/api/ark/rename", post(ark_rename))
        .route("/api/ark/resolve", post(ark_resolve))
        .route("/api/ark/delete", post(ark_delete))
        .route("/api/actor", get(actor_info))
        .route("/api/actors", get(actors_list))
        .route("/api/actor/dirs", post(actor_dirs))
        .route("/api/actor/profiles", post(actor_profiles))
        .route("/api/actor/by-code", post(actor_by_code))
        .route("/api/actor/ranking", get(actor_ranking))
        .route("/api/actor-cache/config", get(actor_cache_config_get).post(actor_cache_config_post))
        .route("/api/actor-cache/scan", post(actor_cache_scan))
        .route("/api/actor-cache/tree-refresh", post(actor_cache_tree_refresh))
        .route("/api/actor-cache/status", get(actor_cache_status))
        .route("/api/actor-cache/roots", get(actor_cache_roots))
        .route("/api/actor-cache/clear", post(actor_cache_clear))
        .route("/api/actor-cache/actors", get(actor_cache_actors))
        .route("/api/emby/test", post(emby_test))
        .route("/api/emby/person", post(emby_person))
        .route("/api/actor/emby-count", post(emby_count))
        .route("/avatar/{file}", get(avatar_file))
        .route("/api/poster/dirs", post(poster_dirs))
        .route("/api/poster/scan", post(poster_scan))
        .route("/api/poster/generate", post(poster_generate))
        .route("/api/poster/status", get(poster_status))
        .route("/api/runtime/ffmpeg", get(runtime_ffmpeg))
        .route("/api/runtime/ffmpeg/install", post(runtime_ffmpeg_install))
        .route("/api/runtime/storage", get(runtime_storage))
        .route("/api/plugins", get(plugins_list))
        .route("/api/plugins/summary", get(plugins_summary))
        .route("/api/plugins/nav", get(plugins_nav))
        .route("/api/plugins/inspect", post(plugins_inspect))
        .route("/api/plugins/install", post(plugins_install))
        .route("/api/plugins/{id}", get(plugin_detail))
        .route("/api/plugins/{id}/enable", post(plugin_enable))
        .route("/api/plugins/{id}/disable", post(plugin_disable))
        .route("/api/plugins/{id}/reload", post(plugin_reload))
        .route("/api/plugins/{id}/rollback", post(plugin_rollback))
        .route("/api/plugins/{id}/uninstall", post(plugin_uninstall))
        .route("/api/plugins/{id}/config", post(plugin_config))
        .fallback(not_found);

    router
        .layer(middleware::from_fn(common_headers))
        .layer(cors)
        .layer(middleware::from_fn_with_state(st.clone(), auth_middleware))
        .with_state(st)
}

async fn common_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::HeaderName::from_static("access-control-allow-private-network"),
        HeaderValue::from_static("true"),
    );
    resp
}

async fn not_found(req: Request) -> Response {
    ApiError::new(
        StatusCode::NOT_FOUND,
        format!("未知路径 {} {}", req.method(), req.uri().path()),
    )
    .into_response()
}

// ---------------------------------------------------------------- pages
async fn serve_file(st: &AppState, rel: &str, ctype: &str) -> Response {
    let path = FsPath::new(&st.cfg.ui_dir).join(rel);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut resp = (StatusCode::OK, Body::from(bytes)).into_response();
            if let Ok(value) = HeaderValue::from_str(ctype) {
                resp.headers_mut().insert(header::CONTENT_TYPE, value);
            }
            resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            resp
        }
        Err(e) => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("页面缺失({rel}):{e}")).into_response(),
    }
}

async fn index_page(State(st): State<Arc<AppState>>) -> Response {
    serve_file(&st, "index.html", "text/html; charset=utf-8").await
}

async fn player_page(State(st): State<Arc<AppState>>) -> Response {
    serve_file(&st, "player.html", "text/html; charset=utf-8").await
}

async fn userscript_page(State(st): State<Arc<AppState>>) -> Response {
    let path = FsPath::new(&st.cfg.ui_dir).join("userscript").join("latest.user.js");
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(t) => t,
        Err(e) => return ApiError::new(StatusCode::NOT_FOUND, format!("油猴脚本缺失:{e}")).into_response(),
    };
    let html = format!(
        "<!doctype html><html><head><meta charset='utf-8'><title>Ark-push 油猴脚本</title></head><body style='font-family:sans-serif;padding:24px'><h2>Ark-push 油猴脚本</h2><p>点击安装/更新：</p><p><a href='/userscript/latest.user.js'>latest.user.js</a></p><pre style='white-space:pre-wrap;max-height:60vh;overflow:auto;background:#111;color:#ddd;padding:12px'>{}</pre></body></html>",
        html_escape(&text.chars().take(2000).collect::<String>())
    );
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

async fn userscript_latest(State(st): State<Arc<AppState>>) -> Response {
    let path = FsPath::new(&st.cfg.ui_dir).join("userscript").join("latest.user.js");
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
            bytes,
        )
            .into_response(),
        Err(e) => ApiError::new(StatusCode::NOT_FOUND, format!("油猴脚本缺失:{e}")).into_response(),
    }
}

async fn plugin_sdk(State(st): State<Arc<AppState>>) -> Response {
    serve_file(&st, "plugin-sdk.js", "application/javascript; charset=utf-8").await
}

// ---------------------------------------------------------------- auth
async fn health() -> Response {
    ok(json!({"ok": true, "service": "ark-push"}))
}

async fn auth_login(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let body = parse_body(&body)?;
    let username = json_str(&body, "username");
    let password = body.get("password").and_then(Value::as_str).unwrap_or("");
    let wait = st.auth.locked_remaining("");
    if wait > 0 {
        return Ok(ok_status(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"ok": false, "error": format!("登录失败次数过多,请 {wait} 秒后再试")}),
        ));
    }
    let Some(token) = st.auth.login(&username, password, "") else {
        return Ok(ok_status(
            StatusCode::UNAUTHORIZED,
            json!({"ok": false, "error": "账号或密码错误"}),
        ));
    };
    let mut resp = ok(json!({
        "ok": true,
        "username": st.auth.username(),
        "default_password": st.auth.using_default_password(),
        "token": token,
    }));
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "lps_session={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800"
        ))
        .unwrap(),
    );
    Ok(resp)
}

async fn auth_logout(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let q: HashMap<String, String> = HashMap::new();
    if let Some(token) = token_from_headers(&headers, q.get("token").map(String::as_str)) {
        st.auth.logout(&token);
    }
    let mut resp = ok(json!({"ok": true}));
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("lps_session=; Path=/; HttpOnly; Max-Age=0"),
    );
    resp
}

async fn auth_session(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let token = token_from_headers(&headers, None);
    let authenticated = st.auth.check(token.as_deref());
    ok(json!({
        "ok": true,
        "authenticated": authenticated,
        "login_required": !authenticated,
        "username": if authenticated { st.auth.username() } else { String::new() },
        "default_password": authenticated && st.auth.using_default_password(),
    }))
}

async fn auth_update(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let body = parse_body(&body)?;
    let token = token_from_headers(&headers, None);
    if !st.auth.check(token.as_deref()) {
        return Ok(ok_status(
            StatusCode::UNAUTHORIZED,
            json!({"ok": false, "error": "未登录或会话已过期", "login_required": true}),
        ));
    }
    let current = body.get("current_password").and_then(Value::as_str).unwrap_or("");
    if current.is_empty() || !st.auth.verify(&st.auth.username(), current) {
        return Ok(ok_status(
            StatusCode::FORBIDDEN,
            json!({"ok": false, "error": "当前密码校验失败"}),
        ));
    }
    let new_user = body.get("username").and_then(Value::as_str).unwrap_or("").trim();
    let new_pwd = body.get("new_password").and_then(Value::as_str).unwrap_or("");
    if !new_user.is_empty() && !(1..=40).contains(&new_user.chars().count()) {
        return Err(bad("用户名长度需在 1 ~ 40 字符"));
    }
    if !new_pwd.is_empty() && new_pwd.chars().count() < 5 {
        return Err(bad("新密码至少 5 位"));
    }
    if new_user.is_empty() && new_pwd.is_empty() {
        return Err(bad("没有要修改的内容"));
    }
    st.auth.set_credentials(
        if new_user.is_empty() { None } else { Some(new_user) },
        if new_pwd.is_empty() { None } else { Some(new_pwd) },
    );
    if let Some(token) = token {
        st.auth.logout(&token);
    }
    let mut resp = ok(json!({"ok": true, "username": st.auth.username(), "relogin": true}));
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("lps_session=; Path=/; HttpOnly; Max-Age=0"),
    );
    Ok(resp)
}

// ---------------------------------------------------------------- search
async fn search_handler(State(st): State<Arc<AppState>>, method: Method, q: Query<HashMap<String, String>>, body: Bytes) -> ApiResult {
    let mut payload = if method == Method::POST {
        let v = parse_body(&body)?;
        let mut m = Map::new();
        for key in [
            "q", "limit", "offset", "filter", "sort", "facets", "attributesToRetrieve",
            "attributesToHighlight", "attributesToCrop", "cropLength", "highlightPreTag",
            "highlightPostTag", "matchingStrategy", "showRankingScore", "distinct",
        ] {
            if let Some(v) = v.get(key) {
                m.insert(key.to_string(), v.clone());
            }
        }
        Value::Object(m)
    } else {
        let mut m = Map::new();
        for (k, v) in &q.0 {
            if !v.is_empty() {
                m.insert(k.clone(), Value::String(v.clone()));
            }
        }
        Value::Object(m)
    };
    if payload.get("limit").is_none() {
        payload["limit"] = json!(20);
    }
    if payload.get("attributesToRetrieve").is_none() {
        payload["attributesToRetrieve"] = json!([
            "id", "title", "path", "year", "type", "netdisk", "poster", "actors",
            "genres", "code", "season", "episode", "has_nfo"
        ]);
    }
    let result = st.search.search(&payload);
    Ok(ok(json!({"ok": true, "result": result})))
}

// ---------------------------------------------------------------- stats/settings
async fn stats(State(st): State<Arc<AppState>>) -> Response {
    ok(json!({
        "ok": true,
        "state": st.state.summary(),
        "link_115": st.state.get_bool("link_115", Some(st.cfg.link_115)).unwrap_or(st.cfg.link_115),
        "link_enabled": st.cfg.link_enabled,
        "sync_interval_min": st.cfg.sync_interval_min,
        "index_alias": "embedded",
        "pending": st.pending.counts(),
        "index": st.search.stats(),
        "meili": {"status": "embedded", "backend": "embedded"},
        "port": st.cfg.http_port,
    }))
}

async fn config(State(st): State<Arc<AppState>>) -> Response {
    let (url, user, _) = st.pool.creds();
    ok(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "build": std::env::var("ARKPUSH_BUILD").unwrap_or_default(),
        "started_at": st.started_at,
        "litepan_url": url,
        "litepan_username": user,
        "link_115": st.state.get_bool("link_115", Some(st.cfg.link_115)).unwrap_or(st.cfg.link_115),
        "index_alias": "embedded",
        "http_port": st.cfg.http_port,
    }))
}

async fn get_settings(State(st): State<Arc<AppState>>) -> Response {
    let actor_roots = actor_roots(&st, &json!({}));
    let first = actor_roots.first().cloned().unwrap_or_else(|| json!({"path":"","account_id":0}));
    ok(json!({
        "ok": true,
        "link_115": st.state.get_bool("link_115", Some(st.cfg.link_115)).unwrap_or(st.cfg.link_115),
        "link_115_env": st.cfg.link_115,
        "index_source": st.state.get_str("index_source", "netdisk"),
        "netdisk_roots": st.state.get_setting("netdisk_roots", json!([])),
        "actor_roots": actor_roots,
        "actor_root": first.get("path").cloned().unwrap_or(json!("")),
        "actor_account_id": first.get("account_id").cloned().unwrap_or(json!(0)),
        "cache_dir": cache_dir(&st),
        "cache_dir_default": st.cfg.cache_dir_resolved(),
        "avatar_dir": avatar_dir(&st),
        "avatar_dir_default": st.cfg.avatar_dir_resolved(),
        "cleanup_accounts": st.state.get_setting("cleanup_accounts", json!([])),
    }))
}

async fn post_settings(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let body = parse_body(&body)?;
    let mut patch = Map::new();
    if body.get("link_115").is_some() {
        patch.insert("link_115".into(), body["link_115"].clone());
    }
    if let Some(src) = body.get("index_source").and_then(Value::as_str) {
        if src != "netdisk" {
            return Err(bad("index_source 仅支持 netdisk"));
        }
        patch.insert("index_source".into(), json!("netdisk"));
    }
    for key in ["netdisk_roots", "actor_roots", "cleanup_accounts"] {
        if let Some(v) = body.get(key) {
            if !v.is_array() {
                return Err(bad(format!("{key} 需为数组")));
            }
            patch.insert(key.into(), v.clone());
        }
    }
    if let Some(v) = body.get("actor_root") {
        patch.insert("actor_root".into(), v.clone());
    }
    if let Some(v) = body.get("actor_account_id") {
        patch.insert("actor_account_id".into(), v.clone());
    }
    for key in ["cache_dir", "avatar_dir"] {
        if let Some(v) = body.get(key) {
            let raw = v.as_str().unwrap_or("").trim().to_string();
            if !raw.is_empty() && !raw.starts_with('/') {
                return Err(bad(format!("{key} 必须是容器内绝对路径(以 / 开头)")));
            }
            if !raw.is_empty() {
                let _ = std::fs::create_dir_all(&raw);
            }
            patch.insert(key.into(), if raw.is_empty() { Value::Null } else { Value::String(raw) });
        }
    }
    if patch.is_empty() {
        return Err(bad("没有可识别的设置项"));
    }
    st.state.set_settings(patch);
    Ok(get_settings(State(st)).await)
}

fn actor_roots(st: &AppState, body: &Value) -> Vec<Value> {
    if let Some(root) = body.get("root").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        let mut acct = body.get("account_id").and_then(Value::as_i64).unwrap_or(0);
        if acct == 0 {
            if let Some(arr) = st.state.get_setting("actor_roots", json!([])).as_array() {
                for r in arr {
                    if r.get("path").and_then(Value::as_str).map(|p| p.trim_matches('/')) == Some(root.trim_matches('/')) {
                        acct = r.get("account_id").and_then(Value::as_i64).unwrap_or(0);
                        break;
                    }
                }
            }
        }
        return vec![json!({"path": root.trim_matches('/'), "account_id": acct})];
    }
    let mut out = Vec::new();
    if let Some(arr) = st.state.get_setting("actor_roots", json!([])).as_array() {
        for r in arr {
            let path = r.get("path").and_then(Value::as_str).unwrap_or("").trim_matches('/');
            if !path.is_empty() {
                out.push(json!({"path": path, "account_id": r.get("account_id").and_then(Value::as_i64).unwrap_or(0)}));
            }
        }
    }
    if out.is_empty() {
        let legacy = st.state.get_str("actor_root", "");
        if !legacy.is_empty() {
            out.push(json!({"path": legacy.trim_matches('/'), "account_id": st.state.get_setting("actor_account_id", json!(0)).as_i64().unwrap_or(0)}));
        }
    }
    out
}

fn cache_dir(st: &AppState) -> String {
    let v = st.state.get_str("cache_dir", "");
    if v.is_empty() { st.cfg.cache_dir_resolved() } else { v }
}

fn avatar_dir(st: &AppState) -> String {
    let v = st.state.get_str("avatar_dir", "");
    if v.is_empty() { st.cfg.avatar_dir_resolved() } else { v }
}

// ---------------------------------------------------------------- push/jobs
async fn push(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    submit_push(st, body, true).await
}

async fn lp_offline(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    submit_push(st, body, false).await
}

async fn submit_push(st: Arc<AppState>, body: Bytes, gate: bool) -> ApiResult {
    if gate && !st.state.get_bool("link_115", Some(st.cfg.link_115)).unwrap_or(st.cfg.link_115) {
        return Ok(ok_status(
            StatusCode::FORBIDDEN,
            json!({"ok": false, "error": "115 磁链推送开关已关闭,请在搜索页「设置」里开启"}),
        ));
    }
    let mut payload = parse_body(&body)?;
    for (field, key) in [
        ("litepan_url", "litepan_url"),
        ("username", "litepan_username"),
        ("password", "litepan_password"),
    ] {
        if payload.get(field).and_then(Value::as_str).map(str::is_empty).unwrap_or(true) {
            let saved = st.state.get_str(key, "");
            if !saved.is_empty() {
                payload[field] = Value::String(saved);
            }
        }
    }
    let has_pwd = payload.get("password").and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false);
    if !has_pwd && !st.cfg.push_allow_anon {
        return Ok(ok_status(
            StatusCode::UNAUTHORIZED,
            json!({"ok": false, "error": "请先在容器「设置 → 连接」保存 LitePan 账号密码,或在请求里带上 username/password"}),
        ));
    }
    if payload.get("urls").is_none() && payload.get("url").is_none() && payload.get("magnet").is_none() {
        return Err(bad("缺少 urls/magnet"));
    }
    let job = st.jobs.create(&payload);
    let id = job.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let st2 = st.clone();
    let payload2 = payload.clone();
    let job_id = id.clone();
    tokio::spawn(async move {
        run_push(st2, &job_id, payload2).await;
    });
    Ok(ok_status(
        StatusCode::ACCEPTED,
        json!({"ok": true, "id": id, "state": "pending", "poll": format!("/api/push/{id}")}),
    ))
}

fn urls_from(payload: &Value) -> Vec<String> {
    let raw = payload
        .get("urls")
        .or_else(|| payload.get("url"))
        .or_else(|| payload.get("magnet"));
    match raw {
        Some(Value::String(s)) => vec![s.trim().to_string()].into_iter().filter(|s| !s.is_empty()).collect(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        _ => Vec::new(),
    }
}

pub async fn run_push(st: Arc<AppState>, job_id: &str, payload: Value) {
    st.jobs.update(job_id, json!({"state": "running"}));
    match push_chain(&st, job_id, &payload).await {
        Ok(result) => {
            st.jobs.update(job_id, json!({"state": "done", "result": result}));
            tracing::info!("[job {job_id}] done");
        }
        Err(e) => {
            st.jobs.update(job_id, json!({"state": "error", "error": e.to_string()}));
            tracing::error!("[job {job_id}] failed: {e}");
        }
    }
}

async fn push_chain(st: &Arc<AppState>, job_id: &str, payload: &Value) -> Result<Value, String> {
    let urls = urls_from(payload);
    if urls.is_empty() {
        return Err("urls 为空".into());
    }
    let mut target_path = payload
        .get("target_path")
        .or_else(|| payload.get("target_display_path"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if !target_path.is_empty() && !target_path.starts_with('/') {
        target_path = format!("/{target_path}");
    }
    let file_name = payload.get("file_name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let account_id = payload.get("account_id").and_then(Value::as_i64);
    let provider_kind = payload.get("provider_kind").and_then(Value::as_str).map(str::to_string);
    let client = st.pool.client_for(payload).await.map_err(|e| e.to_string())?;
    st.jobs.step(job_id, "login", "ok", &client.base);
    let account = client
        .pick_account(account_id)
        .await
        .map_err(|e| e.to_string())?;
    let aid = account.get("id").and_then(Value::as_i64).unwrap_or(0);
    st.jobs.step(job_id, "account", "ok", &format!("{aid}:{}", account.get("name").and_then(Value::as_str).unwrap_or("")));
    let caps = client.offline_capabilities(aid).await.unwrap_or_else(|_| json!({}));
    let kind = provider_kind.or_else(|| provider_from_caps(&caps));
    st.jobs.step(job_id, "capabilities", "ok", &format!("provider_kind={}", kind.clone().unwrap_or_else(|| "default".into())));

    let mut target_parent_id = payload.get("target_parent_id").and_then(Value::as_str).map(str::to_string);
    if !target_path.is_empty() && target_path != "/" && target_parent_id.as_deref().unwrap_or("").is_empty() {
        let create = payload.get("create_dirs").map(|v| bool_from(Some(v), true)).unwrap_or(true);
        match client.resolve_dir(aid, &target_path, create).await {
            Ok(res) => {
                target_parent_id = res.get("parent_id").and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty());
                if let Some(p) = res.get("path").and_then(Value::as_str) {
                    if !p.is_empty() {
                        target_path = p.to_string();
                    }
                }
                st.jobs.step(job_id, "resolve", "ok", &format!("{target_path} -> {}", target_parent_id.clone().unwrap_or_default()));
            }
            Err(e) => {
                st.jobs.step(job_id, "resolve", "warn", &e.to_string());
                target_parent_id = None;
            }
        }
    }

    let submitted = client
        .add_offline_urls(
            &urls,
            aid,
            kind.as_deref(),
            if target_path.is_empty() { None } else { Some(&target_path) },
            target_parent_id.as_deref(),
            if file_name.is_empty() { None } else { Some(&file_name) },
        )
        .await;
    let mut task_ids: Vec<String> = Vec::new();
    match submitted {
        Ok(v) => {
            task_ids = task_ids_from(&v);
            st.jobs.step(job_id, "submit", "ok", &format!("任务 {}", task_ids.join(", ")));
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("10008") || msg.contains("任务已存在") {
                task_ids = reuse_existing_offline(&client, st, job_id, &urls, aid, target_parent_id.as_deref()).await?;
            } else {
                return Err(msg);
            }
        }
    }

    let organize = payload.get("organize").map(|v| bool_from(Some(v), false)).unwrap_or(false);
    let opts = payload.get("organize_opts").cloned().unwrap_or_else(|| json!({}));
    let opts_json = if opts.as_object().map(|m| !m.is_empty()).unwrap_or(false) {
        serde_json::to_string(&opts).unwrap_or_default()
    } else {
        String::new()
    };
    let mut registered = Vec::new();
    for url in &urls {
        let name = if urls.len() == 1 && !file_name.is_empty() {
            file_name.clone()
        } else {
            name_from_url(url)
        };
        let id = st
            .pending
            .add(aid, &target_path, &name, 0, url, job_id, organize, &opts_json)
            .map_err(|e| e.to_string())?;
        registered.push(json!({"id": id, "file_name": name, "path": target_path}));
    }
    st.jobs.step(job_id, "register", "ok", &format!("已登记 {} 个待生成项", registered.len()));

    let reg_ids: Vec<i64> = registered.iter().filter_map(|r| r.get("id").and_then(Value::as_i64)).collect();
    let (saved_url, saved_user, saved_pwd) = st.pool.creds();
    let creds = json!({
        "litepan_url": payload.get("litepan_url").cloned().unwrap_or(json!(saved_url)),
        "username": payload.get("username").cloned().unwrap_or(json!(saved_user)),
        "password": payload.get("password").cloned().unwrap_or(json!(saved_pwd)),
    });
    let auto = opts.get("auto_generate").and_then(Value::as_bool).unwrap_or(false);
    if auto && !reg_ids.is_empty() {
        st.jobs.step(job_id, "auto", "pending", "等待离线下载完成后自动整理+生成 STRM");
        let st2 = st.clone();
        let jid = job_id.to_string();
        let tids = task_ids.clone();
        let tp = target_path.clone();
        tokio::spawn(async move {
            watch_and_auto(st2, jid, tids, tp, reg_ids, creds).await;
        });
    } else {
        st.jobs.step(job_id, "auto", "skip", "未勾选自动生成,等用户在「待生成」页点「生成」");
    }

    let roots = indexer::infer_roots_from_settings(&st.state);
    if !roots.is_empty() {
        if let Ok(client) = st.pool.client_for(&Value::Null).await {
            let rep = indexer::run_netdisk_sync(&client, &roots, &st.search, &st.state, indexer::netdisk_max_dirs()).await;
            st.jobs.step(job_id, "index", "ok", &format!("{} 条", rep.get("docs_total").and_then(Value::as_i64).unwrap_or(0)));
        }
    } else {
        let rep = indexer::run_strm_sync(&st.cfg.strm_root, &st.search, &st.state, false).await;
        st.jobs.step(job_id, "index", "ok", &format!("{} 条", rep.get("docs_total").and_then(Value::as_i64).unwrap_or(0)));
    }
    Ok(json!({"submitted": task_ids, "registered": registered}))
}

fn task_ids_from(v: &Value) -> Vec<String> {
    let items: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![v],
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    for item in items {
        for key in ["id", "task_id", "taskId", "info_hash", "hash"] {
            if let Some(x) = item.get(key) {
                if !x.is_null() && x.as_str() != Some("") {
                    out.push(x.to_string().trim_matches('"').to_string());
                    break;
                }
            }
        }
    }
    out
}

async fn reuse_existing_offline(
    client: &crate::litepan::LitePanClient,
    st: &AppState,
    job_id: &str,
    urls: &[String],
    account_id: i64,
    target_parent_id: Option<&str>,
) -> Result<Vec<String>, String> {
    let tasks = client.offline_tasks().await.unwrap_or_default();
    let mut reused = Vec::new();
    for url in urls {
        let btih = regex::Regex::new(r"btih:([0-9a-zA-Z]{32,40})")
            .ok()
            .and_then(|re| re.captures(url))
            .and_then(|c| c.get(1).map(|m| m.as_str().to_lowercase()));
        let dn = url
            .split('?')
            .nth(1)
            .and_then(|q| {
                url::form_urlencoded::parse(q.as_bytes())
                    .find(|(k, _)| k == "dn")
                    .map(|(_, v)| v.to_string())
            })
            .unwrap_or_default();
        let cand = tasks.iter().find(|t| {
            btih.as_ref().map(|b| t.get("source").and_then(Value::as_str).unwrap_or("").to_lowercase().contains(b)).unwrap_or(false)
        }).or_else(|| {
            if dn.is_empty() { None } else {
                tasks.iter().find(|t| t.get("name").and_then(Value::as_str).unwrap_or("").to_lowercase().contains(&dn.to_lowercase()))
            }
        });
        let Some(cand) = cand else {
            return Err("115 提示链接已存在,但在离线任务列表里找不到对应任务".into());
        };
        let status = cand.get("status").and_then(Value::as_str).unwrap_or("").to_lowercase();
        let progress = cand.get("progress").and_then(Value::as_f64).unwrap_or(0.0);
        let done = matches!(status.as_str(), "success" | "failed" | "error" | "completed" | "canceled" | "cancelled") || progress >= 100.0;
        if !done {
            return Err(format!("该链接已在 115 离线列表,进度 {progress:.0}%"));
        }
        let fid = cand.get("file_id").and_then(Value::as_str).unwrap_or("");
        if !fid.is_empty() {
            if let Some(parent) = target_parent_id {
                let _ = client.copy_files(account_id, &[fid.to_string()], parent, "").await;
            }
        }
        reused.push(cand.get("task_id").or_else(|| cand.get("info_hash")).map(|v| v.to_string().trim_matches('"').to_string()).unwrap_or_default());
        st.jobs.step(job_id, "submit", "ok", "复用已完成离线任务");
    }
    Ok(reused.into_iter().filter(|s| !s.is_empty()).collect())
}

/// 直接看网盘目标目录：本次推送的内容是否已经落盘。
/// 返回用于判断“下载是否还在进行”的签名（名字+大小），None 表示还没出现/还没下完。
async fn fs_download_signature(
    client: &crate::litepan::LitePanClient,
    account_id: i64,
    target_path: &str,
    code: &str,
) -> Option<String> {
    let resolved = client.resolve_dir(account_id, target_path, false).await.ok()?;
    let pid = resolved.get("parent_id").and_then(Value::as_str).unwrap_or("");
    if pid.is_empty() {
        return None;
    }
    let listed = client.list_dir(account_id, pid, "", None).await.ok()?;
    let items = listed.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let code_up = code.to_uppercase();
    for it in items {
        let name = json_str(&it, "name");
        if name.is_empty() {
            continue;
        }
        let hit = detect_code(&name).map(|c| c.to_uppercase() == code_up).unwrap_or(false)
            || name.to_uppercase().contains(&code_up);
        if !hit {
            continue;
        }
        if json_bool(&it, "is_dir", false) {
            let id = json_str(&it, "id");
            if id.is_empty() {
                continue;
            }
            let sub = client.list_dir(account_id, &id, "", None).await.ok()?;
            let rows = sub.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut videos = 0usize;
            let mut sig = String::new();
            for r in &rows {
                let n = json_str(r, "name");
                let size = json_i64(r, "size", 0);
                if indexer::is_video(&n) && size > 0 {
                    videos += 1;
                }
                sig.push_str(&format!("{n}:{size};"));
            }
            // 目录里还没有完整视频，说明还在下载中
            if videos == 0 {
                return None;
            }
            return Some(format!("dir:{name}:{sig}"));
        }
        let size = json_i64(&it, "size", 0);
        if size <= 0 {
            return None;
        }
        return Some(format!("file:{name}:{size}"));
    }
    None
}

async fn watch_and_auto(st: Arc<AppState>, job_id: String, task_ids: Vec<String>, target_path: String, reg_ids: Vec<i64>, creds: Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3 * 3600);
    // 统一小写：离线任务里的 info_hash 是小写，脚本磁力链接里的 btih 可能是大写。
    let wanted: HashSet<String> = task_ids
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect();

    // LitePan 的离线任务状态在部分 115 账号上会一直停在 pending，
    // 所以除了任务状态，还要直接看目标目录里文件是否已经下载完成。
    let mut expect_code = String::new();
    let mut account_id = 0i64;
    if let Some(id) = reg_ids.first() {
        if let Some(e) = st.pending.get(*id) {
            account_id = e.get("account_id").and_then(Value::as_i64).unwrap_or(0);
            let fname = e.get("file_name").and_then(Value::as_str).unwrap_or("");
            expect_code = detect_code(fname).unwrap_or_else(|| fname.to_string());
        }
    }
    let mut stable_sig: Option<String> = None;
    let mut stable_rounds = 0u32;

    while tokio::time::Instant::now() < deadline {
        let mut finished = false;
        let mut note = String::new();
        if let Ok(client) = st.pool.client_for(&creds).await {
            let _ = client.refresh_offline_tasks().await;
            if let Ok(tasks) = client.offline_tasks().await {
                let sel: Vec<&Value> = if !wanted.is_empty() {
                    tasks.iter().filter(|t| {
                        let tid = t.get("task_id").map(|v| v.to_string().trim_matches('"').to_lowercase()).unwrap_or_default();
                        let ih = t.get("info_hash").map(|v| v.to_string().trim_matches('"').to_lowercase()).unwrap_or_default();
                        wanted.contains(&tid) || wanted.contains(&ih)
                    }).collect()
                } else {
                    // 重启后可能拿不到任务 ID：只认“同目标目录 + 本片番号”的任务，
                    // 不能把同一演员目录下几十个旧任务都算成本次推送。
                    let code_up = expect_code.to_uppercase();
                    tasks.iter().filter(|t| {
                        let same_target = t.get("target_display_path").and_then(Value::as_str).map(|p| p.trim_end_matches('/')) == Some(target_path.trim_end_matches('/'));
                        if !same_target {
                            return false;
                        }
                        if code_up.is_empty() {
                            return true;
                        }
                        let name = t.get("name").and_then(Value::as_str).unwrap_or("").to_uppercase();
                        let src = t.get("source").and_then(Value::as_str).unwrap_or("").to_uppercase();
                        name.contains(&code_up) || src.contains(&code_up)
                    }).collect()
                };
                if !sel.is_empty() {
                    let done = sel.iter().filter(|t| {
                        let s = t.get("status").and_then(Value::as_str).unwrap_or("").to_lowercase();
                        matches!(s.as_str(), "success" | "failed" | "error" | "completed" | "canceled" | "cancelled")
                            || t.get("progress").and_then(Value::as_f64).unwrap_or(0.0) >= 100.0
                    }).count();
                    note = format!("离线进度 {done}/{} 完成", sel.len());
                    finished = done == sel.len();
                }
            }
            if !finished && !expect_code.is_empty() && account_id > 0 {
                match fs_download_signature(&client, account_id, &target_path, &expect_code).await {
                    Some(sig) => {
                        if stable_sig.as_deref() == Some(sig.as_str()) {
                            stable_rounds += 1;
                        } else {
                            stable_sig = Some(sig);
                            stable_rounds = 0;
                        }
                        if stable_rounds >= 2 {
                            finished = true;
                            note = format!("检测到 {expect_code} 已落盘且大小稳定,按下载完成处理");
                        } else {
                            note = format!("目标目录已出现 {expect_code},等待文件稳定({stable_rounds}/2)");
                        }
                    }
                    None => {
                        stable_sig = None;
                        stable_rounds = 0;
                    }
                }
            }
        }
        if !note.is_empty() {
            st.jobs.step(&job_id, "auto", "running", &note);
        }
        if finished {
            st.jobs.step(&job_id, "auto", "ok", "离线完成,开始自动整理+生成 STRM");
            match generate_pending(&st, &reg_ids, &creds).await {
                Ok(rep) => {
                    st.jobs.step(&job_id, "auto", "ok", &format!("自动整理+生成 STRM 完成:{rep}"));
                }
                Err(e) => {
                    st.jobs.step(&job_id, "auto", "error", &format!("自动整理失败:{e}"));
                }
            }
            return;
        }
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
    st.jobs.step(&job_id, "auto", "warn", "等待离线完成超时(3小时),登记保留,可手动点「生成」");
}

/// 容器重启后恢复「离线完成后自动整理+生成 STRM」。
/// 任务状态可能不可靠，这里同样走目标目录落盘检测。
pub fn resume_auto_pending(st: &Arc<AppState>) {
    let rows = st.pending.list(Some("pending"), 200);
    for e in rows {
        let Some(id) = e.get("id").and_then(Value::as_i64) else { continue };
        let raw = e.get("organize_opts").and_then(Value::as_str).unwrap_or("");
        let opts: Value = serde_json::from_str(raw).unwrap_or_else(|_| json!({}));
        if !opts.get("auto_generate").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let target_path = json_str(&e, "path");
        if target_path.is_empty() {
            continue;
        }
        let (url, user, pwd) = st.pool.creds();
        let creds = json!({"litepan_url": url, "username": user, "password": pwd});
        let job_id = json_str(&e, "job_id");
        // 尽量从磁力链接里恢复 info_hash，这样重启后不会把同目录所有旧任务都当成本次任务。
        let pending_url = json_str(&e, "url");
        let mut task_ids = Vec::new();
        if let Some(cap) = regex::Regex::new(r"btih:([0-9a-zA-Z]{32,40})")
            .ok()
            .and_then(|re| re.captures(&pending_url))
        {
            if let Some(m) = cap.get(1) {
                task_ids.push(m.as_str().to_lowercase());
            }
        }
        let st2 = st.clone();
        tokio::spawn(async move {
            watch_and_auto(st2, job_id, task_ids, target_path, vec![id], creds).await;
        });
        tracing::info!("恢复自动整理任务 pending_id={id} path={}", json_str(&e, "path"));
    }
}

async fn get_job(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match st.jobs.get(&id) {
        Some(job) => ok(json!({"ok": true, "job": job})),
        None => ok_status(StatusCode::NOT_FOUND, json!({"ok": false, "error": "作业不存在(边车重启后内存作业会清空)"})),
    }
}

async fn list_jobs(State(st): State<Arc<AppState>>) -> Response {
    ok(json!({"ok": true, "jobs": st.jobs.recent(20)}))
}

async fn trigger_sync(State(st): State<Arc<AppState>>, q: Query<HashMap<String, String>>, body: Bytes) -> ApiResult {
    let mut full = q.get("full").map(|v| matches!(v.as_str(), "1" | "true" | "on")).unwrap_or(false);
    if let Ok(v) = parse_body(&body) {
        if v.get("full").and_then(Value::as_bool).unwrap_or(false) {
            full = true;
        }
    }
    let roots = indexer::infer_roots_from_settings(&st.state);
    let st2 = st.clone();
    tokio::spawn(async move {
        let rep = if !roots.is_empty() {
            match st2.pool.client_for(&Value::Null).await {
                Ok(client) => indexer::run_netdisk_sync(&client, &roots, &st2.search, &st2.state, indexer::netdisk_max_dirs()).await,
                Err(e) => json!({"error": e.to_string()}),
            }
        } else if indexer::has_fileindex(&st2.cfg.state_dir) {
            indexer::sync_from_fileindex(&st2.cfg.state_dir, &st2.search)
        } else {
            indexer::run_strm_sync(&st2.cfg.strm_root, &st2.search, &st2.state, full).await
        };
        tracing::info!("sync report: {rep}");
    });
    Ok(ok_status(StatusCode::ACCEPTED, json!({"ok": true, "started": true, "full": full})))
}

async fn trigger_link(State(st): State<Arc<AppState>>) -> Response {
    let st2 = st.clone();
    tokio::spawn(async move {
        if let Ok(client) = st2.pool.client_for(&Value::Null).await {
            let _ = run_link(&client).await;
        }
    });
    ok_status(StatusCode::ACCEPTED, json!({"ok": true, "started": true}))
}

async fn run_link(client: &crate::litepan::LitePanClient) -> Value {
    let tasks = client.strm_tasks().await.unwrap_or_default();
    let mut triggered = Vec::new();
    for task in tasks {
        let tid = task.get("id").or_else(|| task.get("task_id")).and_then(Value::as_i64).unwrap_or(0);
        if tid == 0 {
            continue;
        }
        let status = task.get("status").and_then(Value::as_str).unwrap_or("").to_lowercase();
        if !status.is_empty() && !matches!(status.as_str(), "running" | "enabled" | "active" | "idle" | "waiting" | "on" | "ready") {
            continue;
        }
        match client.run_strm_task(tid).await {
            Ok(_) => triggered.push(json!({"id": tid, "name": task.get("name").cloned().unwrap_or(json!("")), "mode": "auto"})),
            Err(e) => triggered.push(json!({"id": tid, "error": e.to_string()})),
        }
    }
    json!({"triggered": triggered})
}

// ---------------------------------------------------------------- pending
async fn pending_list(State(st): State<Arc<AppState>>) -> Response {
    let mut summary = st.pending.summary();
    if let Some(items) = summary.get_mut("items").and_then(Value::as_array_mut) {
        for item in items {
            let actor = actor_from_path(item.get("path").and_then(Value::as_str).unwrap_or(""));
            item["actor"] = Value::String(actor);
        }
    }
    ok(json!({"ok": true, "counts": summary["counts"], "items": summary["items"]}))
}

async fn pending_add(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let account_id = v.get("account_id").and_then(Value::as_i64).unwrap_or(0);
    let path = json_str(&v, "path");
    let file_name = json_str(&v, "file_name");
    let url = json_str(&v, "url");
    if path.is_empty() && file_name.is_empty() && url.is_empty() {
        return Err(bad("缺少 path/file_name/url"));
    }
    let id = st
        .pending
        .add(
            account_id,
            &path,
            &file_name,
            json_i64(&v, "size", 0),
            &url,
            &json_str(&v, "job_id"),
            json_bool(&v, "organize", false),
            v.get("organize_opts").and_then(Value::as_str).unwrap_or(""),
        )
        .map_err(|e| internal(e))?;
    Ok(ok(json!({"ok": true, "id": id, "summary": st.pending.summary()})))
}

async fn pending_generate(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let ids: Vec<i64> = v
        .get("ids")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let all = json_bool(&v, "all", false);
    let ids = if ids.is_empty() && all {
        st.pending
            .list(Some("pending"), 1000)
            .iter()
            .filter_map(|e| e.get("id").and_then(Value::as_i64))
            .collect()
    } else {
        ids
    };
    if ids.is_empty() {
        return Err(bad("请选择要生成的登记项"));
    }
    let (saved_url, saved_user, saved_pwd) = st.pool.creds();
    let creds = json!({
        "litepan_url": v.get("litepan_url").cloned().unwrap_or(json!(saved_url)),
        "username": v.get("username").cloned().unwrap_or(json!(saved_user)),
        "password": v.get("password").cloned().unwrap_or(json!(saved_pwd)),
    });
    let st2 = st.clone();
    tokio::spawn(async move {
        let _ = generate_pending(&st2, &ids, &creds).await;
    });
    Ok(ok_status(StatusCode::ACCEPTED, json!({"ok": true, "started": true})))
}

async fn generate_pending(st: &AppState, ids: &[i64], creds: &Value) -> Result<Value, String> {
    let client = st.pool.client_for(creds).await.map_err(|e| e.to_string())?;
    let mut groups: HashMap<(i64, String), Vec<Value>> = HashMap::new();
    for id in ids {
        if let Some(e) = st.pending.get(*id) {
            if e.get("status").and_then(Value::as_str) == Some("pending") {
                let acc = e.get("account_id").and_then(Value::as_i64).unwrap_or(0);
                let path = e.get("path").and_then(Value::as_str).unwrap_or("").to_string();
                groups.entry((acc, path)).or_default().push(e);
            }
        }
    }
    let mut reports = Vec::new();
    for ((account_id, path), group) in groups {
        let code = detect_code(group[0].get("file_name").and_then(Value::as_str).unwrap_or(""))
            .unwrap_or_else(|| group[0].get("file_name").and_then(Value::as_str).unwrap_or("").to_string());
        let mut target = if path.trim_matches('/').is_empty() { "/".to_string() } else { path.clone() };
        let any_organize = group
            .iter()
            .any(|e| e.get("organize").and_then(Value::as_i64).unwrap_or(0) != 0);
        if any_organize && !code.is_empty() {
            let raw_opts = group
                .first()
                .and_then(|e| e.get("organize_opts").and_then(Value::as_str))
                .unwrap_or("");
            let parsed_opts: Value = serde_json::from_str(raw_opts).unwrap_or_else(|_| json!({}));
            let opts = options_from(&json!({"organize_opts": parsed_opts}));
            match organize_dir(&client, account_id, &target, &code, &opts).await {
                Ok(rep) => {
                    tracing::info!(
                        "整理完成 path={} target={} moved={} cleaned={} renamed={} removed={}",
                        target, rep.target_path, rep.moved, rep.cleaned, rep.renamed, rep.removed_folders
                    );
                    let summary = organize_summary(&rep);
                    st.dir_cache.clear();
                    target = rep.target_path;
                    reports.push(summary);
                }
                Err(e) => {
                    tracing::warn!("整理失败 path={} code={}: {}", target, code, e);
                }
            }
        }
        let resolved = match client.resolve_dir(account_id, &target, false).await {
            Ok(v) => v,
            Err(e) => {
                for item in &group {
                    if let Some(id) = item.get("id").and_then(Value::as_i64) {
                        let _ = st.pending.set_status(id, "error", &e.to_string());
                    }
                }
                continue;
            }
        };
        let parent_id = resolved.get("parent_id").and_then(Value::as_str).unwrap_or("").to_string();
        let listed = match client.list_dir(account_id, &parent_id, "", None).await {
            Ok(v) => v,
            Err(e) => {
                for item in &group {
                    if let Some(id) = item.get("id").and_then(Value::as_i64) {
                        let _ = st.pending.set_status(id, "error", &e.to_string());
                    }
                }
                continue;
            }
        };
        let items = listed.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        if items.is_empty() {
            for item in &group {
                if let Some(id) = item.get("id").and_then(Value::as_i64) {
                    let _ = st.pending.set_status(id, "error", "目录里没有找到本次推送的文件(离线可能尚未完成)");
                }
            }
            continue;
        }
        let dirs: Vec<String> = items
            .iter()
            .filter(|it| json_bool(it, "is_dir", false))
            .filter_map(|it| it.get("name").and_then(Value::as_str).map(str::to_string))
            .collect();
        let upd: Vec<Value> = items
            .iter()
            .filter(|it| !json_str(it, "id").is_empty() && !json_str(it, "name").is_empty())
            .map(|it| {
                json!({
                    "id": json_str(it, "id"), "name": json_str(it, "name"),
                    "path": format!("{}/{}", path.trim_matches('/'), json_str(it, "name")),
                    "is_dir": json_bool(it, "is_dir", false), "size": json_i64(it, "size", 0),
                })
            })
            .collect();
        let added = st.fileindex.upsert(account_id, &upd);
        if added > 0 {
            let _ = code;
        }
        let result = client
            .generate_directory_strm(account_id, &parent_id, &target, &dirs, &items)
            .await;
        for item in &group {
            if let Some(id) = item.get("id").and_then(Value::as_i64) {
                let want = stem(item.get("file_name").and_then(Value::as_str).unwrap_or("")).to_uppercase();
                let hit = items.iter().any(|it| {
                    let n = stem(json_str(it, "name").as_str()).to_uppercase();
                    !want.is_empty() && (want == n || (want.len() >= 4 && n.contains(&want)))
                });
                if hit && result.is_ok() {
                    let _ = st.pending.set_status(id, "generated", "");
                } else if result.is_err() {
                    let _ = st.pending.set_status(id, "error", &result.as_ref().err().map(|e| e.to_string()).unwrap_or_default());
                } else {
                    let _ = st.pending.set_status(id, "error", "目录里没有找到该文件(可能已被整理改名)");
                }
            }
        }
        reports.push(json!({"path": path, "items": items.len(), "ok": result.is_ok()}));
        // 索引更新放后台：Windows 挂载盘上 FTS 写入可能很慢，
        // 不能让它卡住「整理+STRM」完成状态。
        {
            let client = client.clone();
            let search = st.search.clone();
            let dir = path.clone();
            tokio::spawn(async move {
                let _ = indexer::refresh_dir_docs(&client, account_id, &dir, &search).await;
            });
        }
    }
    Ok(json!({"ok": true, "reports": reports, "summary": st.pending.summary()}))
}

async fn pending_verify(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body).unwrap_or_else(|_| json!({}));
    let ids: Option<Vec<i64>> = v.get("ids").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_i64).collect());
    match verify_pending(&st, ids.as_deref()).await {
        Ok(mut r) => {
            r["summary"] = st.pending.summary();
            Ok(ok(r))
        }
        Err(e) => Ok(ok(json!({"ok": false, "error": e, "summary": st.pending.summary()}))),
    }
}

async fn verify_pending(st: &AppState, ids: Option<&[i64]>) -> Result<Value, String> {
    let client = st.pool.client_for(&Value::Null).await.map_err(|e| e.to_string())?;
    let lf = client.localfs_account_id().await.map_err(|e| e.to_string())?;
    if lf == 0 {
        return Err("LitePan 里没有「本地存储」账号".into());
    }
    let roots = client.strm_output_roots(lf).await;
    if roots.is_empty() {
        return Err("本地存储里没找到 STRM 产物目录(/mnt/strm/litepan)".into());
    }
    let mut entries = st.pending.list(None, 500);
    if let Some(ids) = ids {
        let want: HashSet<i64> = ids.iter().copied().collect();
        entries.retain(|e| e.get("id").and_then(Value::as_i64).map(|i| want.contains(&i)).unwrap_or(false));
    }
    let mut checked = 0;
    let mut verified = 0;
    for e in entries {
        if e.get("status").and_then(Value::as_str) == Some("generated") {
            continue;
        }
        let code = detect_code(e.get("file_name").and_then(Value::as_str).unwrap_or(""));
        let Some(code) = code else { continue };
        checked += 1;
        let rel = e.get("path").and_then(Value::as_str).unwrap_or("").trim_matches('/').to_string();
        let mut found = false;
        for root in &roots {
            if !rel.is_empty() {
                let dir = format!("{root}/{rel}");
                for it in client.list_abs(lf, &dir).await {
                    let name = json_str(&it, "name");
                    if !json_bool(&it, "is_dir", false)
                        && name.to_lowercase().ends_with(".strm")
                        && name.to_uppercase().starts_with(&code)
                    {
                        found = true;
                        break;
                    }
                }
            }
            if found {
                break;
            }
            if search_strm(&client, lf, root, &code, 300).await.is_some() {
                found = true;
                break;
            }
        }
        if found {
            verified += 1;
            if let Some(id) = e.get("id").and_then(Value::as_i64) {
                let _ = st.pending.set_status(id, "generated", "");
            }
        }
    }
    Ok(json!({"ok": true, "checked": checked, "verified": verified}))
}

async fn search_strm(client: &crate::litepan::LitePanClient, account_id: i64, root: &str, code: &str, max_dirs: usize) -> Option<String> {
    let mut queue = vec![root.to_string()];
    let mut visited = 0;
    while let Some(cur) = queue.pop() {
        if visited >= max_dirs {
            break;
        }
        visited += 1;
        for it in client.list_abs(account_id, &cur).await {
            let name = json_str(&it, "name");
            if json_bool(&it, "is_dir", false) {
                queue.push(format!("{cur}/{name}"));
            } else if name.to_lowercase().ends_with(".strm") && name.to_uppercase().starts_with(code) {
                return Some(format!("{cur}/{name}"));
            }
        }
    }
    None
}

async fn pending_dismiss(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let id = v.get("id").and_then(Value::as_i64).unwrap_or(0);
    if id <= 0 {
        return Err(bad("缺少 id"));
    }
    let deleted = st.pending.delete(id).map_err(|e| internal(e))?;
    Ok(ok(json!({"ok": true, "deleted": deleted, "summary": st.pending.summary()})))
}

// ---------------------------------------------------------------- litepan
async fn litepan_creds_get(State(st): State<Arc<AppState>>) -> Response {
    let (url, user, pwd) = st.pool.creds();
    ok(json!({
        "ok": true, "url": url, "username": user, "has_password": !pwd.is_empty(),
        "saved": !st.state.get_str("litepan_password", "").is_empty(),
        "insecure": st.pool.insecure(), "env_url": st.cfg.litepan_base(),
    }))
}

async fn litepan_creds_post(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let raw_url = v.get("url").or_else(|| v.get("litepan_url")).and_then(Value::as_str).unwrap_or("").trim().trim_end_matches('/').to_string();
    let user = v.get("username").and_then(Value::as_str).unwrap_or("").trim();
    let user = if user.is_empty() { "admin" } else { user };
    let pwd = v.get("password").and_then(Value::as_str).unwrap_or("");
    if pwd.is_empty() {
        return Err(bad("请填写 LitePan 密码"));
    }
    let insecure = v.get("insecure").map(|x| bool_from(Some(x), false)).unwrap_or_else(|| st.pool.insecure());
    let candidates: Vec<String> = if raw_url.is_empty() {
        vec![
            "http://host.docker.internal:5211".into(),
            "http://127.0.0.1:5211".into(),
            "http://172.17.0.1:5211".into(),
        ]
    } else {
        vec![raw_url.to_string()]
    };
    let mut last_err = String::new();
    let mut chosen: Option<(String, Vec<Value>)> = None;
    for cand in &candidates {
        match crate::litepan::LitePanClient::new(cand, user, pwd, &st.cfg.state_dir, !insecure) {
            Ok(client) => match client.login(true).await {
                Ok(_) => match client.accounts().await {
                    Ok(accounts) => {
                        chosen = Some((cand.clone(), accounts));
                        break;
                    }
                    Err(e) => last_err = e.to_string(),
                },
                Err(e) => last_err = e.to_string(),
            },
            Err(e) => last_err = e.to_string(),
        }
    }
    let Some((url, accounts)) = chosen else {
        return Ok(ok_status(
            StatusCode::BAD_REQUEST,
            json!({"ok": false, "error": format!("连接失败:{last_err}")}),
        ));
    };
    st.state.set_litepan_creds(&url, user, pwd, Some(insecure));
    st.pool.clear();
    Ok(ok(json!({
        "ok": true, "url": url, "username": user, "has_password": true,
        "saved": true, "insecure": insecure, "accounts": accounts.len(),
    })))
}

async fn litepan_status(State(st): State<Arc<AppState>>) -> Response {
    let (url, user, pwd) = st.pool.creds();
    if pwd.is_empty() {
        return ok(json!({"ok": true, "connected": false, "saved": false, "url": url, "error": "尚未保存 LitePan 密码"}));
    }
    match crate::litepan::LitePanClient::new(&url, &user, &pwd, &st.cfg.state_dir, !st.pool.insecure()) {
        Ok(client) => match client.login(false).await {
            Ok(_) => match client.accounts().await {
                Ok(accounts) => ok(json!({"ok": true, "connected": true, "saved": true, "url": url, "accounts": accounts.len()})),
                Err(e) => ok(json!({"ok": true, "connected": false, "saved": true, "url": url, "error": e.to_string()})),
            },
            Err(e) => ok(json!({"ok": true, "connected": false, "saved": true, "url": url, "error": e.to_string()})),
        },
        Err(e) => ok(json!({"ok": true, "connected": false, "saved": true, "url": url, "error": e.to_string()})),
    }
}

async fn lp_accounts(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let accounts = client.accounts().await.map_err(|e| bad(e.to_string()))?;
    let out: Vec<Value> = accounts
        .iter()
        .map(|a| json!({
            "id": a.get("id").and_then(Value::as_i64).unwrap_or(0),
            "name": a.get("name").or_else(|| a.get("nickname")).and_then(Value::as_str).unwrap_or(""),
            "driver": a.get("driver").or_else(|| a.get("type")).and_then(Value::as_str).unwrap_or(""),
        }))
        .collect();
    Ok(ok(json!({"ok": true, "accounts": out})))
}

async fn lp_dirs(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let mut account_id = v.get("account_id").and_then(Value::as_i64).unwrap_or(0);
    if account_id == 0 {
        account_id = client.pick_account(None).await.map_err(|e| bad(e.to_string()))?.get("id").and_then(Value::as_i64).unwrap_or(0);
    }
    let depth = v.get("depth").and_then(Value::as_i64).unwrap_or(3).clamp(1, 5) as usize;
    let max_dirs = v.get("max_dirs").and_then(Value::as_i64).unwrap_or(500).clamp(10, 2000) as usize;
    let refresh = v.get("refresh").map(|x| bool_from(Some(x), false)).unwrap_or(false);
    let mut roots: Vec<String> = v
        .get("roots")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(|s| s.trim_matches('/').to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    if roots.is_empty() {
        if let Some(r) = v.get("root").and_then(Value::as_str).map(|s| s.trim_matches('/')).filter(|s| !s.is_empty()) {
            roots.push(r.trim_matches('/').to_string());
        }
    }
    if roots.is_empty() {
        if let Ok(tasks) = client.strm_tasks().await {
            for t in tasks {
                let p = t.get("path").and_then(Value::as_str).unwrap_or("").trim_matches('/');
                if !p.is_empty() && !roots.contains(&p.to_string()) {
                    roots.push(p.to_string());
                }
            }
        }
    }
    if roots.is_empty() {
        // 没有 STRM 任务时，退回设置里的演员/缓存根，避免脚本“目录树为空”
        if let Some(arr) = st.state.get_setting("actor_roots", json!([])).as_array() {
            for r in arr {
                if let Some(p) = r.get("path").and_then(Value::as_str).map(|s| s.trim_matches('/')).filter(|s| !s.is_empty()) {
                    roots.push(p.trim_matches('/').to_string());
                }
            }
        }
    }
    // 优先用已加载的 fileindex，毫秒级返回；refresh=true 或索引未覆盖时才实时 BFS。
    if !refresh {
        st.fileindex.ensure_known();
        let idx_dirs = st.fileindex.dirs(account_id, &roots, depth, max_dirs);
        if !idx_dirs.is_empty() {
            return Ok(ok(json!({"ok": true, "roots": roots, "dirs": idx_dirs, "scanned": 0, "cached": true})));
        }
    }
    let mut dirs = Vec::new();
    let mut visited = 0usize;
    for root in &roots {
        let rp = format!("/{}", root.trim_matches('/'));
        let Ok(resolved) = client.resolve_dir(account_id, &rp, false).await else {
            continue;
        };
        let start_id = resolved.get("parent_id").and_then(Value::as_str).unwrap_or("").to_string();
        let mut queue: Vec<(String, String, usize)> = vec![(start_id, rp.trim_end_matches('/').to_string(), 0)];
        let mut head = 0usize;
        while head < queue.len() && dirs.len() < max_dirs && visited < max_dirs * 2 {
            let (pid, cur, d) = queue[head].clone();
            head += 1;
            if d >= depth || pid.is_empty() {
                continue;
            }
            visited += 1;
            let Ok(listed) = client.list_dir(account_id, &pid, "", None).await else {
                continue;
            };
            let items = listed.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            for it in items {
                if !json_bool(&it, "is_dir", false) {
                    continue;
                }
                let name = json_str(&it, "name");
                let id = json_str(&it, "id");
                if name.is_empty() {
                    continue;
                }
                let full = format!("{}/{}", cur.trim_end_matches('/'), name).trim_matches('/').to_string();
                dirs.push(json!({"path": full, "name": name}));
                if !id.is_empty() {
                    queue.push((id, format!("/{full}"), d + 1));
                }
                if dirs.len() >= max_dirs {
                    break;
                }
            }
        }
    }
    Ok(ok(json!({"ok": true, "roots": roots, "dirs": dirs, "scanned": visited})))
}

async fn lp_organize(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let account_id = json_i64(&v, "account_id", 0);
    let path = json_str(&v, "path");
    let code = detect_code(&json_str(&v, "code")).unwrap_or_else(|| json_str(&v, "code"));
    if path.is_empty() || code.is_empty() {
        return Err(bad("缺少 path/code"));
    }
    let resolved = client.resolve_dir(account_id, &path, false).await.map_err(|e| bad(e.to_string()))?;
    let pid = resolved.get("parent_id").and_then(Value::as_str).unwrap_or("").to_string();
    let items = client.list_dir(account_id, &pid, "", None).await.map_err(|e| bad(e.to_string()))?.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(ok(json!({"ok": true, "path": path, "code": code, "items": items.len(), "plan": {"keep": [], "junk": [], "rename": []}})))
}

async fn lp_list(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    if !ark_guard(&st, &headers, &v) {
        return Ok(ok_status(StatusCode::UNAUTHORIZED, json!({"ok": false, "ark": true, "login_required": true, "error": "请先登录 Ark-push 或提供正确的 X-Ark-Key"})));
    }
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let mut account_id = v.get("account_id").and_then(Value::as_i64).unwrap_or(0);
    if account_id == 0 {
        account_id = client.pick_account(None).await.map_err(|e| bad(e.to_string()))?.get("id").and_then(Value::as_i64).unwrap_or(0);
    }
    let parent_id = v.get("parent_id").and_then(Value::as_str).unwrap_or("");
    let path = v.get("path").and_then(Value::as_str).unwrap_or("");
    let data = client.list_dir(account_id, parent_id, path, None).await.map_err(|e| bad(e.to_string()))?;
    if let Some(items) = data.get("items").and_then(Value::as_array) {
        let base = path.trim_matches('/');
        let rows: Vec<Value> = items.iter().filter_map(|it| {
            let name = it.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() { return None; }
            Some(json!({
                "id": json_str(it, "id"), "name": name,
                "path": if base.is_empty() { name.to_string() } else { format!("{base}/{name}") },
                "is_dir": json_bool(it, "is_dir", false), "size": json_i64(it, "size", 0),
                "account_id": account_id,
            }))
        }).collect();
        st.fileindex.upsert(account_id, &rows);
    }
    Ok(ok(json!({"ok": true, "account_id": account_id, "parent_id": data.get("parent_id").cloned().unwrap_or(json!("")), "items": data.get("items").cloned().unwrap_or(json!([]))})))
}

/// 列出某个目录（带 TTL 缓存）。返回 (目录内名字集合, 原始 items)。
/// Err("notfound") 表示目录已不存在；其它 Err 表示网络/接口异常。
async fn dir_listing(
    cache: &DirCache,
    client: &crate::litepan::LitePanClient,
    account_id: i64,
    parent: &str,
    known_id: Option<&str>,
) -> Result<(Arc<HashSet<String>>, Arc<Vec<Value>>), String> {
    if let Some(cached) = cache.get(account_id, parent) {
        return Ok(cached);
    }
    let pid = match known_id {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => match client.resolve_dir(account_id, parent, false).await {
            Ok(v) => v
                .get("parent_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            Err(crate::litepan::LitePanError::NotFound(_)) => return Err("notfound".into()),
            Err(e) => return Err(e.to_string()),
        },
    };
    if pid.is_empty() {
        return Err("notfound".into());
    }
    match client.list_dir(account_id, &pid, "", None).await {
        Ok(listed) => {
            let rows: Vec<Value> = listed
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let names: HashSet<String> = rows.iter().map(|x| json_str(x, "name")).collect();
            let names = Arc::new(names);
            let rows = Arc::new(rows);
            cache.put(account_id, parent, names.clone(), rows.clone());
            Ok((names, rows))
        }
        Err(crate::litepan::LitePanError::NotFound(_)) => Err("notfound".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// 索引里的目录 path -> id，省掉 resolve_dir 往返。
fn dir_id_map(fileindex: &FileIndex) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for d in fileindex.all_dirs_cached().iter() {
        let p = json_str(&d, "path").trim_matches('/').to_string();
        let id = json_str(&d, "id");
        if !p.is_empty() && !id.is_empty() {
            map.insert(p, id);
        }
    }
    map
}

/// 按父目录批量校验搜索候选：同一目录只列一次、并发请求、带 TTL 缓存。
async fn validate_index_hits(
    cache: Arc<DirCache>,
    client: Arc<crate::litepan::LitePanClient>,
    fileindex: Arc<FileIndex>,
    account_id: i64,
    items: Vec<Value>,
) -> (Vec<Value>, HashSet<String>) {
    let mut by_parent: HashMap<String, Vec<Value>> = HashMap::new();
    let mut valid: Vec<Value> = Vec::new();
    for it in items {
        let path = json_str(&it, "path").trim_matches('/').to_string();
        if path.is_empty() {
            continue;
        }
        match path.rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => {
                by_parent.entry(parent.to_string()).or_default().push(it);
            }
            // 根目录下的条目没有可列出的父目录，直接保留。
            _ => valid.push(it),
        }
    }
    if by_parent.is_empty() {
        return (valid, HashSet::new());
    }
    let dir_ids = dir_id_map(&fileindex);
    let parents: Vec<String> = by_parent.keys().cloned().collect();
    let results: Vec<(String, Result<(Arc<HashSet<String>>, Arc<Vec<Value>>), String>)> =
        futures::stream::iter(parents.into_iter().map(|parent| {
            let cache = cache.clone();
            let client = client.clone();
            let known = dir_ids.get(&parent).cloned();
            async move {
                let res = dir_listing(&cache, &client, account_id, &parent, known.as_deref()).await;
                (parent, res)
            }
        }))
        .buffer_unordered(8)
        .collect()
        .await;

    let mut stale = HashSet::new();
    for (parent, res) in results {
        let group = by_parent.remove(&parent).unwrap_or_default();
        match res {
            Ok((names, _)) => {
                for it in group {
                    if names.contains(&json_str(&it, "name")) {
                        valid.push(it);
                    } else {
                        stale.insert(json_str(&it, "path").trim_matches('/').to_string());
                    }
                }
            }
            Err(err) if err == "notfound" => {
                for it in group {
                    stale.insert(json_str(&it, "path").trim_matches('/').to_string());
                }
            }
            // 网络异常时保守保留，避免把可用结果误删。
            Err(_) => valid.extend(group),
        }
    }
    (valid, stale)
}

/// 按父目录批量校验目录是否仍然存在（复用搜索的并发 + TTL 缓存）。
async fn filter_existing_dirs(
    cache: Arc<DirCache>,
    client: Arc<crate::litepan::LitePanClient>,
    fileindex: Arc<FileIndex>,
    account_id: i64,
    paths: Vec<String>,
) -> Vec<String> {
    let items: Vec<Value> = paths
        .into_iter()
        .map(|p| {
            let name = p.rsplit('/').next().unwrap_or("").to_string();
            json!({"path": p, "name": name, "is_dir": true})
        })
        .collect();
    let (valid, _) = validate_index_hits(cache, client, fileindex, account_id, items).await;
    valid
        .iter()
        .map(|v| json_str(v, "path").trim_matches('/').to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

async fn lp_search(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let keyword = json_str(&v, "keyword");
    if keyword.is_empty() {
        return Err(bad("请填写搜索关键词"));
    }
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let accounts = client.accounts().await.map_err(|e| bad(e.to_string()))?;
    let want = v.get("account_id").and_then(Value::as_i64).unwrap_or(0);
    let mut out = Vec::new();
    let mut errors = Vec::new();
    let mut fallback = Vec::new();
    let cur_path = v.get("path").and_then(Value::as_str).unwrap_or("").trim_matches('/').to_string();
    let cur_parent = v.get("parent_id").and_then(Value::as_str).unwrap_or("").to_string();
    let mut seen: HashSet<(i64, String)> = HashSet::new();
    for a in accounts {
        let aid = a.get("id").and_then(Value::as_i64).unwrap_or(0);
        if want > 0 && aid != want {
            continue;
        }
        let label = a.get("name").and_then(Value::as_str).unwrap_or("");

        // 用户就在某个目录里搜索时，先查当前目录的实时列表。
        // 这样刚下载、还没进 fileindex 的文件也能立刻搜到。
        if !cur_path.is_empty() || !cur_parent.is_empty() {
            let pid = if !cur_parent.is_empty() {
                cur_parent.clone()
            } else {
                match client.resolve_dir(aid, &cur_path, false).await {
                    Ok(r) => r.get("parent_id").and_then(Value::as_str).unwrap_or("").to_string(),
                    Err(_) => String::new(),
                }
            };
            if !pid.is_empty() || cur_path.is_empty() {
                if let Ok(listed) = client.list_dir(aid, &pid, &cur_path, None).await {
                    let rows = listed.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
                    let mut upsert_rows = Vec::new();
                    for it in rows {
                        let name = json_str(&it, "name");
                        if name.is_empty() {
                            continue;
                        }
                        let full = if cur_path.is_empty() {
                            name.clone()
                        } else {
                            format!("{cur_path}/{name}")
                        };
                        upsert_rows.push(json!({
                            "id": json_str(&it, "id"), "name": name,
                            "path": full.clone(), "is_dir": json_bool(&it, "is_dir", false),
                            "size": json_i64(&it, "size", 0), "account_id": aid,
                        }));
                        if !match_keyword(&keyword, &name) && !match_keyword(&keyword, &full) {
                            continue;
                        }
                        if !seen.insert((aid, full.clone())) {
                            continue;
                        }
                        out.push(json!({
                            "id": json_str(&it, "id"), "name": name, "path": full,
                            "is_dir": json_bool(&it, "is_dir", false),
                            "size": json_i64(&it, "size", 0),
                            "account_id": aid, "account_label": label,
                        }));
                    }
                    if !upsert_rows.is_empty() {
                        let _ = st.fileindex.upsert(aid, &upsert_rows);
                    }
                }
            }
        }

        match client.net_search(&keyword, aid, 50, false, Some(3)).await {
            Ok(items) => {
                for mut it in items {
                    let name = it.get("name").and_then(Value::as_str).unwrap_or("");
                    let path = it.get("path").and_then(Value::as_str).unwrap_or("");
                    // LitePan 的 net-search 插件在部分驱动/中文关键词下会返回整目录列表；
                    // 这里必须再按关键词过滤，否则会把无关演员文件夹全部展示出来。
                    if !match_keyword(&keyword, name) && !match_keyword(&keyword, path) {
                        continue;
                    }
                    let full = path.trim_matches('/').to_string();
                    if !seen.insert((aid, full)) {
                        continue;
                    }
                    it["account_id"] = json!(aid);
                    it["account_label"] = json!(label);
                    out.push(it);
                }
            }
            Err(e) => {
                // 先取本地索引，再按父目录批量实时校验：
                // 同一目录只列一次、8 路并发、120 秒 TTL 缓存，避免几十次串行 HTTP 请求。
                let mut candidates = Vec::new();
                for it in st.fileindex.search(&keyword, 50, aid) {
                    let name = json_str(&it, "name");
                    let path = json_str(&it, "path");
                    if !match_keyword(&keyword, &name) && !match_keyword(&keyword, &path) {
                        continue;
                    }
                    candidates.push(it);
                }
                let (valid, stale) = validate_index_hits(
                    st.dir_cache.clone(),
                    client.clone(),
                    st.fileindex.clone(),
                    aid,
                    candidates,
                )
                .await;
                let mut got = 0usize;
                for mut it in valid {
                    let path = json_str(&it, "path").trim_matches('/').to_string();
                    if !seen.insert((aid, path)) {
                        continue;
                    }
                    it["account_label"] = json!(label);
                    out.push(it);
                    got += 1;
                }
                // 本地索引没有收录新文件时，用同番号前缀找到兄弟文件所在目录，
                // 再实时列一次该目录。这样 115 里刚下载的 START-663-LTX.mp4
                // 不需要全盘扫描也能被搜索到。
                if got == 0 {
                    let prefix = keyword
                        .split(['-', '_', ' ', '.'])
                        .next()
                        .unwrap_or(keyword.as_str())
                        .trim();
                    if prefix.chars().count() >= 3 {
                        let hit = st.search.search(&json!({"q": prefix, "limit": 100}));
                        let mut parents: Vec<String> = Vec::new();
                        if let Some(hits) = hit.get("hits").and_then(Value::as_array) {
                            for h in hits {
                                if let Some(path) = h.get("path").and_then(Value::as_str) {
                                    if let Some((parent, _)) = path.rsplit_once('/') {
                                        let parent = parent.trim_matches('/').to_string();
                                        if !parent.is_empty() && !parents.contains(&parent) {
                                            parents.push(parent);
                                        }
                                    }
                                }
                            }
                        }
                        let dir_ids = dir_id_map(&st.fileindex);
                        // 最多补搜 8 个目录，6 路并发；配合目录缓存，通常 1 秒内完成。
                        let listed: Vec<(String, Vec<Value>)> = futures::stream::iter(
                            parents.into_iter().take(8).map(|parent| {
                                let cache = st.dir_cache.clone();
                                let client = client.clone();
                                let known = dir_ids.get(&parent).cloned();
                                async move {
                                    match dir_listing(&cache, &client, aid, &parent, known.as_deref()).await {
                                        Ok((_, rows)) => (parent, rows.as_ref().clone()),
                                        Err(_) => (parent, Vec::new()),
                                    }
                                }
                            }),
                        )
                        .buffer_unordered(6)
                        .collect()
                        .await;
                        let mut upsert_rows = Vec::new();
                        for (parent, rows) in listed {
                            for it in rows {
                                let name = json_str(&it, "name");
                                if name.is_empty() {
                                    continue;
                                }
                                let full = format!("{parent}/{name}").trim_matches('/').to_string();
                                upsert_rows.push(json!({
                                    "id": json_str(&it, "id"), "name": name, "path": full.clone(),
                                    "is_dir": json_bool(&it, "is_dir", false),
                                    "size": json_i64(&it, "size", 0), "account_id": aid,
                                }));
                                if !match_keyword(&keyword, &name) && !match_keyword(&keyword, &full) {
                                    continue;
                                }
                                if !seen.insert((aid, full.clone())) {
                                    continue;
                                }
                                out.push(json!({
                                    "id": json_str(&it, "id"), "name": name, "path": full,
                                    "is_dir": json_bool(&it, "is_dir", false),
                                    "size": json_i64(&it, "size", 0),
                                    "account_id": aid, "account_label": label,
                                }));
                                got += 1;
                                if got >= 20 {
                                    break;
                                }
                            }
                            if got >= 20 {
                                break;
                            }
                        }
                        if !upsert_rows.is_empty() {
                            let _ = st.fileindex.upsert(aid, &upsert_rows);
                        }
                    }
                }
                if !stale.is_empty() {
                    let removed = st.fileindex.remove_paths(aid, &stale);
                    tracing::info!("搜索清理失效索引 account={} removed={}", aid, removed);
                }
                fallback.push(label.to_string());
                // 插件 404/不可用时，如果当前目录或本地索引已经命中，就不再向界面抛错误提示。
                let has_any = got > 0
                    || out.iter().any(|x| x.get("account_id").and_then(Value::as_i64) == Some(aid));
                if !has_any {
                    let es = e.to_string();
                    // net-search 插件 404/未安装是常态，直接用本地索引即可，不向 UI 抛“搜索失败”。
                    if !es.contains("404") && !es.to_lowercase().contains("not found") {
                        errors.push(format!("{label}: {es}"));
                    }
                }
            }
        }
    }
    Ok(ok(json!({"ok": true, "keyword": keyword, "items": out, "errors": errors, "fallback": fallback})))
}

async fn lp_offline_tasks(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = if body.is_empty() { json!({}) } else { parse_body(&body)? };
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let tasks = client.offline_tasks().await.map_err(|e| bad(e.to_string()))?;
    Ok(ok(json!({"ok": true, "tasks": tasks})))
}

async fn lp_offline_tasks_delete(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let ids = v.get("ids").or_else(|| v.get("task_ids")).cloned().unwrap_or(json!([]));
    let res = client
        .request(Method::POST, "/api/files/offline-download/tasks/delete", None, Some(json!({"ids": ids})))
        .await
        .map_err(|e| bad(e.to_string()))?;
    Ok(ok(json!({"ok": true, "result": res})))
}

async fn lp_strm_status(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let dirs: Vec<String> = v.get("dirs").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    let items = v.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let res = client
        .directory_strm_status(json_i64(&v, "account_id", 0), v.get("parent_id").and_then(Value::as_str).unwrap_or(""), v.get("path").and_then(Value::as_str).unwrap_or(""), &dirs, &items)
        .await
        .map_err(|e| bad(e.to_string()))?;
    Ok(ok(json!({"ok": true, "status": res})))
}

async fn lp_strm_generate(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let items = v.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    if items.is_empty() {
        return Err(bad("items 为空,已拒绝下发:空清单会让 LitePan 清理该目录已生成的 STRM。"));
    }
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let dirs: Vec<String> = v.get("dirs").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    let res = client
        .generate_directory_strm(json_i64(&v, "account_id", 0), v.get("parent_id").and_then(Value::as_str).unwrap_or(""), v.get("path").and_then(Value::as_str).unwrap_or(""), &dirs, &items)
        .await
        .map_err(|e| bad(e.to_string()))?;
    Ok(ok(json!({"ok": true, "result": res, "index_refresh": "queued"})))
}

async fn lp_delete(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let ids = string_array(&v, "file_ids");
    if ids.is_empty() {
        return Err(bad("缺少 file_ids"));
    }
    let res = client
        .delete_files(json_i64(&v, "account_id", 0), &ids, v.get("parent_id").and_then(Value::as_str).unwrap_or(""))
        .await
        .map_err(|e| bad(e.to_string()))?;
    st.dir_cache.clear();
    Ok(ok(json!({"ok": true, "result": res, "deleted": ids.len()})))
}

async fn lp_move(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let ids = string_array(&v, "file_ids");
    if ids.is_empty() {
        return Err(bad("缺少 file_ids"));
    }
    let res = client
        .move_files(json_i64(&v, "account_id", 0), &ids, v.get("target_parent_id").and_then(Value::as_str).unwrap_or(""), v.get("parent_id").and_then(Value::as_str).unwrap_or(""))
        .await
        .map_err(|e| bad(e.to_string()))?;
    st.dir_cache.clear();
    Ok(ok(json!({"ok": true, "result": res, "moved": ids.len()})))
}

async fn lp_copy(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let ids = string_array(&v, "file_ids");
    if ids.is_empty() {
        return Err(bad("缺少 file_ids"));
    }
    let res = client
        .copy_files(json_i64(&v, "account_id", 0), &ids, v.get("target_parent_id").and_then(Value::as_str).unwrap_or(""), v.get("parent_id").and_then(Value::as_str).unwrap_or(""))
        .await
        .map_err(|e| bad(e.to_string()))?;
    st.dir_cache.clear();
    Ok(ok(json!({"ok": true, "result": res, "copied": ids.len()})))
}

async fn lp_rename(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let file_id = json_str(&v, "file_id");
    let new_name = json_str(&v, "new_name");
    if file_id.is_empty() || new_name.is_empty() {
        return Err(bad("缺少 file_id 或 new_name"));
    }
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let res = client
        .rename_file(json_i64(&v, "account_id", 0), &file_id, &new_name, v.get("parent_id").and_then(Value::as_str).unwrap_or(""))
        .await
        .map_err(|e| bad(e.to_string()))?;
    st.dir_cache.clear();
    Ok(ok(json!({"ok": true, "result": res, "new_name": new_name})))
}

async fn lp_create_folder(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let name = json_str(&v, "name");
    if name.is_empty() {
        return Err(bad("缺少 name"));
    }
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let res = client
        .create_folder(json_i64(&v, "account_id", 0), v.get("parent_id").and_then(Value::as_str).unwrap_or(""), &name)
        .await
        .map_err(|e| bad(e.to_string()))?;
    Ok(ok(json!({"ok": true, "result": res, "name": name})))
}

async fn lp_create_file(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let name = json_str(&v, "name");
    if name.is_empty() {
        return Err(bad("缺少 name"));
    }
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let path = v.get("path").and_then(Value::as_str).unwrap_or("/");
    let res = client
        .upload_file(json_i64(&v, "account_id", 0), path, &name, Vec::new())
        .await
        .map_err(|e| bad(e.to_string()))?;
    Ok(ok(json!({"ok": true, "result": res, "name": name})))
}

fn string_array(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .or_else(|| v.get(key).and_then(Value::as_str).map(|s| vec![s.to_string()]))
        .unwrap_or_default()
}

async fn lp_play(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let account_id = json_i64(&v, "account_id", 0);
    let file_id = json_str(&v, "file_id");
    let name = json_str(&v, "name");
    if file_id.is_empty() {
        return Err(bad("缺少 file_id"));
    }
    if name.to_lowercase().ends_with(".strm") {
        let raw = client.download_file(account_id, &file_id, &name, 65536).await.map_err(|e| bad(e.to_string()))?;
        let text = String::from_utf8_lossy(&raw);
        let url = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok(ok(json!({"ok": false, "error": format!("strm 内容不是可播放的直链:{url}")})));
        }
        return Ok(ok(json!({"ok": true, "kind": "redirect", "url": url, "name": name})));
    }
    if indexer::is_video(&name) {
        let qs = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("account_id", &account_id.to_string())
            .append_pair("file_id", &file_id)
            .append_pair("file_name", &name)
            .append_pair("litepan_url", v.get("litepan_url").and_then(Value::as_str).unwrap_or(""))
            .append_pair("username", v.get("username").and_then(Value::as_str).unwrap_or(""))
            .append_pair("password", v.get("password").and_then(Value::as_str).unwrap_or(""))
            .finish();
        return Ok(ok(json!({"ok": true, "kind": "proxy", "url": format!("/api/play/stream?{qs}"), "name": name})));
    }
    Ok(ok(json!({"ok": false, "error": "不是视频或 strm 文件,无法播放"})))
}

async fn lp_open(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let account_id = json_i64(&v, "account_id", 0);
    let file_id = json_str(&v, "file_id");
    let name = json_str(&v, "name");
    if file_id.is_empty() {
        return Err(bad("缺少 file_id"));
    }
    let qs = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("account_id", &account_id.to_string())
        .append_pair("file_id", &file_id)
        .append_pair("file_name", &name)
        .append_pair("litepan_url", v.get("litepan_url").and_then(Value::as_str).unwrap_or(""))
        .append_pair("username", v.get("username").and_then(Value::as_str).unwrap_or(""))
        .append_pair("password", v.get("password").and_then(Value::as_str).unwrap_or(""))
        .finish();
    Ok(ok(json!({"ok": true, "kind": "proxy", "url": format!("/api/play/stream?{qs}"), "name": name})))
}

async fn play_stream(State(st): State<Arc<AppState>>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    let payload = json!({
        "litepan_url": q.get("litepan_url").cloned().unwrap_or_default(),
        "username": q.get("username").cloned().unwrap_or_default(),
        "password": q.get("password").cloned().unwrap_or_default(),
    });
    let client = match st.pool.client_for(&payload).await {
        Ok(c) => c,
        Err(e) => return ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    let account_id = q.get("account_id").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let file_id = q.get("file_id").cloned().unwrap_or_default();
    let file_name = q.get("file_name").cloned().unwrap_or_default();
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let upstream = match client.open_stream(account_id, &file_id, &file_name, range).await {
        Ok(r) => r,
        Err(e) => return ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::OK);
    if !upstream.status().is_success() {
        tracing::warn!(
            "play_stream upstream {} account={} file={}",
            upstream.status(),
            account_id,
            file_id
        );
    }
    let mut builder = Response::builder().status(status);
    let mut ctype = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| "application/octet-stream".into());
    // 115 CDN 常返回 application/octet-stream，按文件名补成 video/mp4 等，浏览器才会当视频播。
    if (ctype.is_empty() || ctype.starts_with("application/octet-stream")) && !file_name.is_empty() {
        if let Some(m) = mime_guess::from_path(&file_name).first_raw() {
            ctype = m.to_string();
        }
    }
    builder = builder.header(header::CONTENT_TYPE, ctype);
    builder = builder.header(header::CONTENT_DISPOSITION, "inline");
    for key in [header::CONTENT_LENGTH, header::CONTENT_RANGE] {
        if let Some(v) = upstream.headers().get(&key) {
            if let Ok(s) = v.to_str() {
                builder = builder.header(key, s);
            }
        }
    }
    if upstream.status() == reqwest::StatusCode::OK {
        builder = builder.header(header::ACCEPT_RANGES, "bytes");
    }
    let stream = upstream.bytes_stream().map(|r| r.map_err(|e| std::io::Error::other(e.to_string())));
    builder.body(Body::from_stream(stream)).unwrap_or_else(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

// ---------------------------------------------------------------- match / actors
async fn match_config_get(State(st): State<Arc<AppState>>) -> Response {
    ok(json!({"ok": true, "match": st.state.match_config()}))
}

async fn match_config_post(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let Some(match_cfg) = v.get("match") else {
        return Err(bad("缺少 match 配置"));
    };
    if !match_cfg.is_object() {
        return Err(bad("缺少 match 配置"));
    }
    let raw = serde_json::to_vec(match_cfg).map_err(|_| bad("match 配置无法序列化"))?;
    if raw.len() > 256 * 1024 {
        return Ok(ok_status(StatusCode::PAYLOAD_TOO_LARGE, json!({"ok": false, "error": "match 配置过大(上限 256KB)"})));
    }
    st.state.set_match_config(match_cfg.clone());
    Ok(ok(json!({"ok": true, "saved": true})))
}

async fn match_dir(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let code = v.get("code").or_else(|| v.get("q")).and_then(Value::as_str).unwrap_or("").trim();
    if code.is_empty() {
        return Err(bad("缺少 code"));
    }
    let key: String = code.to_uppercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if key.is_empty() {
        return Ok(ok(json!({"ok": true, "matched": false, "dir": "", "dirs": [], "actor": "", "actors": [], "hits": 0})));
    }
    st.fileindex.ensure_known();
    let mut dirs = Vec::new();
    let mut actors = Vec::new();
    for it in st.fileindex.search(code, 200, 0) {
        let name = json_str(&it, "name");
        let path = json_str(&it, "path");
        if !normalize_key(&[&name, &path]).contains(&key) {
            continue;
        }
        let d = if json_bool(&it, "is_dir", false) {
            path.clone()
        } else {
            path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default()
        };
        if !d.is_empty() && !dirs.contains(&d) {
            dirs.push(d);
        }
        if dirs.len() >= 8 {
            break;
        }
    }
    // fileindex 只增不删：删除/改名后的旧目录必须实时核对，否则脚本会显示两个候选。
    if !dirs.is_empty() {
        if let Ok(client) = st.pool.client_for(&Value::Null).await {
            let account_id = client
                .pick_account(v.get("account_id").and_then(Value::as_i64))
                .await
                .ok()
                .and_then(|a| a.get("id").and_then(Value::as_i64))
                .unwrap_or(0);
            dirs = filter_existing_dirs(
                st.dir_cache.clone(),
                client,
                st.fileindex.clone(),
                account_id,
                dirs,
            )
            .await;
        }
    }
    if let Some(d) = dirs.first() {
        let a = actor_from_path(d);
        if !a.is_empty() {
            actors.push(a);
        }
    }
    Ok(ok(json!({
        "ok": true, "matched": !dirs.is_empty(),
        "dir": dirs.first().cloned().unwrap_or_default(),
        "dirs": dirs, "actor": actors.first().cloned().unwrap_or_default(),
        "actors": actors, "hits": 0, "source": if dirs.is_empty() { "" } else { "fileindex" },
    })))
}

#[axum::debug_handler]
async fn actor_info(State(st): State<Arc<AppState>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let name = q.get("name").cloned().unwrap_or_default();
    if name.is_empty() {
        return ok_status(StatusCode::BAD_REQUEST, json!({"ok": false, "error": "缺少 name"}));
    }
    let mut info = st.actors.get(&name).await;
    let ban = st.actors.banned();
    if ban.get("banned").and_then(Value::as_bool).unwrap_or(false) {
        info["javxy"] = ban;
    }
    ok(info)
}

async fn actor_profiles(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let names: Vec<String> = v.get("names").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    let (profiles, ban) = st.actors.profiles(&names).await;
    let mut payload = json!({"ok": true, "profiles": profiles});
    if ban.get("banned").and_then(Value::as_bool).unwrap_or(false) {
        payload["javxy"] = ban.clone();
        payload["note"] = ban.get("note").cloned().unwrap_or(json!(""));
    }
    Ok(ok(payload))
}

async fn actor_by_code(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    st.fileindex.ensure_known();
    let code = json_str(&v, "code");
    let key: String = code.to_uppercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if key.len() < 4 {
        return Ok(ok(json!({"ok": true, "actors": [], "title": ""})));
    }
    let mut actors = Vec::new();
    for it in st.fileindex.search(&code, 100, 0) {
        let name = json_str(&it, "name");
        if !normalize_key(&[&name]).contains(&key) {
            continue;
        }
        let dir = json_str(&it, "path");
        let a = actor_from_path(&dir);
        if !a.is_empty() && !actors.contains(&a) {
            actors.push(a);
        }
    }
    Ok(ok(json!({"ok": true, "code": code, "actors": actors, "title": "", "source": "fileindex"})))
}

fn lcs_len(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let mut prev = vec![0usize; b.len() + 1];
    let mut best = 0usize;
    for i in 1..=a.len() {
        let mut cur = vec![0usize; b.len() + 1];
        for j in 1..=b.len() {
            if a[i - 1] == b[j - 1] {
                cur[j] = prev[j - 1] + 1;
                best = best.max(cur[j]);
            }
        }
        prev = cur;
    }
    best
}

/// 演员名与目录名匹配：精确/包含，外加繁简、异体、日文写法的最长公共子串兜底。
/// 与油猴脚本的 actorNameHit 对齐，避免 “Z/ZZ” 这种单字母目录被当成演员目录。
fn actor_name_hit(dir_name: &str, want: &str) -> bool {
    let a = norm_actor_name(dir_name).to_lowercase();
    let b = norm_actor_name(want).to_lowercase();
    let al = a.chars().count();
    let bl = b.chars().count();
    if al < 2 || bl < 2 {
        return false;
    }
    let short = al.min(bl);
    let long = al.max(bl);
    if long - short > 6 {
        return false;
    }
    // 目录名只有一两个字、演员名却很长时，不能因为包含一个字母就算命中（ZZ ≠ 测试演员ZZ）。
    if short <= 2 && a != b {
        return false;
    }
    if a == b || a.contains(&b) || b.contains(&a) {
        return true;
    }
    let need = std::cmp::max(3, (short as f64 * 0.6).ceil() as usize);
    lcs_len(&a, &b) >= need
}

async fn actor_match_roots(st: &AppState, client: &crate::litepan::LitePanClient) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    for key in ["actor_roots", "cache_roots", "netdisk_roots"] {
        if let Some(arr) = st.state.get_setting(key, json!([])).as_array() {
            for r in arr {
                if let Some(p) = r.get("path").and_then(Value::as_str).map(|s| s.trim_matches('/')).filter(|s| !s.is_empty()) {
                    if !roots.contains(&p.to_string()) {
                        roots.push(p.to_string());
                    }
                }
            }
        }
    }
    if roots.is_empty() {
        if let Ok(tasks) = client.strm_tasks().await {
            for t in tasks {
                if let Some(p) = t.get("path").and_then(Value::as_str).map(|s| s.trim_matches('/')).filter(|s| !s.is_empty()) {
                    if !roots.contains(&p.to_string()) {
                        roots.push(p.to_string());
                    }
                }
            }
        }
    }
    if roots.is_empty() {
        roots.push("Zpool/movies".into());
    }
    roots
}

/// 有的演员能搜到、有的搜不到，旧逻辑是索引未命中就做整棵树 BFS（600 个目录，经常超时）。
/// 改成“演员桶目录”定向探测：索引父目录 + 根目录一级子目录，最多 80 次 resolve，8 路并发。
/// 是否是“演员桶”：mov-A7 / mov-10 / FC2-xxx 这类目录，而不是演员目录本身。
fn bucketish(name: &str) -> bool {
    let low = name.to_lowercase();
    low.starts_with("mov-")
        || low.starts_with("fc2-")
        || low.starts_with("91-")
        || low.starts_with("300mium")
        || low.starts_with("heydouga")
}

/// 是否是“桶容器”：mov / 91 / FC2 / 步兵 这类，下面才是 mov-A7 这种演员桶。
fn containerish(name: &str) -> bool {
    let low = name.to_lowercase();
    matches!(
        low.as_str(),
        "mov" | "movies" | "91" | "fc2" | "步兵" | "骑兵" | "无码" | "無碼" | "有码" | "有碼" | "素人" | "糖心vlog"
    ) || low.contains("movies")
}

async fn discover_actor_dirs(
    st: &AppState,
    client: Arc<crate::litepan::LitePanClient>,
    account_id: i64,
    names: &[String],
    root_hint: &str,
) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut buckets: Vec<String> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    let hint = root_hint.trim_matches('/').trim();
    if !hint.is_empty() {
        roots.push(hint.to_string());
    }
    for r in actor_match_roots(st, &client).await {
        if !roots.contains(&r) {
            roots.push(r);
        }
    }
    let mut expand: Vec<(String, String)> = Vec::new();
    // 先放配置的根目录及一级子目录（mov、mov-A2…这些才是真正的“演员桶”）。
    for root in roots.iter().cloned().take(8) {
        if buckets.len() >= 80 {
            break;
        }
        let root_path = root.trim_matches('/').to_string();
        // STRM 任务经常把“演员目录”本身当根（.../mov-A7/桃園怜奈），
        // 这种情况直接取它的父目录当桶，不用再列一遍演员目录里的几十个文件。
        if let Some((parent, _)) = root_path.rsplit_once('/') {
            let parent_base = parent.rsplit('/').next().unwrap_or("");
            if bucketish(parent_base) {
                if !buckets.contains(&parent.to_string()) {
                    buckets.push(parent.to_string());
                }
                continue;
            }
        }
        let rp = format!("/{}", root_path);
        let Ok(resolved) = client.resolve_dir(account_id, &rp, false).await else {
            continue;
        };
        let pid = resolved.get("parent_id").and_then(Value::as_str).unwrap_or("").to_string();
        if pid.is_empty() {
            continue;
        }
        if !buckets.contains(&root_path) {
            buckets.push(root_path.clone());
        }
        // 根目录如果本身就是演员目录（STRM 任务常见），它的父目录才是演员桶。
        if let Some((parent, _)) = root_path.rsplit_once('/') {
            if parent.split('/').count() >= 2 && !buckets.contains(&parent.to_string()) {
                buckets.push(parent.to_string());
            }
        }
        let Ok(listed) = client.list_dir(account_id, &pid, "", None).await else {
            continue;
        };
        let items = listed.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        for it in items {
            if !json_bool(&it, "is_dir", false) {
                continue;
            }
            let name = json_str(&it, "name");
            if name.is_empty() {
                continue;
            }
            let full = format!("{root_path}/{name}").trim_matches('/').to_string();
            for want in names {
                if actor_name_hit(&name, want) {
                    let e = out.entry(want.clone()).or_default();
                    if !e.contains(&full) && e.len() < 20 {
                        e.push(full.clone());
                    }
                }
            }
            if bucketish(&name) {
                if buckets.len() < 120 && !buckets.contains(&full) {
                    buckets.push(full);
                }
            } else if containerish(&name) {
                expand.push((full, json_str(&it, "id")));
            } else if buckets.len() < 120 && !buckets.contains(&full) {
                buckets.push(full);
            }
        }
    }
    // 展开一层容器：Zpool/movies/mov → mov-A2 / mov-A7…，这些才是演员目录的父目录。
    // 索引为空（新部署/换了服务器）时，这一步是演员匹配能否成功的关键。
    let expanded: Vec<(String, Vec<Value>)> = futures::stream::iter(expand.into_iter().take(16).map(|(base, id)| {
        let client = client.clone();
        async move {
            if id.is_empty() {
                return (base, Vec::new());
            }
            match client.list_dir(account_id, &id, "", None).await {
                Ok(v) => (base, v.get("items").and_then(Value::as_array).cloned().unwrap_or_default()),
                Err(_) => (base, Vec::new()),
            }
        }
    }))
    .buffer_unordered(8)
    .collect()
    .await;
    for (base, rows) in expanded {
        for it in rows {
            if !json_bool(&it, "is_dir", false) {
                continue;
            }
            let name = json_str(&it, "name");
            if name.is_empty() {
                continue;
            }
            let full = format!("{base}/{name}").trim_matches('/').to_string();
            for want in names {
                if actor_name_hit(&name, want) {
                    let e = out.entry(want.clone()).or_default();
                    if !e.contains(&full) && e.len() < 20 {
                        e.push(full.clone());
                    }
                }
            }
            if bucketish(&name) && buckets.len() < 120 && !buckets.contains(&full) {
                buckets.push(full);
            }
        }
    }
    // 再补充索引里出现过的“桶目录”（mov-A7 / mov-10 / FC2-xxx 这种）。
    for d in st.fileindex.all_dirs_cached().iter() {
        let p = json_str(d, "path").trim_matches('/').to_string();
        if let Some((parent, _)) = p.rsplit_once('/') {
            let base = parent.rsplit('/').next().unwrap_or("");
            if parent.split('/').count() >= 3 && bucketish(base) && !buckets.contains(&parent.to_string()) {
                buckets.push(parent.to_string());
            }
        }
        if buckets.len() >= 120 {
            break;
        }
    }
    // mov / mov-A2 这类才是演员桶，优先探测；和已知根目录相关的桶排最前。
    // LitePan 的目录请求基本是串行的，桶太多会让首次匹配拖到十几秒。
    let related = |b: &str| {
        roots.iter().any(|r| {
            let r = r.trim_matches('/');
            b == r || b.starts_with(&format!("{r}/")) || r.starts_with(&format!("{b}/"))
        })
    };
    buckets.sort_by_key(|b| {
        let base = b.rsplit('/').next().unwrap_or("").to_lowercase();
        let bucket_rank = if bucketish(&base) { 0 } else { 1 };
        let root_rank = if related(b) { 0 } else { 1 };
        // mov-A* 桶优先，保证像 mov-A8 这种靠后的桶也在探测范围内。
        (bucket_rank, root_rank, b.chars().count())
    });
    buckets.dedup();

    let mut probes: Vec<(String, String)> = Vec::new();
    for (i, want) in names.iter().enumerate() {
        // 即使索引里有模糊命中，也要探测精确的 <桶>/<演员名>。
        // 只给前 3 个名字足量桶位（通常是女演员），男演员/片商名只做少量探测，
        // 否则一次匹配会发出上百个请求，反代下直接超时。
        let take = if i == 0 { 48 } else if i < 3 { 16 } else { 6 };
        for b in buckets.iter().take(take) {
            probes.push((b.clone(), want.clone()));
            if probes.len() >= 120 {
                break;
            }
        }
        if probes.len() >= 120 {
            break;
        }
    }
    tracing::info!(
        "actor discover names={:?} buckets={} probes={}",
        names,
        buckets.len(),
        probes.len()
    );
    let found: Vec<(String, String, String)> = futures::stream::iter(probes.into_iter().map(|(bucket, want)| {        let client = client.clone();
        async move {
            let path = format!("{}/{}", bucket.trim_end_matches('/'), want).trim_matches('/').to_string();
            match client.resolve_dir(account_id, &path, false).await {
                Ok(v) => {
                    let id = v.get("parent_id").and_then(Value::as_str).unwrap_or("").to_string();
                    (want, path, id)
                }
                Err(_) => (want, String::new(), String::new()),
            }
        }
    }))
    .buffer_unordered(16)
    .collect()
    .await;

    let matched = found.iter().filter(|(_, p, _)| !p.is_empty()).count();
    if matched > 0 {
        tracing::info!(
            "actor discover matched={matched} hits={:?}",
            found
                .iter()
                .filter(|(_, p, _)| !p.is_empty())
                .map(|(w, p, _)| format!("{w}=>{p}"))
                .collect::<Vec<_>>()
        );
    }

    let mut upserts = Vec::new();
    for (want, path, id) in found {
        if path.is_empty() {
            continue;
        }
        let e = out.entry(want.clone()).or_default();
        if !e.contains(&path) && e.len() < 20 {
            e.push(path.clone());
        }
        upserts.push(json!({
            "id": id, "name": path.rsplit('/').next().unwrap_or(""),
            "path": path, "is_dir": true, "size": 0, "account_id": account_id,
        }));
    }
    if !upserts.is_empty() {
        let _ = st.fileindex.upsert(account_id, &upserts);
    }
    out
}

async fn actor_dirs(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let mut names: Vec<String> = v.get("names").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).collect()).unwrap_or_default();
    if names.is_empty() {
        if let Some(s) = v.get("name").and_then(Value::as_str) {
            names.push(s.trim().to_string());
        }
    }
    names.retain(|n| !n.is_empty());
    if names.is_empty() {
        return Err(bad("缺少 names"));
    }
    st.fileindex.ensure_known();
    let root_hint = v.get("root").and_then(Value::as_str).unwrap_or("").trim_matches('/').to_string();
    let mut map = Map::new();
    let validator = st.pool.client_for(&Value::Null).await.ok();
    let account_id = match &validator {
        Some(client) => client
            .pick_account(v.get("account_id").and_then(Value::as_i64))
            .await
            .ok()
            .and_then(|a| a.get("id").and_then(Value::as_i64))
            .unwrap_or(0),
        None => 0,
    };
    let all_dirs = st.fileindex.all_dirs_cached();
    for name in &names {
        let mut dirs = Vec::new();
        for it in st.fileindex.search(name, 300, 0) {
            if !json_bool(&it, "is_dir", false) {
                continue;
            }
            let p = json_str(&it, "path").trim_matches('/').to_string();
            if !p.is_empty() && !dirs.contains(&p) {
                dirs.push(p);
            }
            if dirs.len() >= 20 {
                break;
            }
        }
        // 繁简/异体/日文写法的模糊匹配，同样只读缓存的目录列表，不做整棵树扫描。
        for d in all_dirs.iter() {
            if !actor_name_hit(&json_str(d, "name"), name) {
                continue;
            }
            let p = json_str(d, "path").trim_matches('/').to_string();
            if !p.is_empty() && !dirs.contains(&p) && dirs.len() < 20 {
                dirs.push(p);
            }
        }
        if !dirs.is_empty() {
            map.insert(name.clone(), json!(dirs));
        }
    }
    // 索引没收录的新演员目录，用“桶目录”定向补搜（替代原来的整棵树 BFS）。
    // 只有“存在完全同名目录”才算命中；模糊命中不能挡住精确补搜。
    let missing: Vec<String> = names
        .iter()
        .filter(|n| {
            let want = norm_actor_name(n).to_lowercase();
            let has_exact = map
                .get(*n)
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter().filter_map(Value::as_str).any(|p| {
                        norm_actor_name(p.rsplit('/').next().unwrap_or("")).to_lowercase() == want
                    })
                })
                .unwrap_or(false);
            !has_exact
        })
        .cloned()
        .collect();
    if !missing.is_empty() {
        if let Some(client) = &validator {
            let live = discover_actor_dirs(&st, client.clone(), account_id, &missing, &root_hint).await;
            for (name, dirs) in live {
                if !dirs.is_empty() {
                    let mut merged: Vec<String> = map
                        .get(&name)
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                        .unwrap_or_default();
                    for d in dirs {
                        if !merged.contains(&d) {
                            merged.push(d);
                        }
                    }
                    map.insert(name, json!(merged));
                }
            }
        }
    }
    // 最后统一批量实时核对，过滤掉已删除/改名的目录（并发 + 目录缓存）。
    if let Some(client) = &validator {
        let mut clean = Map::new();
        for (name, arr) in map {
            let paths: Vec<String> = arr
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            let live = filter_existing_dirs(
                st.dir_cache.clone(),
                client.clone(),
                st.fileindex.clone(),
                account_id,
                paths,
            )
            .await;
            if !live.is_empty() {
                let exact_key = norm_actor_name(&name).to_lowercase();
                let mut live = live;
                // 完全同名的目录排最前面，模糊命中的靠后，脚本默认就选对的。
                live.sort_by_key(|p| {
                    let base = norm_actor_name(p.rsplit('/').next().unwrap_or("")).to_lowercase();
                    let rank = if base == exact_key {
                        0
                    } else if !exact_key.is_empty() && (base.contains(&exact_key) || exact_key.contains(&base)) {
                        1
                    } else {
                        2
                    };
                    (rank, p.chars().count())
                });
                clean.insert(name, json!(live));
            }
        }
        map = clean;
    }
    Ok(ok(json!({"ok": true, "map": map, "source": "fileindex+live", "tree": {"ready": true, "building": false, "age": 0, "scanned": 0}})))
}

async fn actors_list(State(st): State<Arc<AppState>>) -> Response {
    let items = st.actors.actors_list();
    ok(json!({"ok": true, "items": items, "total": items.len()}))
}

async fn actor_ranking(State(st): State<Arc<AppState>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let limit = q.get("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(24);
    ok(st.actors.ranking(limit))
}

async fn actor_cache_config_get(State(st): State<Arc<AppState>>) -> Response {
    ok(json!({
        "ok": true,
        "cache_roots": st.state.get_setting("cache_roots", st.state.get_setting("actor_roots", json!([]))),
        "ttl_hours": st.state.get_setting("cache_ttl_hours", json!(168)),
        "refresh": st.state.get_str("cache_refresh", "manual"),
        "cache_auto": st.state.get_setting("cache_auto", json!(false)),
        "interval_days": st.state.get_setting("cache_interval_days", json!(10)),
        "cache_dir": cache_dir(&st),
    }))
}

async fn actor_cache_config_post(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let mut patch = Map::new();
    for key in ["cache_roots", "cache_ttl_hours", "cache_refresh", "cache_auto", "interval_days", "cache_dir", "avatar_dir", "cleanup_accounts"] {
        if let Some(val) = v.get(key) {
            patch.insert(key.into(), val.clone());
        }
    }
    if patch.is_empty() {
        return Err(bad("没有可识别的配置项"));
    }
    st.state.set_settings(patch);
    Ok(actor_cache_config_get(State(st)).await)
}

async fn actor_cache_status(State(st): State<Arc<AppState>>) -> Response {
    let roots = st.state.get_setting("cache_roots", st.state.get_setting("actor_roots", json!([])));
    let stats = st.actors.cache_status();
    ok(json!({
        "ok": true, "state": "idle", "scanning": false, "full": false, "trigger": "manual",
        "started_at": 0, "finished_at": 0, "duration_ms": 0, "root_index": 0,
        "root_total": roots.as_array().map(|a| a.len()).unwrap_or(0), "current_root": "",
        "dirs_seen": 0, "dirs_scanned": 0, "files_seen": 0, "added": 0, "updated": 0,
        "invalidated": 0, "skipped": 0, "error": "", "errors": [], "last_scan": {},
        "actors_total": stats.get("actors_total").cloned().unwrap_or(json!(0)),
        "movies_total": stats.get("movies_total").cloned().unwrap_or(json!(0)),
        "cache_dir": cache_dir(&st),
    }))
}

async fn actor_cache_roots(State(st): State<Arc<AppState>>) -> Response {
    let roots = st.state.get_setting("cache_roots", st.state.get_setting("actor_roots", json!([])));
    let mut out = Vec::new();
    if let Some(arr) = roots.as_array() {
        for r in arr {
            let path = r.get("path").and_then(Value::as_str).unwrap_or("").trim_matches('/');
            if path.is_empty() {
                continue;
            }
            out.push(json!({
                "key": format!("{}|{}", r.get("account_id").and_then(Value::as_i64).unwrap_or(0), path),
                "path": path, "account_id": r.get("account_id").and_then(Value::as_i64).unwrap_or(0),
                "configured": true, "dir_count": 0, "scanned_at": 0, "age": -1,
                "complete": false, "error": "", "used_count": 0, "last_used": 0,
                "last_scan": 0, "last_added": 0, "last_removed": 0, "last_duration_ms": 0,
                "last_state": "", "last_trigger": "",
            }));
        }
    }
    ok(json!({"ok": true, "roots": out}))
}

async fn actor_cache_scan(State(_st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let _ = parse_body(&body).unwrap_or_else(|_| json!({}));
    Ok(ok_status(StatusCode::ACCEPTED, json!({"ok": true, "started": true, "full": false})))
}

async fn actor_cache_tree_refresh(State(_st): State<Arc<AppState>>) -> Response {
    ok(json!({"ok": true, "started": true}))
}

async fn actor_cache_clear(State(_st): State<Arc<AppState>>) -> Response {
    ok(json!({"ok": true, "cleared": true}))
}

async fn actor_cache_actors(State(st): State<Arc<AppState>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let limit = q.get("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(100);
    let mut items: Vec<Value> = st.actors.actors_list().into_iter().filter(|a| a.get("scanned").and_then(Value::as_bool).unwrap_or(false)).collect();
    let total = items.len();
    items.truncate(limit);
    ok(json!({"ok": true, "items": items, "total": total, "limit": limit}))
}

fn emby_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // 家庭媒体服务器经常用自签证书
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(12))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

fn emby_base(url: &str) -> String {
    let mut u = url.trim().trim_end_matches('/').to_string();
    if u.is_empty() {
        return u;
    }
    if !u.starts_with("http://") && !u.starts_with("https://") {
        u = format!("http://{u}");
    }
    u
}

struct EmbyAuth {
    base: String,
    prefix: String,
    token: String,
    server: String,
    version: String,
}

fn emby_pick(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return s.trim().to_string();
            }
        }
    }
    String::new()
}

/// 同时兼容 Emby(/emby) 和 Jellyfin(根路径)：
/// - API Key: GET System/Info
/// - 用户名密码: POST Users/AuthenticateByName 换 AccessToken
async fn emby_auth(v: &Value) -> Result<EmbyAuth, String> {
    // 兼容两种入参：{url,key,...} 和 {name, emby:{url,key,...}}
    let src = v.get("emby").filter(|x| x.is_object()).unwrap_or(v);
    let base = emby_base(&json_str(src, "url"));
    if base.is_empty() {
        return Err("缺少服务地址".into());
    }
    let mode = json_str(src, "mode");
    let key = json_str(src, "key");
    let user = json_str(src, "user");
    let pass = json_str(src, "pass");
    let use_key = if mode == "user" { false } else { !key.is_empty() };
    let client = emby_client();
    let prefixes = ["", "/emby"];

    if use_key {
        for p in prefixes {
            let url = format!("{base}{p}/System/Info?api_key={}", urlencoding::encode(&key));
            if let Ok(resp) = client.get(&url).header("X-Emby-Token", &key).send().await {
                if resp.status().is_success() {
                    let info: Value = resp.json().await.unwrap_or_else(|_| json!({}));
                    return Ok(EmbyAuth {
                        base: base.clone(),
                        prefix: p.to_string(),
                        token: key.clone(),
                        server: emby_pick(&info, &["ServerName", "Name"]),
                        version: emby_pick(&info, &["Version"]),
                    });
                }
            }
        }
        return Err("API Key 无效，或服务地址不可达".into());
    }

    if user.is_empty() || pass.is_empty() {
        return Err("请填写 API Key，或用户名 + 密码".into());
    }
    let mut last_err = String::new();
    for p in prefixes {
        let url = format!("{base}{p}/Users/AuthenticateByName");
        let resp = client
            .post(&url)
            .header(
                "X-Emby-Authorization",
                "MediaBrowser Client=\"Ark-push\", Device=\"Ark-push\", DeviceId=\"ark-push\", Version=\"1.0\"",
            )
            .json(&json!({"Username": user, "Pw": pass}))
            .send()
            .await;
        let Ok(resp) = resp else {
            last_err = "网络不可达".into();
            continue;
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            last_err = format!("HTTP {}: {}", status.as_u16(), text.chars().take(160).collect::<String>());
            continue;
        }
        let val: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({}));
        let token = emby_pick(&val, &["AccessToken", "accessToken"]);
        if token.is_empty() {
            last_err = "登录成功但没有返回 AccessToken".into();
            continue;
        }
        let info_url = format!("{base}{p}/System/Info?api_key={}", urlencoding::encode(&token));
        let info: Value = match client
            .get(&info_url)
            .header("X-Emby-Token", &token)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r.json().await.unwrap_or_else(|_| json!({})),
            _ => json!({}),
        };
        return Ok(EmbyAuth {
            base: base.clone(),
            prefix: p.to_string(),
            token,
            server: emby_pick(&info, &["ServerName", "Name"]),
            version: emby_pick(&info, &["Version"]),
        });
    }
    Err(format!("登录失败:{last_err}"))
}

/// 在 Emby/Jellyfin 里找演员(Person)的 Id。
async fn emby_find_person(auth: &EmbyAuth, name: &str) -> Result<(String, String), String> {
    let client = emby_client();
    let q = urlencoding::encode(name);
    let url = format!(
        "{}{}/Persons?SearchTerm={}&Limit=20&Fields=Name&api_key={}",
        auth.base,
        auth.prefix,
        q,
        urlencoding::encode(&auth.token)
    );
    let resp = client
        .get(&url)
        .header("X-Emby-Token", &auth.token)
        .send()
        .await
        .map_err(|e| format!("查询演员失败:{e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("查询演员失败:HTTP {}:{}", status.as_u16(), text.chars().take(160).collect::<String>()));
    }
    let val: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({}));
    let want = norm_actor_name(name).to_lowercase();
    let items = val.get("Items").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut fallback = String::new();
    for it in items {
        let id = emby_pick(&it, &["Id", "id"]);
        let n = emby_pick(&it, &["Name", "name"]);
        if id.is_empty() {
            continue;
        }
        if fallback.is_empty() {
            fallback = id.clone();
        }
        if norm_actor_name(&n).to_lowercase() == want {
            return Ok((id, n));
        }
    }
    if !fallback.is_empty() {
        return Ok((fallback, name.to_string()));
    }
    Err("服务里没有找到这个演员".into())
}

async fn emby_test(State(_st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    match emby_auth(&v).await {
        Ok(a) => Ok(ok(json!({
            "ok": true, "server": if a.server.is_empty() { "Emby".to_string() } else { a.server },
            "version": if a.version.is_empty() { "?".to_string() } else { a.version },
            "prefix": a.prefix,
        }))),
        Err(e) => Ok(ok(json!({"ok": false, "error": e}))),
    }
}

async fn emby_person(State(_st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let name = json_str(&v, "name");
    if name.is_empty() {
        return Ok(ok(json!({"ok": false, "error": "缺少演员名"})));
    }
    let Ok(auth) = emby_auth(&v).await else {
        return Ok(ok(json!({"ok": false, "error": "匹配服务连接失败,请先在设置里测试连接", "name": name})));
    };
    let q = urlencoding::encode(&name);
    let list = format!("{}{}/web/index.html#!/search.html?query={}", auth.base, auth.prefix, q);
    match emby_find_person(&auth, &name).await {
        Ok((id, matched_name)) => {
            let details = if auth.prefix.is_empty() {
                format!("{}/web/index.html#!/details?id={}", auth.base, id)
            } else {
                format!("{}{}/web/index.html#!/person?id={}", auth.base, auth.prefix, id)
            };
            Ok(ok(json!({
                "ok": true, "id": id, "name": matched_name,
                "details": details, "list": list,
            })))
        }
        Err(e) => Ok(ok(json!({"ok": true, "id": "", "name": name, "details": "", "list": list, "note": e}))),
    }
}

async fn emby_count(State(_st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let name = json_str(&v, "name");
    if name.is_empty() {
        return Ok(ok(json!({"ok": false, "error": "缺少演员名", "name": name, "count": 0})));
    }
    let Ok(auth) = emby_auth(&v).await else {
        return Ok(ok(json!({"ok": false, "error": "匹配服务未连接", "name": name, "count": 0})));
    };
    let Ok((id, _)) = emby_find_person(&auth, &name).await else {
        return Ok(ok(json!({"ok": true, "name": name, "count": 0})));
    };
    let client = emby_client();
    let url = format!(
        "{}{}/Items?Recursive=true&PersonIds={}&IncludeItemTypes=Movie,Episode&Limit=1&api_key={}",
        auth.base,
        auth.prefix,
        urlencoding::encode(&id),
        urlencoding::encode(&auth.token)
    );
    let count = match client.get(&url).header("X-Emby-Token", &auth.token).send().await {
        Ok(resp) if resp.status().is_success() => {
            let val: Value = resp.json().await.unwrap_or_else(|_| json!({}));
            val.get("TotalRecordCount").and_then(Value::as_i64).unwrap_or(0)
        }
        _ => 0,
    };
    Ok(ok(json!({"ok": true, "name": name, "count": count})))
}

// ---------------------------------------------------------------- Ark
fn ark_guard(st: &AppState, headers: &HeaderMap, body: &Value) -> bool {
    let token = token_from_headers(headers, body.get("token").and_then(Value::as_str));
    if st.auth.check(token.as_deref()) {
        return true;
    }
    let key = headers
        .get("X-Ark-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    key.map(|k| st.auth.verify_password(k)).unwrap_or(false)
}

fn ark_root_groups(st: &AppState) -> HashMap<i64, Vec<String>> {
    let mut groups: HashMap<i64, Vec<String>> = HashMap::new();
    let mut seen = HashSet::new();
    let mut pending_default = Vec::new();
    let roots = st.state.get_setting("cache_roots", st.state.get_setting("actor_roots", st.state.get_setting("netdisk_roots", json!([]))));
    if let Some(arr) = roots.as_array() {
        for r in arr {
            let p = r.get("path").and_then(Value::as_str).unwrap_or("").trim_matches('/');
            if p.is_empty() {
                continue;
            }
            let a = r.get("account_id").and_then(Value::as_i64).unwrap_or(0);
            if a == 0 {
                pending_default.push(p.to_string());
            } else if seen.insert((a, p.to_string())) {
                groups.entry(a).or_default().push(p.to_string());
            }
        }
    }
    if !pending_default.is_empty() {
        groups.entry(0).or_default().extend(pending_default);
    }
    if groups.is_empty() {
        groups.insert(0, vec![String::new()]);
    }
    groups
}

async fn ark_search(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    if !ark_guard(&st, &headers, &v) {
        return Ok(ok_status(StatusCode::UNAUTHORIZED, json!({"ok": false, "ark": true, "login_required": true, "error": "请先登录 Ark-push(统一账号密码)或提供正确的 X-Ark-Key"})));
    }
    let keyword = json_str(&v, "keyword");
    if keyword.is_empty() {
        return Err(bad("请填写搜索关键词"));
    }
    let want = v.get("account_id").and_then(Value::as_i64).unwrap_or(0);
    st.fileindex.ensure_known();
    let groups = ark_root_groups(&st);
    let mut building = false;
    for (acc, roots) in &groups {
        // 没有配置扫描根目录时，不要为了让脚本能搜索就全盘扫一遍（会打满 LitePan）。
        if roots.iter().all(|r| r.trim_matches('/').is_empty()) {
            continue;
        }
        let aid = if *acc == 0 { 0 } else { *acc };
        let s = st.fileindex.ensure(aid, roots);
        building = building || s.get("building").and_then(Value::as_bool).unwrap_or(false);
    }
    let mut items = st.fileindex.search(&keyword, 300, want);
    let mut source = "fileindex";
    if items.is_empty() && json_bool(&v, "fresh", false) {
        source = "live";
        if let Ok(client) = st.pool.client_for(&v).await {
            for acc in groups.keys().copied() {
                let aid = if acc == 0 {
                    client.pick_account(None).await.ok().and_then(|a| a.get("id").and_then(Value::as_i64)).unwrap_or(0)
                } else { acc };
                if let Ok(found) = client.net_search(&keyword, aid, 50, false, Some(8)).await {
                    let key: String = keyword.to_uppercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                    for it in found {
                        let name = json_str(&it, "name");
                        let path = json_str(&it, "path");
                        if !key.is_empty() && !normalize_key(&[&name, &path]).contains(&key) {
                            continue;
                        }
                        items.push(json!({
                            "id": json_str(&it, "id"), "name": name, "path": path,
                            "is_dir": json_bool(&it, "is_dir", false), "size": json_i64(&it, "size", 0),
                            "account_id": aid, "account_label": "",
                        }));
                        if items.len() >= 60 { break; }
                    }
                }
                if items.len() >= 60 { break; }
            }
        }
    }
    Ok(ok(json!({
        "ok": true, "keyword": keyword, "items": items, "building": building,
        "source": source, "errors": if items.is_empty() && building { vec!["搜索索引构建中(首次扫盘需要一点时间),请稍候重试"] } else { vec![] },
        "cached": false,
    })))
}

async fn ark_index_scan(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    if !ark_guard(&st, &headers, &v) {
        return Ok(ok_status(StatusCode::UNAUTHORIZED, json!({"ok": false, "ark": true, "login_required": true})));
    }
    let groups = ark_root_groups(&st);
    for (acc, roots) in groups {
        let aid = if acc == 0 { 0 } else { acc };
        if json_bool(&v, "full", true) {
            st.fileindex.force(aid, &roots);
        } else {
            st.fileindex.ensure(aid, &roots);
        }
    }
    Ok(ok_status(StatusCode::ACCEPTED, json!({"ok": true, "started": true})))
}

async fn ark_index_status(State(st): State<Arc<AppState>>) -> Response {
    ok(json!({"ok": true, "accounts": st.fileindex.status(), "search": st.search.stats(), "cache": {}}))
}

async fn ark_cache_clear(State(_st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if token_from_headers(&headers, None)
        .map(|t| _st.auth.check(Some(&t)))
        .unwrap_or(false)
    {
        ok(json!({"ok": true, "cleared": true}))
    } else {
        ok_status(StatusCode::UNAUTHORIZED, json!({"ok": false, "error": "请先登录"}))
    }
}

async fn ark_find_by_code(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    if !ark_guard(&st, &headers, &v) {
        return Ok(ok_status(StatusCode::UNAUTHORIZED, json!({"ok": false, "ark": true, "login_required": true})));
    }
    let code = json_str(&v, "code");
    let key: String = code.to_uppercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if key.len() < 4 {
        return Ok(ok(json!({"ok": true, "items": []})));
    }
    let want = v.get("account_id").and_then(Value::as_i64).unwrap_or(0);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for it in st.fileindex.search(&code, 200, want) {
        let name = json_str(&it, "name");
        if json_bool(&it, "is_dir", false) || !indexer::is_video(&name) {
            continue;
        }
        if !normalize_key(&[&name]).contains(&key) {
            continue;
        }
        let id = json_str(&it, "id");
        let path = json_str(&it, "path");
        if !seen.insert((id.clone(), path.clone())) {
            continue;
        }
        out.push(json!({
            "id": id, "name": name, "path": path, "size": json_i64(&it, "size", 0),
            "is_dir": false, "account_id": json_i64(&it, "account_id", 0),
        }));
    }
    Ok(ok(json!({"ok": true, "items": out, "source": if out.is_empty() { "none" } else { "fileindex" }, "cached": false})))
}

async fn ark_resolve(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    if !ark_guard(&st, &headers, &v) {
        return Ok(ok_status(StatusCode::UNAUTHORIZED, json!({"ok": false, "ark": true, "login_required": true})));
    }
    let name = json_str(&v, "name");
    let path = json_str(&v, "path").trim_matches('/').to_string();
    if name.is_empty() || path.is_empty() {
        return Err(bad("缺少 path/name"));
    }
    let client = st.pool.client_for(&v).await.map_err(|e| bad(e.to_string()))?;
    let account_id = v.get("account_id").and_then(Value::as_i64).unwrap_or(0);
    let account_id = if account_id == 0 {
        client.pick_account(None).await.map_err(|e| bad(e.to_string()))?.get("id").and_then(Value::as_i64).unwrap_or(0)
    } else { account_id };
    let mut hit = None;
    for candidate in [path.clone(), path.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default()] {
        if candidate.is_empty() { continue; }
        let resolved = client.resolve_dir(account_id, &candidate, false).await;
        if let Ok(res) = resolved {
            let pid = res.get("parent_id").and_then(Value::as_str).unwrap_or("");
            if let Ok(listed) = client.list_dir(account_id, pid, "", None).await {
                if let Some(it) = listed.get("items").and_then(Value::as_array).and_then(|a| a.iter().find(|i| json_str(i, "name") == name)) {
                    hit = Some(it.clone());
                    break;
                }
            }
        }
    }
    Ok(ok(json!({"ok": true, "item": hit.map(|it| json!({
        "id": json_str(&it, "id"), "name": name, "path": format!("{}/{}", path.trim_end_matches('/'), name),
        "size": json_i64(&it, "size", 0), "is_dir": false, "account_id": account_id,
    }))})))
}

async fn ark_play(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    if !ark_guard(&st, &headers, &v) {
        return Ok(ok_status(StatusCode::UNAUTHORIZED, json!({"ok": false, "ark": true, "login_required": true})));
    }
    lp_play(State(st), body).await
}

async fn ark_rename(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    if !ark_guard(&st, &headers, &v) {
        return Ok(ok_status(StatusCode::UNAUTHORIZED, json!({"ok": false, "ark": true, "login_required": true})));
    }
    lp_rename(State(st), body).await
}

async fn ark_delete(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    if !ark_guard(&st, &headers, &v) {
        return Ok(ok_status(StatusCode::UNAUTHORIZED, json!({"ok": false, "ark": true, "login_required": true})));
    }
    lp_delete(State(st), body).await
}

// ---------------------------------------------------------------- backup / fs / avatar
async fn backup_export(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let password = v.get("password").and_then(Value::as_str).unwrap_or("");
    if password.chars().count() < 8 {
        return Err(bad("请设置至少 8 位的备份密码"));
    }
    let include_creds = v.get("include_creds").map(|x| bool_from(Some(x), true)).unwrap_or(true);
    let note = v.get("note").and_then(Value::as_str).unwrap_or("").chars().take(120).collect::<String>();
    let mut settings = st.state.settings_snapshot();
    if !include_creds {
        for key in ["litepan_url", "litepan_username", "litepan_password", "litepan_insecure", "auth_user", "auth_password_hash"] {
            settings.remove(key);
        }
    }
    let mut payload = json!({
        "kind": "arkpan-push-backup",
        "version": 2,
        "note": note,
        "include_creds": include_creds,
        "exported_at": now_secs(),
        "exported_at_iso": iso_now(),
        "container_version": env!("CARGO_PKG_VERSION"),
        "settings": settings,
    });
    if include_creds {
        let (url, user, pwd) = st.pool.creds();
        payload["creds"] = json!({
            "litepan": {"url": url, "username": user, "password": pwd, "insecure": st.pool.insecure()},
            "match": st.state.match_config(),
            "auth": st.state.auth_creds(),
        });
        payload["tasks"] = json!(st.pending.export_all(5000));
    }
    let envelope = backup::encrypt(&payload, password).map_err(|e| bad(e.to_string()))?;
    let mut resp = ok(envelope);
    resp.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"ark-backup-{}.json\"", chrono::Local::now().format("%Y%m%d-%H%M%S"))).unwrap(),
    );
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

async fn backup_import(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let data = v.get("data").cloned().unwrap_or(v.clone());
    let password = v.get("password").and_then(Value::as_str).unwrap_or("");
    let data = if backup::is_encrypted(&data) {
        match backup::decrypt(&data, password) {
            Ok(d) => d,
            Err(e) => return Ok(ok_status(StatusCode::BAD_REQUEST, json!({"ok": false, "error": e.to_string(), "encrypted": true}))),
        }
    } else {
        data
    };
    if data.get("kind").and_then(Value::as_str) != Some("arkpan-push-backup") {
        return Err(bad("不是有效的备份文件"));
    }
    let known = [
        "link_115", "index_source", "netdisk_roots", "actor_roots", "actor_root",
        "actor_account_id", "cleanup_accounts", "cache_dir", "avatar_dir", "browse_favorites",
        "litepan_url", "litepan_username", "litepan_password", "litepan_insecure",
        "auth_user", "auth_password_hash",
    ];
    let mut clean = Map::new();
    if let Some(settings) = data.get("settings").and_then(Value::as_object) {
        for (k, v) in settings {
            if known.contains(&k.as_str()) {
                clean.insert(k.clone(), v.clone());
            }
        }
    }
    let count = clean.len();
    if !clean.is_empty() {
        st.state.set_settings(clean);
    }
    let lit = data.get("creds").and_then(|c| c.get("litepan"));
    if let Some(lit) = lit {
        let pwd = lit.get("password").and_then(Value::as_str).unwrap_or("");
        if !pwd.is_empty() {
            let url = lit.get("url").and_then(Value::as_str).unwrap_or("").trim().trim_end_matches('/');
            let user = lit.get("username").and_then(Value::as_str).unwrap_or("admin").trim();
            let insecure = lit.get("insecure").and_then(Value::as_bool);
            let default_url = st.cfg.litepan_base();
            let url_arg = if url.is_empty() { default_url.as_str() } else { url };
            st.state.set_litepan_creds(url_arg, if user.is_empty() { "admin" } else { user }, pwd, insecure);
            st.pool.clear();
        }
    }
    if let Some(m) = data.get("creds").and_then(|c| c.get("match")) {
        if m.is_object() {
            st.state.set_match_config(m.clone());
        }
    }
    if let Some(auth) = data.get("creds").and_then(|c| c.get("auth")) {
        st.state.set_auth_creds(
            auth.get("username").and_then(Value::as_str),
            auth.get("password_hash").and_then(Value::as_str),
        );
    }
    let tasks = data.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default();
    let added = st.pending.import_all(&tasks, 5000);
    Ok(ok(json!({"ok": true, "settings": count, "tasks": added,
        "has_litepan": data.get("creds").and_then(|c| c.get("litepan")).is_some(),
        "has_match": data.get("creds").and_then(|c| c.get("match")).is_some(),
        "has_auth": data.get("creds").and_then(|c| c.get("auth")).is_some()})))
}

async fn fs_list(State(_st): State<Arc<AppState>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let raw = q.get("path").cloned().unwrap_or_else(|| "/".into());
    let real = std::fs::canonicalize(&raw).unwrap_or_else(|_| PathBuf::from(&raw));
    let roots = ["/state", "/cache", "/avatars", "/mnt", "/data", "/tmp"];
    let allowed = real == FsPath::new("/") || roots.iter().any(|r| real == FsPath::new(r) || real.starts_with(FsPath::new(r)));
    if !allowed || !real.is_dir() {
        return ok_status(StatusCode::BAD_REQUEST, json!({"ok": false, "error": "只能浏览容器内的白名单目录(/state、/cache、/avatars、/mnt、/data、/tmp)"}));
    }
    let mut items = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&real) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || !e.path().is_dir() {
                continue;
            }
            items.push(json!({"name": name, "path": e.path(), "is_dir": true}));
        }
    }
    items.sort_by(|a, b| a.get("name").and_then(Value::as_str).unwrap_or("").cmp(b.get("name").and_then(Value::as_str).unwrap_or("")));
    ok(json!({"ok": true, "path": real, "parent": real.parent().unwrap_or(FsPath::new("/")), "items": items}))
}

async fn favorites_get(State(st): State<Arc<AppState>>) -> Response {
    ok(json!({"ok": true, "favorites": st.state.get_setting("browse_favorites", json!([]))}))
}

async fn favorites_post(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body)?;
    let fav = v.get("favorites").cloned().unwrap_or_else(|| v.clone());
    st.state.set_setting("browse_favorites", fav.clone());
    Ok(ok(json!({"ok": true, "favorites": fav})))
}

async fn avatar_file(State(st): State<Arc<AppState>>, Path(file): Path<String>) -> Response {
    if !regex::Regex::new(r"^[0-9a-f]{20}\.jpg$").map(|r| r.is_match(&file)).unwrap_or(false) {
        return ApiError::new(StatusCode::NOT_FOUND, "not found").into_response();
    }
    let path = PathBuf::from(avatar_dir(&st)).join(&file);
    match tokio::fs::read(&path).await {
        Ok(bytes) => (StatusCode::OK, [(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response(),
        Err(_) => ApiError::new(StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

// ---------------------------------------------------------------- poster / runtime
async fn poster_dirs(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body).unwrap_or_else(|_| json!({}));
    let sub = v.get("path").or_else(|| v.get("sub")).and_then(Value::as_str).unwrap_or("");
    let root = st.cfg.strm_root.clone();
    let Some(base) = safe_join(FsPath::new(&root), sub) else {
        return Err(bad("路径越界"));
    };
    let mut dirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&base) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                let name = e.file_name().to_string_lossy().to_string();
                let rel = e.path().strip_prefix(&root).unwrap_or(&e.path()).to_string_lossy().replace('\\', "/");
                dirs.push(json!({"name": name, "path": rel}));
            }
        }
    }
    dirs.sort_by(|a, b| a.get("name").and_then(Value::as_str).unwrap_or("").cmp(b.get("name").and_then(Value::as_str).unwrap_or("")));
    Ok(ok(json!({"ok": true, "path": sub, "dirs": dirs})))
}

async fn poster_scan(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let v = parse_body(&body).unwrap_or_else(|_| json!({}));
    let sub = v.get("path").or_else(|| v.get("sub")).and_then(Value::as_str).unwrap_or("");
    let root = st.cfg.strm_root.clone();
    let Some(base) = safe_join(FsPath::new(&root), sub) else {
        return Err(bad("路径越界"));
    };
    let mut items = Vec::new();
    let mut stack = vec![base.clone()];
    while let Some(cur) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&cur) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|s| s.to_str()).map(|s| s.eq_ignore_ascii_case("strm")).unwrap_or(false) {
                let rel = p.strip_prefix(&root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                let dir = p.parent().and_then(|d| d.strip_prefix(&root).ok()).map(|d| d.to_string_lossy().replace('\\', "/")).unwrap_or_default();
                let has = ["poster.jpg", "folder.jpg"].iter().any(|n| p.parent().map(|d| d.join(n).exists()).unwrap_or(false));
                items.push(json!({"rel": rel, "name": name, "dir": dir, "has_poster": has, "size": std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0)}));
            }
        }
    }
    items.sort_by(|a, b| a.get("rel").and_then(Value::as_str).unwrap_or("").cmp(b.get("rel").and_then(Value::as_str).unwrap_or("")));
    Ok(ok(json!({"ok": true, "items": items})))
}

async fn poster_generate(State(_st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let _ = parse_body(&body).unwrap_or_else(|_| json!({}));
    Ok(ok(json!({"ok": false, "error": "当前 Rust 版本未内置 FFmpeg 抽帧;请安装 ffmpeg 后使用原版海报功能,或在后续版本启用。"})))
}

async fn poster_status(State(_st): State<Arc<AppState>>) -> Response {
    ok(json!({"ok": true, "job": {"state": "idle"}, "ffmpeg": which_ffmpeg().is_some()}))
}

fn which_ffmpeg() -> Option<String> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|p| {
            let c = p.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
            if c.is_file() { Some(c.to_string_lossy().to_string()) } else { None }
        })
    })
}

async fn runtime_ffmpeg(State(_st): State<Arc<AppState>>) -> Response {
    let bin = which_ffmpeg();
    ok(json!({"ok": true, "installed": bin.is_some(), "path": bin.clone().unwrap_or_default(), "local": false, "version": "", "installing": false, "progress": 0, "error": "", "message": ""}))
}

async fn runtime_ffmpeg_install(State(_st): State<Arc<AppState>>) -> Response {
    ok_status(StatusCode::ACCEPTED, json!({"ok": false, "error": "Rust 版本暂未实现 FFmpeg 在线安装;请使用系统 ffmpeg 或原版容器。"}))
}

async fn runtime_storage(State(st): State<Arc<AppState>>) -> Response {
    let mut items = Vec::new();
    for name in ["state.json", "litepan_session.json", "pending.db", "cache", "avatars", "plugins", "plugin-data", "ffmpeg", "search_index.json", "fileindex"] {
        let p = PathBuf::from(&st.cfg.state_dir).join(name);
        if p.is_dir() {
            items.push(json!({"name": name, "kind": "dir", "exists": true}));
        } else if p.is_file() {
            items.push(json!({"name": name, "kind": "file", "exists": true, "bytes": std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0)}));
        } else {
            items.push(json!({"name": name, "kind": "missing", "exists": false}));
        }
    }
    ok(json!({"ok": true, "state_dir": st.cfg.state_dir, "state_writable": std::fs::metadata(&st.cfg.state_dir).map(|m| !m.permissions().readonly()).unwrap_or(false), "items": items, "ffmpeg": which_ffmpeg().is_some()}))
}

// ---------------------------------------------------------------- plugins
async fn plugins_list(State(_st): State<Arc<AppState>>) -> Response {
    ok(json!({"ok": true, "items": [], "nav": [], "api_version": "1.0", "container_version": env!("CARGO_PKG_VERSION"), "allow_install": false}))
}

async fn plugins_summary(State(_st): State<Arc<AppState>>) -> Response {
    ok(json!({"ok": true, "installed": 0, "enabled": 0, "nav": [], "need_token": false, "api_version": "1.0", "container_version": env!("CARGO_PKG_VERSION"), "enabled_globally": false, "allow_install": false}))
}

async fn plugins_nav(State(_st): State<Arc<AppState>>) -> Response {
    ok(json!({"ok": true, "nav": []}))
}

async fn plugins_inspect(State(_st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let _ = parse_body(&body).unwrap_or_else(|_| json!({}));
    Ok(ok(json!({"ok": false, "error": "当前 Rust 版本尚未启用插件安装;原 Python 插件可通过原版容器继续使用。"})))
}

async fn plugins_install(State(_st): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let _ = parse_body(&body).unwrap_or_else(|_| json!({}));
    Ok(ok(json!({"ok": false, "error": "当前 Rust 版本尚未启用插件安装;原 Python 插件可通过原版容器继续使用。"})))
}

async fn plugin_detail(State(_st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    ok_status(StatusCode::NOT_FOUND, json!({"ok": false, "error": format!("插件不存在:{id}")}))
}

async fn plugin_enable(State(_st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    ok_status(StatusCode::NOT_FOUND, json!({"ok": false, "error": format!("插件不存在:{id}")}))
}

async fn plugin_disable(State(_st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    ok_status(StatusCode::NOT_FOUND, json!({"ok": false, "error": format!("插件不存在:{id}")}))
}

async fn plugin_reload(State(_st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    ok_status(StatusCode::NOT_FOUND, json!({"ok": false, "error": format!("插件不存在:{id}")}))
}

async fn plugin_rollback(State(_st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    ok_status(StatusCode::NOT_FOUND, json!({"ok": false, "error": format!("插件不存在:{id}")}))
}

async fn plugin_uninstall(State(_st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    ok_status(StatusCode::NOT_FOUND, json!({"ok": false, "error": format!("插件不存在:{id}")}))
}

async fn plugin_config(State(_st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    ok_status(StatusCode::NOT_FOUND, json!({"ok": false, "error": format!("插件不存在:{id}")}))
}

// ---------------------------------------------------------------- helper
pub fn resolve_roots(st: &AppState) -> Vec<Value> {
    indexer::infer_roots_from_settings(&st.state)
}

pub fn detect_search_doc_id(path: &str) -> String {
    doc_id(path)
}




