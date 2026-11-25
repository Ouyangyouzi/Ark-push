use parking_lot::Mutex;
use rusqlite::{params, Connection, Row};
use serde_json::{json, Value};
use std::path::Path;

pub struct PendingStore {
    conn: Mutex<Connection>,
}

fn row_to_json(row: &Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, i64>("id")?,
        "account_id": row.get::<_, i64>("account_id")?,
        "path": row.get::<_, String>("path")?,
        "file_name": row.get::<_, String>("file_name")?,
        "size": row.get::<_, i64>("size")?,
        "url": row.get::<_, String>("url")?,
        "status": row.get::<_, String>("status")?,
        "error": row.get::<_, String>("error")?,
        "job_id": row.get::<_, String>("job_id")?,
        "organize": row.get::<_, i64>("organize")?,
        "organize_opts": row.get::<_, String>("organize_opts")?,
        "created_at": row.get::<_, i64>("created_at")?,
        "updated_at": row.get::<_, i64>("updated_at")?,
    }))
}

impl PendingStore {
    pub fn new(state_dir: &str) -> rusqlite::Result<Self> {
        std::fs::create_dir_all(state_dir).ok();
        let conn = Connection::open(Path::new(state_dir).join("pending.db"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS pending (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id INTEGER NOT NULL DEFAULT 0,
                path TEXT NOT NULL DEFAULT '',
                file_name TEXT NOT NULL DEFAULT '',
                size INTEGER NOT NULL DEFAULT 0,
                url TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'pending',
                error TEXT NOT NULL DEFAULT '',
                job_id TEXT NOT NULL DEFAULT '',
                organize INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_pending_status ON pending(status);",
        )?;
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(pending)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(Result::ok)
            .collect();
        if !cols.iter().any(|c| c == "organize") {
            let _ = conn.execute("ALTER TABLE pending ADD COLUMN organize INTEGER NOT NULL DEFAULT 0", []);
        }
        if !cols.iter().any(|c| c == "organize_opts") {
            let _ = conn.execute("ALTER TABLE pending ADD COLUMN organize_opts TEXT NOT NULL DEFAULT ''", []);
        }
        tracing::info!("pending store ready: {}", Path::new(state_dir).join("pending.db").display());
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn add(
        &self,
        account_id: i64,
        path: &str,
        file_name: &str,
        size: i64,
        url: &str,
        job_id: &str,
        organize: bool,
        organize_opts: &str,
    ) -> rusqlite::Result<i64> {
        let now = chrono::Utc::now().timestamp();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO pending(account_id,path,file_name,size,url,job_id,organize,organize_opts,status,created_at,updated_at)
             VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            params![
                account_id,
                path,
                file_name,
                size,
                url,
                job_id,
                if organize { 1 } else { 0 },
                organize_opts,
                "pending",
                now,
                now
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn set_status(&self, id: i64, status: &str, error: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE pending SET status=?, error=?, updated_at=? WHERE id=?",
            params![status, error.chars().take(500).collect::<String>(), chrono::Utc::now().timestamp(), id],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> rusqlite::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM pending WHERE id=?", params![id])? > 0)
    }

    pub fn get(&self, id: i64) -> Option<Value> {
        let conn = self.conn.lock();
        conn.query_row("SELECT * FROM pending WHERE id=?", params![id], row_to_json)
            .ok()
    }

    pub fn list(&self, status: Option<&str>, limit: usize) -> Vec<Value> {
        let conn = self.conn.lock();
        let mut out = Vec::new();
        if let Some(status) = status {
            if let Ok(mut stmt) = conn.prepare("SELECT * FROM pending WHERE status=? ORDER BY id DESC LIMIT ?") {
                if let Ok(rows) = stmt.query_map(params![status, limit as i64], row_to_json) {
                    out.extend(rows.filter_map(Result::ok));
                }
            }
        } else if let Ok(mut stmt) = conn.prepare("SELECT * FROM pending ORDER BY id DESC LIMIT ?") {
            if let Ok(rows) = stmt.query_map(params![limit as i64], row_to_json) {
                out.extend(rows.filter_map(Result::ok));
            }
        }
        out
    }

    pub fn export_all(&self, limit: usize) -> Vec<Value> {
        let conn = self.conn.lock();
        let mut out = Vec::new();
        if let Ok(mut stmt) = conn.prepare("SELECT * FROM pending ORDER BY id LIMIT ?") {
            if let Ok(rows) = stmt.query_map(params![limit as i64], row_to_json) {
                out.extend(rows.filter_map(Result::ok));
            }
        }
        out
    }

    pub fn import_all(&self, rows: &[Value], limit: usize) -> usize {
        let conn = self.conn.lock();
        let mut added = 0usize;
        for raw in rows.iter().take(limit) {
            let path = raw.get("path").and_then(Value::as_str).unwrap_or("");
            let file_name = raw.get("file_name").and_then(Value::as_str).unwrap_or("");
            let url = raw.get("url").and_then(Value::as_str).unwrap_or("");
            let status = raw.get("status").and_then(Value::as_str).unwrap_or("pending");
            if path.is_empty() && file_name.is_empty() && url.is_empty() {
                continue;
            }
            let dup: Option<i64> = conn
                .query_row(
                    "SELECT id FROM pending WHERE path=? AND file_name=? AND url=? AND status=? LIMIT 1",
                    params![path, file_name, url, status],
                    |r| r.get(0),
                )
                .ok();
            if dup.is_some() {
                continue;
            }
            let now = chrono::Utc::now().timestamp();
            let ok = conn.execute(
                "INSERT INTO pending (account_id,path,file_name,size,url,status,error,job_id,organize,organize_opts,created_at,updated_at)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    raw.get("account_id").and_then(Value::as_i64).unwrap_or(0),
                    path,
                    file_name,
                    raw.get("size").and_then(Value::as_i64).unwrap_or(0),
                    url,
                    status,
                    raw.get("error").and_then(Value::as_str).unwrap_or(""),
                    "",
                    raw.get("organize").and_then(Value::as_i64).unwrap_or(0),
                    raw.get("organize_opts").and_then(Value::as_str).unwrap_or(""),
                    raw.get("created_at").and_then(Value::as_i64).unwrap_or(now),
                    now
                ],
            );
            if ok.is_ok() {
                added += 1;
            }
        }
        added
    }

    pub fn counts(&self) -> Value {
        let conn = self.conn.lock();
        let mut out = serde_json::Map::new();
        if let Ok(mut stmt) = conn.prepare("SELECT status, COUNT(*) FROM pending GROUP BY status") {
            if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) {
                for (status, n) in rows.filter_map(Result::ok) {
                    out.insert(status, json!(n));
                }
            }
        }
        for key in ["pending", "error", "generated"] {
            out.entry(key.to_string()).or_insert(json!(0));
        }
        Value::Object(out)
    }

    pub fn summary(&self) -> Value {
        json!({"counts": self.counts(), "items": self.list(None, 500)})
    }
}
