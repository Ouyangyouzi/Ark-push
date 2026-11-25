use parking_lot::RwLock;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub const STEPS: &[&str] = &[
    "login", "account", "capabilities", "resolve", "submit", "register", "auto", "index",
];

pub struct JobStore {
    jobs: RwLock<HashMap<String, Value>>,
}

impl JobStore {
    pub fn new() -> Self {
        Self {
            jobs: RwLock::new(HashMap::new()),
        }
    }

    pub fn create(&self, payload: &Value) -> Value {
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let now = chrono::Utc::now().timestamp();
        let mut steps = Map::new();
        for step in STEPS {
            steps.insert((*step).to_string(), Value::String("pending".into()));
        }
        let mut req = payload.clone();
        if let Some(obj) = req.as_object_mut() {
            obj.remove("password");
            obj.remove("username");
        }
        let job = json!({
            "id": id,
            "state": "pending",
            "created": now,
            "updated": now,
            "steps": steps,
            "detail": {},
            "result": {},
            "error": "",
            "request": req,
        });
        self.jobs.write().insert(id.to_string(), job.clone());
        job
    }

    pub fn update(&self, id: &str, patch: Value) {
        let mut jobs = self.jobs.write();
        if let Some(job) = jobs.get_mut(id).and_then(Value::as_object_mut) {
            if let Some(obj) = patch.as_object() {
                for (k, v) in obj {
                    job.insert(k.clone(), v.clone());
                }
            }
            job.insert("updated".into(), json!(chrono::Utc::now().timestamp()));
        }
    }

    pub fn step(&self, id: &str, name: &str, status: &str, detail: &str) {
        let mut jobs = self.jobs.write();
        if let Some(job) = jobs.get_mut(id).and_then(Value::as_object_mut) {
            if let Some(steps) = job.get_mut("steps").and_then(Value::as_object_mut) {
                steps.insert(name.to_string(), Value::String(status.to_string()));
            }
            if !detail.is_empty() {
                if let Some(d) = job.get_mut("detail").and_then(Value::as_object_mut) {
                    d.insert(name.to_string(), Value::String(detail.to_string()));
                }
            }
            job.insert("updated".into(), json!(chrono::Utc::now().timestamp()));
        }
        tracing::info!("[job {id}] {name} -> {status} {detail}");
    }

    pub fn get(&self, id: &str) -> Option<Value> {
        self.jobs.read().get(id).cloned()
    }

    pub fn recent(&self, limit: usize) -> Vec<Value> {
        let mut items: Vec<Value> = self.jobs.read().values().cloned().collect();
        items.sort_by_key(|j| {
            std::cmp::Reverse(j.get("created").and_then(Value::as_i64).unwrap_or(0))
        });
        items.truncate(limit);
        items
    }
}
