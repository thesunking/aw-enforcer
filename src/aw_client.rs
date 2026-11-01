use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::RwLock;

/// Predicate checking if an ActivityWatch bucket key represents a local (non-synchronized) bucket.
pub fn is_local_bucket(key: &str) -> bool {
    !key.contains("-synced") && !key.contains("_synced")
}

/// Typed payload for ActivityWatch window, web, and afk events.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AwEventData {
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

/// Represents a single time-series event returned from an ActivityWatch bucket query.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AwEvent {
    pub timestamp: DateTime<Utc>,
    pub duration: f64, // seconds
    #[serde(default)]
    pub data: AwEventData,
}

/// Asynchronous HTTP client for querying the local ActivityWatch REST API (`/api/0`).
#[derive(Clone)]
pub struct AwClient {
    base_url: String,
    client: reqwest::Client,
    buckets_verified: Arc<AtomicBool>,
    has_web_bucket: Arc<AtomicBool>,
    last_web_bucket_check: Arc<Mutex<Option<Instant>>>,
    pub window_bucket_id: Arc<RwLock<Option<String>>>,
    pub afk_bucket_id: Arc<RwLock<Option<String>>>,
    pub web_bucket_ids: Arc<RwLock<Vec<String>>>,
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
            last_web_bucket_check: Arc::new(Mutex::new(None)),
            window_bucket_id: Arc::new(RwLock::new(None)),
            afk_bucket_id: Arc::new(RwLock::new(None)),
            web_bucket_ids: Arc::new(RwLock::new(Vec::new())),
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
            let window_bucket = map
                .keys()
                .find(|k| k.starts_with("aw-watcher-window_") && is_local_bucket(k))
                .cloned();
            let afk_bucket = map
                .keys()
                .find(|k| k.starts_with("aw-watcher-afk_") && is_local_bucket(k))
                .cloned();
            let mut web_buckets: Vec<String> = map
                .keys()
                .filter(|k| k.starts_with("aw-watcher-web") && is_local_bucket(k))
                .cloned()
                .collect();
            web_buckets.sort();

            let has_window = window_bucket.is_some();
            let has_afk = afk_bucket.is_some();
            let has_web = !web_buckets.is_empty();

            *self.window_bucket_id.write().await = window_bucket;
            *self.afk_bucket_id.write().await = afk_bucket;
            *self.web_bucket_ids.write().await = web_buckets;

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

        let window_bucket = self.window_bucket_id.read().await.clone();
        let afk_bucket = self.afk_bucket_id.read().await.clone();

        let window_line = match window_bucket {
            Some(ref id) => format!("events = flood(query_bucket(\"{id}\"));"),
            None => "events = flood(query_bucket(find_bucket(\"aw-watcher-window_\")));".to_string(),
        };
        let afk_line = match afk_bucket {
            Some(ref id) => format!("not_afk = flood(query_bucket(\"{id}\"));"),
            None => "not_afk = flood(query_bucket(find_bucket(\"aw-watcher-afk_\")));".to_string(),
        };

        let query_body = serde_json::json!({
            "timeperiods": [timeperiod],
            "query": [
                window_line,
                afk_line,
                "not_afk = filter_keyvals(not_afk, \"status\", [\"not-afk\"]);",
                "events = filter_period_intersect(events, not_afk);",
                "RETURN = events;"
            ]
        });

        self.execute_query(&url, &query_body).await
    }

    /// Queries browser tab events from aw-watcher-web intersected with not-afk status across all discovered web buckets.
    pub async fn query_web_events(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<AwEvent> {
        let mut should_refresh = !self.buckets_verified.load(Ordering::Relaxed);
        if !should_refresh && !self.has_web_bucket.load(Ordering::Relaxed) {
            let last_check = *self.last_web_bucket_check.lock().unwrap();
            if last_check.map_or(true, |t| t.elapsed() >= std::time::Duration::from_secs(60)) {
                should_refresh = true;
                self.buckets_verified.store(false, Ordering::Relaxed);
            }
        }

        if should_refresh {
            *self.last_web_bucket_check.lock().unwrap() = Some(Instant::now());
            self.has_required_buckets().await;
        }

        if !self.has_web_bucket.load(Ordering::Relaxed) {
            return Vec::new();
        }

        let web_buckets = self.web_bucket_ids.read().await.clone();
        if web_buckets.is_empty() {
            return Vec::new();
        }

        let url = format!("{}/api/0/query/", self.base_url);
        let timeperiod = format!("{}/{}", start.to_rfc3339(), end.to_rfc3339());

        let web_expr = if web_buckets.len() == 1 {
            format!("query_bucket(\"{}\")", web_buckets[0])
        } else {
            let concat_args = web_buckets
                .iter()
                .map(|b| format!("query_bucket(\"{b}\")"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("sort_by_timestamp(concat({concat_args}))")
        };
        let web_line = format!("web = flood({web_expr});");

        let afk_bucket = self.afk_bucket_id.read().await.clone();
        let afk_line = match afk_bucket {
            Some(ref id) => format!("not_afk = flood(query_bucket(\"{id}\"));"),
            None => "not_afk = flood(query_bucket(find_bucket(\"aw-watcher-afk_\")));".to_string(),
        };

        let query_body = serde_json::json!({
            "timeperiods": [timeperiod],
            "query": [
                web_line,
                afk_line,
                "not_afk = filter_keyvals(not_afk, \"status\", [\"not-afk\"]);",
                "events = filter_period_intersect(web, not_afk);",
                "RETURN = events;"
            ]
        });

        let events = self.execute_query(&url, &query_body).await.unwrap_or_default();
        if events.is_empty() && !self.buckets_verified.load(Ordering::Relaxed) {
            self.has_web_bucket.store(false, Ordering::Relaxed);
        }
        events
    }

    /// Dispatches a query request to ActivityWatch `/api/0/query/`, checking HTTP status and parsing returned events.
    async fn execute_query(
        &self,
        url: &str,
        query_body: &Value,
    ) -> Result<Vec<AwEvent>, Box<dyn std::error::Error + Send + Sync>> {
        let resp_result = self.client.post(url).json(query_body).send().await;
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

        let batches = match resp.json::<Vec<Vec<AwEvent>>>().await {
            Ok(b) => b,
            Err(_) => return Ok(Vec::new()),
        };

        Ok(parse_query_response_events(batches))
    }
}

/// Extracts `Vec<AwEvent>` from an ActivityWatch `/api/0/query/` response batch array (`[[events]]`).
pub fn parse_query_response_events(batches: Vec<Vec<AwEvent>>) -> Vec<AwEvent> {
    batches.into_iter().next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_is_local_bucket_filtering() {
        // Local buckets allowed
        assert!(is_local_bucket("aw-watcher-window_DESKTOP-123"));
        assert!(is_local_bucket("aw-watcher-afk_DESKTOP-123"));
        assert!(is_local_bucket("aw-watcher-web-chrome"));
        assert!(is_local_bucket("aw-watcher-web-firefox"));
        assert!(is_local_bucket("aw-watcher-web-edge"));
        assert!(is_local_bucket("aw-watcher-web-brave"));

        // Remote/synced buckets excluded (-synced and _synced)
        assert!(!is_local_bucket("aw-watcher-window_DESKTOP-123-synced"));
        assert!(!is_local_bucket("aw-watcher-window_DESKTOP-123_synced"));
        assert!(!is_local_bucket("aw-watcher-afk_DESKTOP-123-synced"));
        assert!(!is_local_bucket("aw-watcher-afk_DESKTOP-123_synced"));
        assert!(!is_local_bucket("aw-watcher-web-chrome-synced"));
        assert!(!is_local_bucket("aw-watcher-web-firefox_synced"));
        assert!(!is_local_bucket("aw-watcher-web-edge-synced"));
    }

    #[test]
    fn test_bucket_selection_excludes_synced() {
        let keys = vec![
            "aw-watcher-window_DESKTOP_synced".to_string(),
            "aw-watcher-window_LOCAL".to_string(),
            "aw-watcher-afk_DESKTOP-synced".to_string(),
            "aw-watcher-afk_LOCAL".to_string(),
            "aw-watcher-web-chrome_synced".to_string(),
            "aw-watcher-web-firefox-synced".to_string(),
            "aw-watcher-web-edge".to_string(),
            "aw-watcher-web-brave".to_string(),
        ];

        let window_bucket = keys
            .iter()
            .find(|k| k.starts_with("aw-watcher-window_") && is_local_bucket(k))
            .cloned();
        assert_eq!(window_bucket.as_deref(), Some("aw-watcher-window_LOCAL"));

        let afk_bucket = keys
            .iter()
            .find(|k| k.starts_with("aw-watcher-afk_") && is_local_bucket(k))
            .cloned();
        assert_eq!(afk_bucket.as_deref(), Some("aw-watcher-afk_LOCAL"));

        let mut web_buckets: Vec<String> = keys
            .iter()
            .filter(|k| k.starts_with("aw-watcher-web") && is_local_bucket(k))
            .cloned()
            .collect();
        web_buckets.sort();
        assert_eq!(web_buckets, vec!["aw-watcher-web-brave", "aw-watcher-web-edge"]);

        // When only synced web buckets exist, web_buckets should be empty (no fallback)
        let keys_only_synced_web = vec![
            "aw-watcher-web-chrome_synced".to_string(),
            "aw-watcher-web-firefox-synced".to_string(),
        ];
        let web_only_synced: Vec<String> = keys_only_synced_web
            .iter()
            .filter(|k| k.starts_with("aw-watcher-web") && is_local_bucket(k))
            .cloned()
            .collect();
        assert!(web_only_synced.is_empty());
    }

    #[test]
    fn test_parse_query_response_events() {
        // 1. Empty batches
        let empty_batches: Vec<Vec<AwEvent>> = Vec::new();
        assert!(parse_query_response_events(empty_batches).is_empty());

        // 2. Batches with empty first batch
        let batches_empty_first: Vec<Vec<AwEvent>> = vec![Vec::new()];
        assert!(parse_query_response_events(batches_empty_first).is_empty());

        // 3. Batches with multiple events in first batch and another batch
        let ev1 = AwEvent {
            timestamp: Utc.with_ymd_and_hms(2026, 11, 1, 10, 0, 0).unwrap(),
            duration: 10.0,
            data: AwEventData {
                app: Some("chrome.exe".to_string()),
                title: Some("Google Search".to_string()),
                url: Some("https://google.com".to_string()),
                status: None,
            },
        };
        let ev2 = AwEvent {
            timestamp: Utc.with_ymd_and_hms(2026, 11, 1, 10, 0, 10).unwrap(),
            duration: 20.0,
            data: AwEventData {
                app: Some("code.exe".to_string()),
                title: Some("VS Code".to_string()),
                url: None,
                status: None,
            },
        };
        let ev3 = AwEvent {
            timestamp: Utc.with_ymd_and_hms(2026, 11, 1, 10, 1, 0).unwrap(),
            duration: 30.0,
            data: AwEventData::default(),
        };

        let batches = vec![vec![ev1.clone(), ev2.clone()], vec![ev3]];
        let extracted = parse_query_response_events(batches);
        assert_eq!(extracted.len(), 2);
        assert_eq!(extracted[0].data.app.as_deref(), Some("chrome.exe"));
        assert_eq!(extracted[1].data.app.as_deref(), Some("code.exe"));

        // 4. Deserialization directly from JSON string to Vec<Vec<AwEvent>>
        let json_data = r#"
        [
            [
                {
                    "timestamp": "2026-11-01T10:00:00Z",
                    "duration": 42.5,
                    "data": {
                        "app": "firefox.exe",
                        "title": "Rust Documentation",
                        "url": "https://doc.rust-lang.org"
                    }
                }
            ]
        ]
        "#;
        let deserialized: Vec<Vec<AwEvent>> = serde_json::from_str(json_data).expect("valid JSON");
        let parsed_events = parse_query_response_events(deserialized);
        assert_eq!(parsed_events.len(), 1);
        assert_eq!(parsed_events[0].duration, 42.5);
        assert_eq!(parsed_events[0].data.app.as_deref(), Some("firefox.exe"));
        assert_eq!(parsed_events[0].data.title.as_deref(), Some("Rust Documentation"));
        assert_eq!(parsed_events[0].data.url.as_deref(), Some("https://doc.rust-lang.org"));
    }
}
