use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Represents a single time-series event returned from an ActivityWatch bucket query.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AwEvent {
    pub timestamp: DateTime<Utc>,
    pub duration: f64, // seconds
    #[serde(default)]
    pub data: Value,
}

/// Asynchronous HTTP client for querying the local ActivityWatch REST API (`/api/0`).
#[derive(Clone)]
pub struct AwClient {
    base_url: String,
    client: reqwest::Client,
    buckets_verified: Arc<AtomicBool>,
    has_web_bucket: Arc<AtomicBool>,
}

impl AwClient {
    /// Creates a new `AwClient` targeting `http://localhost:<port>` with an optional Bearer token.
    pub fn new(port: u16, api_key: &str) -> Self {
        let mut headers = HeaderMap::new();
        if !api_key.trim().is_empty() {
            if let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", api_key.trim())) {
                headers.insert(AUTHORIZATION, val);
            }
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default();

        Self {
            base_url: format!("http://localhost:{}", port),
            client,
            buckets_verified: Arc::new(AtomicBool::new(false)),
            has_web_bucket: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Checks whether the ActivityWatch server is reachable via GET /api/0/info.
    pub async fn check_connection(&self) -> bool {
        let url = format!("{}/api/0/info", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    /// Resilient bucket discovery: checks GET /api/0/buckets/ to confirm both
    /// aw-watcher-window_ and aw-watcher-afk_ buckets exist before querying.
    pub async fn has_required_buckets(&self) -> bool {
        if self.buckets_verified.load(Ordering::Relaxed) {
            return true;
        }

        let url = format!("{}/api/0/buckets/", self.base_url);
        let resp = match self.client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => return false,
        };

        if let Ok(Value::Object(map)) = resp.json::<Value>().await {
            let has_window = map.keys().any(|k| k.starts_with("aw-watcher-window_"));
            let has_afk = map.keys().any(|k| k.starts_with("aw-watcher-afk_"));
            let has_web = map.keys().any(|k| k.starts_with("aw-watcher-web"));
            self.has_web_bucket.store(has_web, Ordering::Relaxed);
            if !has_window || !has_afk {
                tracing::warn!(
                    "ActivityWatch buckets not ready yet (window={}, afk={})",
                    has_window,
                    has_afk
                );
            }
            if has_window && has_afk {
                self.buckets_verified.store(true, Ordering::Relaxed);
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Queries canonical active window events intersected with not-afk status.
    pub async fn query_intersected_events(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<AwEvent>, Box<dyn std::error::Error + Send + Sync>> {
        if !self.has_required_buckets().await {
            return Ok(Vec::new());
        }

        let url = format!("{}/api/0/query/", self.base_url);
        let timeperiod = format!("{}/{}", start.to_rfc3339(), end.to_rfc3339());

        let query_body = serde_json::json!({
            "timeperiods": [timeperiod],
            "query": [
                "events = flood(query_bucket(find_bucket(\"aw-watcher-window_\")));",
                "not_afk = flood(query_bucket(find_bucket(\"aw-watcher-afk_\")));",
                "not_afk = filter_keyvals(not_afk, \"status\", [\"not-afk\"]);",
                "events = filter_period_intersect(events, not_afk);",
                "RETURN = events;"
            ]
        });

        let resp_result = self.client.post(&url).json(&query_body).send().await;
        let resp = match resp_result {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                let status = r.status();
                tracing::warn!("ActivityWatch query returned HTTP status {}", status);
                self.buckets_verified.store(false, Ordering::Relaxed);
                return Ok(Vec::new());
            }
            Err(e) => {
                tracing::warn!("ActivityWatch query network error: {}", e);
                self.buckets_verified.store(false, Ordering::Relaxed);
                return Err(e.into());
            }
        };

        let body: Value = resp.json().await?;
        // ActivityWatch /api/0/query/ returns a list of results per timeperiod: [[events]]
        if let Some(arr) = body.as_array() {
            if let Some(first_period_events) = arr.first() {
                if let Ok(events) =
                    serde_json::from_value::<Vec<AwEvent>>(first_period_events.clone())
                {
                    return Ok(events);
                }
            }
        }

        Ok(Vec::new())
    }

    /// Queries browser tab events from aw-watcher-web intersected with not-afk status.
    #[allow(dead_code)]
    pub async fn query_web_events(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<AwEvent> {
        if !self.has_required_buckets().await || !self.has_web_bucket.load(Ordering::Relaxed) {
            return Vec::new();
        }

        let url = format!("{}/api/0/query/", self.base_url);
        let timeperiod = format!("{}/{}", start.to_rfc3339(), end.to_rfc3339());

        let query_body = serde_json::json!({
            "timeperiods": [timeperiod],
            "query": [
                "web = flood(query_bucket(find_bucket(\"aw-watcher-web\")));",
                "not_afk = flood(query_bucket(find_bucket(\"aw-watcher-afk_\")));",
                "not_afk = filter_keyvals(not_afk, \"status\", [\"not-afk\"]);",
                "events = filter_period_intersect(web, not_afk);",
                "RETURN = events;"
            ]
        });

        let resp_result = self.client.post(&url).json(&query_body).send().await;
        let resp = match resp_result {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                let status = r.status();
                tracing::warn!("ActivityWatch web query returned HTTP status {}", status);
                return Vec::new();
            }
            Err(e) => {
                tracing::warn!("ActivityWatch web query network error: {}", e);
                return Vec::new();
            }
        };

        let body: Value = match resp.json().await {
            Ok(b) => b,
            Err(_) => return Vec::new(),
        };

        if let Some(arr) = body.as_array() {
            if let Some(first_period_events) = arr.first() {
                if let Ok(events) =
                    serde_json::from_value::<Vec<AwEvent>>(first_period_events.clone())
                {
                    return events;
                }
            }
        }

        Vec::new()
    }
}