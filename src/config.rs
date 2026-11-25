use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub litepan_url: String,
    pub litepan_username: String,
    pub litepan_password: String,
    pub strm_root: String,
    pub http_port: u16,
    pub link_115: bool,
    pub link_enabled: bool,
    pub sync_interval_min: u64,
    pub link_interval_min: u64,
    pub offline_timeout_min: u64,
    pub state_dir: String,
    pub log_level: String,
    pub batch_size: usize,
    pub push_allow_anon: bool,
    pub cache_dir: String,
    pub avatar_dir: String,
    pub plugins_enabled: bool,
    pub plugin_allow_install: bool,
    pub plugin_admin_token: String,
    pub plugin_dir: String,
    pub plugin_data_dir: String,
    pub plugin_call_timeout: u64,
    pub plugin_max_package_mb: u64,
    pub plugin_memory_mb: u64,
    pub plugin_run_uid: u32,
    pub plugin_run_gid: u32,
    pub ui_dir: String,
    pub python_dir: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0}")]
    Message(String),
}

fn s(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string()).trim().to_string()
}

fn b(key: &str, default: bool) -> bool {
    let raw = env::var(key).unwrap_or_default();
    let raw = raw.trim().to_lowercase();
    if raw.is_empty() {
        return default;
    }
    matches!(raw.as_str(), "1" | "true" | "on" | "yes" | "y")
}

fn i(key: &str, default: i64) -> Result<i64, ConfigError> {
    let raw = env::var(key).unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(default);
    }
    raw.parse::<i64>()
        .map_err(|_| ConfigError::Message(format!("{key} ?={raw:?}")))
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let state_dir = s("STATE_DIR", "/state");
        let plugin_dir = env::var("PLUGIN_DIR")
            .unwrap_or_else(|_| format!("{state_dir}/plugins"))
            .trim()
            .to_string();
        let plugin_data_dir = env::var("PLUGIN_DATA_DIR")
            .unwrap_or_else(|_| format!("{state_dir}/plugin-data"))
            .trim()
            .to_string();

        let cfg = Self {
            litepan_url: s("LITEPAN_URL", "http://host.docker.internal:5211"),
            litepan_username: s("LITEPAN_USERNAME", "admin"),
            litepan_password: s("LITEPAN_PASSWORD", ""),
            strm_root: s("STRM_ROOT", "/src"),
            http_port: i("HTTP_PORT", i("SEARCH_PORT", 7701)?)? as u16,
            link_115: b("LINK_115", false),
            link_enabled: b("LINK_ENABLED", true),
            sync_interval_min: i("SYNC_INTERVAL_MIN", 10)?.max(1) as u64,
            link_interval_min: i("LINK_INTERVAL_MIN", 60)?.max(1) as u64,
            offline_timeout_min: i("OFFLINE_TIMEOUT_MIN", 120)?.max(1) as u64,
            state_dir,
            log_level: s("LOG_LEVEL", "info").to_uppercase(),
            batch_size: i("INDEX_BATCH_SIZE", 500)?.max(1) as usize,
            push_allow_anon: b("PUSH_ALLOW_ANON", false),
            cache_dir: s("CACHE_DIR", ""),
            avatar_dir: s("ACTOR_AVATAR_DIR", ""),
            plugins_enabled: b("PLUGINS_ENABLED", true),
            plugin_allow_install: b("PLUGIN_ALLOW_INSTALL", true),
            plugin_admin_token: s("PLUGIN_ADMIN_TOKEN", ""),
            plugin_dir,
            plugin_data_dir,
            plugin_call_timeout: i("PLUGIN_CALL_TIMEOUT", 30)?.max(1) as u64,
            plugin_max_package_mb: i("PLUGIN_MAX_PACKAGE_MB", 20)?.max(1) as u64,
            plugin_memory_mb: i("PLUGIN_MEMORY_MB", 256)?.max(16) as u64,
            plugin_run_uid: i("PLUGIN_RUN_UID", 10001)?.max(1) as u32,
            plugin_run_gid: i("PLUGIN_RUN_GID", 10001)?.max(1) as u32,
            ui_dir: s("UI_DIR", "/app/ui"),
            python_dir: s("PYTHON_DIR", "/app/python"),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if !self.litepan_url.starts_with("http://") && !self.litepan_url.starts_with("https://") {
            return Err(ConfigError::Message(format!(
                "LITEPAN_URL ?http:// ?https:// ?={:?}",
                self.litepan_url
            )));
        }
        Ok(())
    }

    pub fn litepan_base(&self) -> String {
        self.litepan_url.trim_end_matches('/').to_string()
    }

    pub fn cache_dir_resolved(&self) -> String {
        if !self.cache_dir.is_empty() {
            self.cache_dir.clone()
        } else {
            format!("{}/cache", self.state_dir.trim_end_matches('/'))
        }
    }

    pub fn avatar_dir_resolved(&self) -> String {
        if !self.avatar_dir.is_empty() {
            self.avatar_dir.clone()
        } else {
            format!("{}/avatars", self.state_dir.trim_end_matches('/'))
        }
    }

    pub fn state_path(&self, name: &str) -> PathBuf {
        PathBuf::from(&self.state_dir).join(name)
    }

    pub fn insecure_tls(&self) -> bool {
        let raw = env::var("LITEPAN_INSECURE")
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        matches!(raw.as_str(), "1" | "true" | "on" | "yes")
    }
}

pub fn setup_logging(level: &str) {
    let resolved = level.trim().to_uppercase();
    let filter = match resolved.as_str() {
        "TRACE" => "trace",
        "DEBUG" => "debug",
        "WARN" => "warn",
        "ERROR" => "error",
        _ => "info",
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(true)
        .try_init();
}

pub fn iso_now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}
