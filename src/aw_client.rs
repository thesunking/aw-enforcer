use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

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
    port: u16,
    api_key: String,
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
        let trimmed_key = api_key.trim();
        let mut headers = HeaderMap::new();
        if !trimmed_key.is_empty() {
            if let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", trimmed_key)) {
                headers.insert(AUTHORIZATION, val);
            }
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(5))
            .tcp_nodelay(true)
            .build()
            .unwrap_or_default();

        Self {
            base_url: format!("http://localhost:{}", port),
            port,
            api_key: trimmed_key.to_string(),
            client,
            buckets_verified: Arc::new(AtomicBool::new(false)),
            has_web_bucket: Arc::new(AtomicBool::new(false)),
            last_web_bucket_check: Arc::new(Mutex::new(None)),
            window_bucket_id: Arc::new(RwLock::new(None)),
            afk_bucket_id: Arc::new(RwLock::new(None)),
            web_bucket_ids: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Returns true if this client connects to `port` with `api_key` (trimmed).
    pub fn matches_config(&self, port: u16, api_key: &str) -> bool {
        self.port == port && self.api_key == api_key.trim()
    }

    /// Checks whether the ActivityWatch server is reachable via GET /api/0/info.
    pub async fn check_connection(&self) -> bool {
        let url = self.endpoint_url("/api/0/info");
        match self.client.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
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

        let Some(window_id) = self.window_bucket_id.read().await.clone() else {
            return Ok(Vec::new());
        };
        let Some(afk_id) = self.afk_bucket_id.read().await.clone() else {
            return Ok(Vec::new());
        };

        let target_expr = flood_bucket_expr(&window_id);
        let query_body = build_not_afk_intersect_query(format_timeperiod(start, end), &target_expr, &afk_id);

        self.execute_query(&self.query_url(), &query_body).await
    }

    /// Queries browser tab events from aw-watcher-web intersected with not-afk status across all discovered web buckets.
    pub async fn query_web_events(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<AwEvent> {
        if !self.prepare_poll_buckets(true).await || !self.has_web_bucket.load(Ordering::Relaxed) {
            return Vec::new();
        }

        let web_buckets = self.web_bucket_ids.read().await.clone();
        let Some(web_expr) = build_web_query_expr(&web_buckets) else {
            return Vec::new();
        };

        let Some(afk_id) = self.afk_bucket_id.read().await.clone() else {
            return Vec::new();
        };

        let query_body = build_not_afk_intersect_query(format_timeperiod(start, end), &web_expr, &afk_id);

        self.execute_query(&self.query_url(), &query_body).await.unwrap_or_default()
    }

    /// Prepares buckets before polling, refreshing `/api/0/buckets/` if unverified or if web buckets need a 60s refresh.
    /// Returns `true` if required buckets are ready, avoiding duplicate calls and racing queries during `tokio::join!`.
    pub async fn prepare_poll_buckets(&self, need_web: bool) -> bool {
        let is_verified = self.buckets_verified.load(Ordering::Relaxed);
        let need_web_refresh = need_web && self.should_refresh_web_buckets();

        if !is_verified || need_web_refresh {
            self.refresh_buckets().await
        } else {
            true
        }
    }

    /// Resilient bucket discovery: checks GET /api/0/buckets/ to confirm both
    /// aw-watcher-window_ and aw-watcher-afk_ buckets exist before querying.
    pub async fn has_required_buckets(&self) -> bool {
        self.prepare_poll_buckets(false).await
    }

    /// Fetches all buckets from `/api/0/buckets/`, updates cached window, afk, and web bucket IDs,
    /// and sets `buckets_verified` if both window and afk buckets exist.
    pub async fn refresh_buckets(&self) -> bool {
        *self.last_web_bucket_check.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        let url = self.endpoint_url("/api/0/buckets/");
        let resp = match self.client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => {
                self.buckets_verified.store(false, Ordering::Relaxed);
                return false;
            }
        };

        if let Ok(Value::Object(map)) = resp.json::<Value>().await {
            let discovered = DiscoveredBuckets::from_bucket_map(&map);
            let has_window = discovered.window_bucket.is_some();
            let has_afk = discovered.afk_bucket.is_some();
            let has_web = !discovered.web_buckets.is_empty();
            let ready = discovered.is_ready();

            *self.window_bucket_id.write().await = discovered.window_bucket;
            *self.afk_bucket_id.write().await = discovered.afk_bucket;
            *self.web_bucket_ids.write().await = discovered.web_buckets;

            self.has_web_bucket.store(has_web, Ordering::Relaxed);
            if !has_window || !has_afk {
                tracing::warn!(
                    "ActivityWatch buckets not ready yet (window={}, afk={})",
                    has_window,
                    has_afk
                );
            }
            self.buckets_verified.store(ready, Ordering::Relaxed);
            ready
        } else {
            self.buckets_verified.store(false, Ordering::Relaxed);
            false
        }
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
            Err(e) => {
                tracing::warn!("Failed to parse ActivityWatch query response JSON: {}", e);
                return Ok(Vec::new());
            }
        };

        Ok(parse_query_response_events(batches))
    }

    /// Checks whether web buckets need a refresh (unverified or >=60 seconds since last check).
    fn should_refresh_web_buckets(&self) -> bool {
        let last_check = *self.last_web_bucket_check.lock().unwrap_or_else(|e| e.into_inner());
        last_check.map_or(true, |t| t.elapsed() >= Duration::from_secs(60))
    }

    /// Resolves an ActivityWatch API endpoint URL given an API path.
    fn endpoint_url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// URL for the ActivityWatch query endpoint.
    fn query_url(&self) -> String {
        self.endpoint_url("/api/0/query/")
    }
}

// ---------------------------------------------------------------------------
// Discovered Buckets Structure
// ---------------------------------------------------------------------------

/// Discovered window, AFK, and web buckets parsed from the ActivityWatch `/api/0/buckets/` map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredBuckets {
    pub window_bucket: Option<String>,
    pub afk_bucket: Option<String>,
    pub web_buckets: Vec<String>,
}

impl DiscoveredBuckets {
    /// Extracts canonical local window, afk, and sorted web buckets from an ActivityWatch bucket map.
    pub fn from_bucket_map(map: &serde_json::Map<String, Value>) -> Self {
        let window_bucket = select_best_bucket("aw-watcher-window_", map.iter().map(|(k, v)| (k.as_str(), v)));
        let afk_bucket = select_best_bucket("aw-watcher-afk_", map.iter().map(|(k, v)| (k.as_str(), v)));
        let mut web_buckets: Vec<String> = map
            .keys()
            .filter(|k| k.starts_with("aw-watcher-web") && is_local_bucket(k))
            .cloned()
            .collect();
        web_buckets.sort();

        Self {
            window_bucket,
            afk_bucket,
            web_buckets,
        }
    }

    /// Returns `true` if both window and AFK buckets are present.
    pub fn is_ready(&self) -> bool {
        self.window_bucket.is_some() && self.afk_bucket.is_some()
    }
}

// ---------------------------------------------------------------------------
// Query Construction & Response Extraction Helpers
// ---------------------------------------------------------------------------

/// Extracts `Vec<AwEvent>` from an ActivityWatch `/api/0/query/` response batch array (`[[events]]`).
pub fn parse_query_response_events(batches: Vec<Vec<AwEvent>>) -> Vec<AwEvent> {
    batches.into_iter().next().unwrap_or_default()
}

/// Builds an ActivityWatch query body intersecting `target_expr` with `not_afk` status.
fn build_not_afk_intersect_query(timeperiod: String, target_expr: &str, afk_id: &str) -> Value {
    let afk_expr = flood_bucket_expr(afk_id);
    serde_json::json!({
        "timeperiods": [timeperiod],
        "query": [
            format!("events = {target_expr};"),
            format!("not_afk = {afk_expr};"),
            "not_afk = filter_keyvals(not_afk, \"status\", [\"not-afk\"]);",
            "events = filter_period_intersect(events, not_afk);",
            "RETURN = events;"
        ]
    })
}

/// Builds the ActivityWatch query expression string for one or more browser web buckets.
/// Floods each bucket individually, folds multiple buckets via binary `concat(acc, next)`,
/// and wraps multiple buckets in `sort_by_timestamp(...)`.
pub fn build_web_query_expr(web_buckets: &[String]) -> Option<String> {
    if web_buckets.is_empty() {
        return None;
    }
    if web_buckets.len() == 1 {
        return Some(flood_bucket_expr(&web_buckets[0]));
    }
    let mut it = web_buckets.iter().map(|b| flood_bucket_expr(b));
    let first = it.next()?;
    let folded = it.fold(first, |acc, next| format!("concat({}, {})", acc, next));
    Some(format!("sort_by_timestamp({})", folded))
}

/// Wraps an escaped bucket ID in ActivityWatch's query expression `flood(query_bucket("..."))`.
pub fn flood_bucket_expr(bucket_id: &str) -> String {
    format!("flood(query_bucket(\"{}\"))", escape_bucket_id(bucket_id))
}

/// Formats a start/end time range into an ActivityWatch `start/end` timeperiod string.
fn format_timeperiod(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    format!("{}/{}", start.to_rfc3339(), end.to_rfc3339())
}

/// Escapes backslashes and double quotes in bucket IDs for safe interpolation into aw-server query strings.
pub fn escape_bucket_id(id: &str) -> String {
    id.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// Bucket Discovery & Selection Helpers
// ---------------------------------------------------------------------------

/// Selects the best bucket matching `prefix` among candidate local buckets.
///
/// When multiple local buckets exist (e.g. following a hostname change), metadata is inspected:
/// 1. Candidates with `last_updated` are compared first (`b.last_updated.cmp(&a.last_updated)`).
/// 2. Prefer buckets whose `"hostname"` matches the local machine hostname (`COMPUTERNAME`).
/// 3. Compare effective timestamp (`created`).
/// 4. Fall back to deterministic ascending lexicographical key order.
pub fn select_best_bucket<'a>(
    prefix: &str,
    buckets: impl IntoIterator<Item = (&'a str, &'a Value)>,
) -> Option<String> {
    select_best_bucket_with_hostname(
        prefix,
        std::env::var("COMPUTERNAME").ok().as_deref(),
        buckets,
    )
}

/// Selects the best bucket matching `prefix` using an explicit hostname override for ranking.
pub fn select_best_bucket_with_hostname<'a>(
    prefix: &str,
    local_hostname: Option<&str>,
    buckets: impl IntoIterator<Item = (&'a str, &'a Value)>,
) -> Option<String> {
    struct Candidate<'a> {
        key: &'a str,
        effective_time: Option<DateTime<Utc>>,
        last_updated: Option<DateTime<Utc>>,
        hostname_matches: bool,
    }

    let mut candidates: Vec<Candidate<'a>> = buckets
        .into_iter()
        .filter(|(key, _)| key.starts_with(prefix) && is_local_bucket(key))
        .map(|(key, val)| {
            let last_updated = parse_bucket_time(val, "last_updated");
            let created = parse_bucket_time(val, "created");
            let effective_time = last_updated.or(created);

            let hostname_matches = local_hostname.is_some_and(|local_h| {
                val.get("hostname")
                    .and_then(|h| h.as_str())
                    .is_some_and(|h| h.eq_ignore_ascii_case(local_h))
            });

            Candidate {
                key,
                effective_time,
                last_updated,
                hostname_matches,
            }
        })
        .collect();

    if candidates.is_empty() {
        return None;
    }

    candidates.sort_by(|a, b| {
        b.last_updated
            .cmp(&a.last_updated)
            .then_with(|| b.hostname_matches.cmp(&a.hostname_matches))
            .then_with(|| b.effective_time.cmp(&a.effective_time))
            .then_with(|| a.key.cmp(b.key))
    });

    candidates.first().map(|c| c.key.to_string())
}

/// Predicate checking if an ActivityWatch bucket key represents a local (non-synchronized) bucket.
pub fn is_local_bucket(key: &str) -> bool {
    !key.contains("-synced") && !key.contains("_synced")
}

/// Helper to parse an ISO 8601 / RFC 3339 timestamp from a JSON value field.
fn parse_bucket_time(val: &Value, field: &str) -> Option<DateTime<Utc>> {
    val.get(field)
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
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

    #[test]
    fn test_select_best_bucket_prefers_latest_timestamp() {
        let val_old = serde_json::json!({
            "id": "aw-watcher-window_OLD-PC",
            "hostname": "OLD-PC",
            "created": "2026-09-01T10:00:00Z",
            "last_updated": "2026-10-01T10:00:00Z"
        });
        let val_new = serde_json::json!({
            "id": "aw-watcher-window_NEW-PC",
            "hostname": "NEW-PC",
            "created": "2026-10-15T10:00:00Z",
            "last_updated": "2026-11-01T10:00:00Z"
        });
        let val_synced = serde_json::json!({
            "id": "aw-watcher-window_FUTURE-synced",
            "hostname": "FUTURE",
            "created": "2026-11-02T10:00:00Z",
            "last_updated": "2026-11-02T10:00:00Z"
        });

        let buckets = [
            ("aw-watcher-window_OLD-PC", &val_old),
            ("aw-watcher-window_FUTURE-synced", &val_synced),
            ("aw-watcher-window_NEW-PC", &val_new),
        ];

        let selected = select_best_bucket("aw-watcher-window_", buckets);
        assert_eq!(selected.as_deref(), Some("aw-watcher-window_NEW-PC"));
    }

    #[test]
    fn test_select_best_bucket_fallback_lexicographical() {
        let val1 = serde_json::json!({
            "created": "2026-10-01T10:00:00Z",
            "last_updated": "2026-10-01T10:00:00Z"
        });
        let val2 = serde_json::json!({
            "created": "2026-10-01T10:00:00Z",
            "last_updated": "2026-10-01T10:00:00Z"
        });

        let buckets = [
            ("aw-watcher-afk_ZED", &val1),
            ("aw-watcher-afk_ALPHA", &val2),
        ];

        let selected = select_best_bucket("aw-watcher-afk_", buckets);
        assert_eq!(selected.as_deref(), Some("aw-watcher-afk_ALPHA"));
    }

    #[test]
    fn test_select_best_bucket_created_timestamp_when_no_last_updated() {
        let val_old = serde_json::json!({
            "created": "2026-10-01T10:00:00Z"
        });
        let val_new = serde_json::json!({
            "created": "2026-10-25T10:00:00Z"
        });

        let buckets = [
            ("aw-watcher-window_EARLY", &val_old),
            ("aw-watcher-window_LATER", &val_new),
        ];

        let selected = select_best_bucket("aw-watcher-window_", buckets);
        assert_eq!(selected.as_deref(), Some("aw-watcher-window_LATER"));
    }

    #[test]
    fn test_select_best_bucket_prefers_hostname_when_no_last_updated() {
        // Bucket A matches COMPUTERNAME but has an older created timestamp
        let val_matching = serde_json::json!({
            "hostname": "MY-LOCAL-PC",
            "created": "2026-09-01T10:00:00Z"
        });
        // Bucket B does not match COMPUTERNAME but has a newer created timestamp
        let val_other = serde_json::json!({
            "hostname": "OTHER-PC",
            "created": "2026-10-25T10:00:00Z"
        });

        let buckets = [
            ("aw-watcher-window_OTHER", &val_other),
            ("aw-watcher-window_MATCHING", &val_matching),
        ];

        let selected = select_best_bucket_with_hostname(
            "aw-watcher-window_",
            Some("MY-LOCAL-PC"),
            buckets,
        );
        assert_eq!(selected.as_deref(), Some("aw-watcher-window_MATCHING"));
    }

    #[test]
    fn test_escape_bucket_id() {
        assert_eq!(escape_bucket_id("aw-watcher-window_HOST"), "aw-watcher-window_HOST");
        assert_eq!(
            escape_bucket_id("aw-watcher-web-chrome_\"test\"\\pc"),
            "aw-watcher-web-chrome_\\\"test\\\"\\\\pc"
        );
    }

    #[test]
    fn test_flood_bucket_expr() {
        assert_eq!(
            flood_bucket_expr("aw-watcher-window_LOCAL"),
            "flood(query_bucket(\"aw-watcher-window_LOCAL\"))"
        );
        assert_eq!(
            flood_bucket_expr("aw-watcher-web-chrome_\"quoted\""),
            "flood(query_bucket(\"aw-watcher-web-chrome_\\\"quoted\\\"\"))"
        );
    }

    #[test]
    fn test_discovered_buckets_from_bucket_map() {
        let mut map = serde_json::Map::new();
        map.insert(
            "aw-watcher-window_LOCAL".to_string(),
            serde_json::json!({ "created": "2026-11-01T10:00:00Z" }),
        );
        map.insert(
            "aw-watcher-afk_LOCAL".to_string(),
            serde_json::json!({ "created": "2026-11-01T10:00:00Z" }),
        );
        map.insert(
            "aw-watcher-web-firefox".to_string(),
            serde_json::json!({ "created": "2026-11-01T10:00:00Z" }),
        );
        map.insert(
            "aw-watcher-web-chrome".to_string(),
            serde_json::json!({ "created": "2026-11-01T10:00:00Z" }),
        );
        map.insert(
            "aw-watcher-web-edge_synced".to_string(),
            serde_json::json!({ "created": "2026-11-01T10:00:00Z" }),
        );

        let discovered = DiscoveredBuckets::from_bucket_map(&map);
        assert_eq!(discovered.window_bucket.as_deref(), Some("aw-watcher-window_LOCAL"));
        assert_eq!(discovered.afk_bucket.as_deref(), Some("aw-watcher-afk_LOCAL"));
        assert_eq!(
            discovered.web_buckets,
            vec!["aw-watcher-web-chrome".to_string(), "aw-watcher-web-firefox".to_string()]
        );
        assert!(discovered.is_ready());

        let mut partial_map = serde_json::Map::new();
        partial_map.insert(
            "aw-watcher-window_LOCAL".to_string(),
            serde_json::json!({ "created": "2026-11-01T10:00:00Z" }),
        );
        let partial_discovered = DiscoveredBuckets::from_bucket_map(&partial_map);
        assert!(!partial_discovered.is_ready());
    }

    #[test]
    fn test_build_web_query_expr_single_and_multi_buckets() {
        // 1. Empty slice returns None
        assert_eq!(build_web_query_expr(&[]), None);

        // 2. 1 bucket returns flood(query_bucket("aw-watcher-web-chrome"))
        let single = vec!["aw-watcher-web-chrome".to_string()];
        assert_eq!(
            build_web_query_expr(&single),
            Some("flood(query_bucket(\"aw-watcher-web-chrome\"))".to_string())
        );

        // 3. 2 buckets returns sort_by_timestamp(concat(flood(query_bucket("aw-watcher-web-brave")), flood(query_bucket("aw-watcher-web-chrome"))))
        let two = vec![
            "aw-watcher-web-brave".to_string(),
            "aw-watcher-web-chrome".to_string(),
        ];
        assert_eq!(
            build_web_query_expr(&two),
            Some("sort_by_timestamp(concat(flood(query_bucket(\"aw-watcher-web-brave\")), flood(query_bucket(\"aw-watcher-web-chrome\"))))".to_string())
        );

        // 4. 3+ buckets produces nested binary concat(concat(flood(...), flood(...)), flood(...)) calls and properly escapes quotes/backslashes
        let three = vec![
            "aw-watcher-web-brave".to_string(),
            "aw-watcher-web-chrome_\"test\"\\pc".to_string(),
            "aw-watcher-web-firefox".to_string(),
        ];
        assert_eq!(
            build_web_query_expr(&three),
            Some("sort_by_timestamp(concat(concat(flood(query_bucket(\"aw-watcher-web-brave\")), flood(query_bucket(\"aw-watcher-web-chrome_\\\"test\\\"\\\\pc\"))), flood(query_bucket(\"aw-watcher-web-firefox\"))))".to_string())
        );
    }

    #[tokio::test]
    async fn test_query_never_uses_find_bucket_when_buckets_none() {
        let client = AwClient::new(56999, "");
        assert_eq!(*client.window_bucket_id.read().await, None);
        assert_eq!(*client.afk_bucket_id.read().await, None);

        let now = Utc::now();
        let start = now - chrono::Duration::hours(1);

        // When buckets_verified is forced to true while window_bucket_id and afk_bucket_id are None,
        // it must return Ok(empty) immediately without constructing find_bucket or making network queries
        client.buckets_verified.store(true, Ordering::Relaxed);
        let res = client.query_intersected_events(start, now).await.unwrap();
        assert!(res.is_empty());

        // When only window_bucket_id is set but afk_bucket_id is None:
        *client.window_bucket_id.write().await = Some("aw-watcher-window_LOCAL".to_string());
        let res2 = client.query_intersected_events(start, now).await.unwrap();
        assert!(res2.is_empty());

        // When only afk_bucket_id is set but window_bucket_id is None:
        *client.window_bucket_id.write().await = None;
        *client.afk_bucket_id.write().await = Some("aw-watcher-afk_LOCAL".to_string());
        let res3 = client.query_intersected_events(start, now).await.unwrap();
        assert!(res3.is_empty());

        // For query_web_events:
        // Set last check to now so elapsed_60s is false
        *client.last_web_bucket_check.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        client.has_web_bucket.store(true, Ordering::Relaxed);
        *client.web_bucket_ids.write().await = vec!["aw-watcher-web-chrome_LOCAL".to_string()];
        *client.afk_bucket_id.write().await = None;
        // afk_bucket_id is None: must return empty without find_bucket fallback
        let web_res = client.query_web_events(start, now).await;
        assert!(web_res.is_empty());
    }

    #[test]
    fn test_aw_client_matches_config() {
        let client = AwClient::new(5600, "my-token");
        assert!(client.matches_config(5600, "my-token"));
        assert!(client.matches_config(5600, "  my-token  "));
        assert!(!client.matches_config(5601, "my-token"));
        assert!(!client.matches_config(5600, "other-token"));

        let client_empty = AwClient::new(5600, "   ");
        assert!(client_empty.matches_config(5600, ""));
        assert!(client_empty.matches_config(5600, "   "));
        assert!(!client_empty.matches_config(5600, "token"));
    }

    #[tokio::test]
    async fn test_query_web_events_empty_preserves_has_web_bucket() {
        let client = AwClient::new(56999, "");
        *client.last_web_bucket_check.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        client.buckets_verified.store(false, Ordering::Relaxed);
        client.has_web_bucket.store(true, Ordering::Relaxed);
        *client.web_bucket_ids.write().await = vec!["aw-watcher-web-chrome_LOCAL".to_string()];
        *client.afk_bucket_id.write().await = Some("aw-watcher-afk_LOCAL".to_string());

        let now = Utc::now();
        let start = now - chrono::Duration::hours(1);

        // When execute_query fails or returns empty (e.g. server offline), has_web_bucket remains true
        let events = client.query_web_events(start, now).await;
        assert!(events.is_empty());
        assert!(
            client.has_web_bucket.load(Ordering::Relaxed),
            "Empty web events must not falsely clear has_web_bucket"
        );
    }
}
