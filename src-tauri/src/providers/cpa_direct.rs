//! CLIProxyAPI direct Codex quota Provider.
//!
//! Calls CPA's management `api-call` endpoint. CPA replaces `$TOKEN$` with the
//! OAuth token belonging to `auth_index`, then proxies the Codex usage request.

use super::{network_error, validate_endpoint, Provider, ProviderError, Usage};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const API_CALL_PATH: &str = "/v0/management/api-call";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const FIVE_HOURS_SECONDS: i64 = 5 * 60 * 60;
const WEEK_SECONDS: i64 = 7 * 24 * 60 * 60;
const MONTH_SECONDS: [i64; 2] = [30 * 24 * 60 * 60, 365 * 24 * 60 * 60 / 12];
const MAX_ATTEMPTS: usize = 3;

#[derive(Clone, Serialize, Deserialize)]
pub struct CpaDirectProvider {
    pub id: String,
    pub display_name: String,
    pub icon: String,
    pub color: String,
    pub unit: String,
    /// CLIProxyAPI root URL, for example `http://127.0.0.1:8317`.
    pub base_url: String,
    /// CLIProxyAPI management key, not an API key from `api-keys`.
    pub management_key: String,
    pub auth_index: String,
    /// Optional ChatGPT account ID. Current Codex usage API also accepts no ID.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub account_id: Option<String>,
    /// `auto`, `five_hour`, `weekly`, or `monthly`.
    pub quota_window: String,
}

#[derive(Debug, Clone)]
struct QuotaCandidate {
    used_percent: f64,
    window_seconds: i64,
    reset_at: Option<DateTime<Utc>>,
}

#[async_trait]
impl Provider for CpaDirectProvider {
    async fn fetch(&self) -> Result<Usage, ProviderError> {
        let base = self.base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err(ProviderError::Other(
                "CLIProxyAPI address is required".into(),
            ));
        }
        if self.management_key.trim().is_empty() {
            return Err(ProviderError::Auth(
                "CLIProxyAPI management key is required".into(),
            ));
        }
        if self.auth_index.trim().is_empty() {
            return Err(ProviderError::Other(
                "CLIProxyAPI auth_index is required".into(),
            ));
        }

        let api_url = if base.ends_with(API_CALL_PATH) {
            base.to_string()
        } else {
            format!("{base}{API_CALL_PATH}")
        };
        validate_endpoint(&api_url, true)?;
        let mut upstream_headers = serde_json::Map::new();
        upstream_headers.insert(
            "Authorization".into(),
            serde_json::Value::String("Bearer $TOKEN$".into()),
        );
        upstream_headers.insert(
            "Content-Type".into(),
            serde_json::Value::String("application/json".into()),
        );
        upstream_headers.insert(
            "User-Agent".into(),
            serde_json::Value::String(
                "codex_cli_rs/0.76.0 (Debian 13.0.0; x86_64) WindowsTerminal".into(),
            ),
        );
        if let Some(account_id) = self
            .account_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            upstream_headers.insert(
                "Chatgpt-Account-Id".into(),
                serde_json::Value::String(account_id.into()),
            );
        }

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| ProviderError::Other("failed to initialize HTTP client".into()))?;
        let request_body = serde_json::json!({
            "authIndex": self.auth_index.trim(),
            "method": "GET",
            "url": CODEX_USAGE_URL,
            "header": upstream_headers,
        });
        let mut last_transient = String::new();

        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(retry_delay(attempt)).await;
            }
            let resp = match client
                .post(&api_url)
                .header("X-Management-Key", self.management_key.trim())
                .header("Content-Type", "application/json")
                .json(&request_body)
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(error) => {
                    last_transient = network_error(&error).to_string();
                    continue;
                }
            };

            let status = resp.status();
            let response_text = resp
                .text()
                .await
                .map_err(|_| ProviderError::Parse("failed to read CPA response".into()))?;
            if matches!(status.as_u16(), 401 | 403) {
                return Err(ProviderError::Auth(format!("CLIProxyAPI HTTP {status}")));
            }
            if !status.is_success() {
                let message = format!("CLIProxyAPI HTTP {status}");
                if transient_status(status.as_u16() as u64) {
                    last_transient = message;
                    continue;
                }
                return Err(ProviderError::Other(message));
            }

            let envelope: serde_json::Value = serde_json::from_str(&response_text)
                .map_err(|error| ProviderError::Parse(format!("invalid CPA response: {error}")))?;
            let upstream_status = envelope
                .get("status_code")
                .and_then(|value| value.as_u64())
                .ok_or_else(|| {
                    ProviderError::Parse("missing status_code in CPA response".into())
                })?;
            let body = envelope
                .get("body")
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    ProviderError::Parse("missing body string in CPA response".into())
                })?;

            if matches!(upstream_status, 401 | 403) {
                return Err(ProviderError::Auth(format!(
                    "Codex upstream HTTP {upstream_status}"
                )));
            }
            if !(200..300).contains(&upstream_status) {
                let message = format!("Codex upstream HTTP {upstream_status}");
                if transient_status(upstream_status) {
                    last_transient = message;
                    continue;
                }
                return Err(ProviderError::Other(message));
            }

            let usage_json: serde_json::Value = serde_json::from_str(body).map_err(|error| {
                ProviderError::Parse(format!("invalid Codex usage body: {error}"))
            })?;
            return parse_usage(&usage_json, &self.quota_window);
        }

        Err(ProviderError::Transient(format!(
            "{last_transient} (retried {MAX_ATTEMPTS} times)"
        )))
    }
}

fn transient_status(status: u64) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

fn retry_delay(attempt: usize) -> Duration {
    match attempt {
        1 => Duration::from_millis(400),
        _ => Duration::from_millis(1_200),
    }
}

fn parse_usage(json: &serde_json::Value, quota_window: &str) -> Result<Usage, ProviderError> {
    let rate_limit = json
        .get("rate_limit")
        .ok_or_else(|| ProviderError::Parse("missing rate_limit in Codex response".into()))?;
    let mut candidates = ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|key| rate_limit.get(key).and_then(parse_candidate))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(ProviderError::NotFound(
            "Codex response contains no quota windows".into(),
        ));
    }

    let selector = quota_window.trim();
    if !selector.is_empty() && selector != "auto" {
        candidates.retain(|candidate| window_matches(selector, candidate.window_seconds));
        if candidates.is_empty() {
            return Err(ProviderError::NotFound(format!(
                "requested {} window was not returned by Codex",
                selector_label(selector)
            )));
        }
    }

    let selected = candidates
        .into_iter()
        .max_by(|left, right| left.used_percent.total_cmp(&right.used_percent))
        .ok_or_else(|| ProviderError::NotFound("no matching Codex quota window".into()))?;

    Ok(Usage {
        used: selected.used_percent.clamp(0.0, 100.0),
        total: 100.0,
        unit: "%".into(),
        label: Some(window_label(selected.window_seconds).into()),
        reset_at: selected.reset_at,
        fetched_at: Some(Utc::now()),
        windows: vec![],
        balance: None,
    })
}

fn parse_candidate(value: &serde_json::Value) -> Option<QuotaCandidate> {
    if value.is_null() {
        return None;
    }
    let used_percent = number(value.get("used_percent")?)?;
    let window_seconds = number(value.get("limit_window_seconds")?)? as i64;
    let reset_at = value
        .get("reset_at")
        .and_then(number)
        .and_then(|value| DateTime::from_timestamp(value as i64, 0))
        .or_else(|| {
            value
                .get("reset_after_seconds")
                .and_then(number)
                .map(|seconds| Utc::now() + ChronoDuration::seconds(seconds as i64))
        });
    Some(QuotaCandidate {
        used_percent,
        window_seconds,
        reset_at,
    })
}

fn number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn window_matches(selector: &str, seconds: i64) -> bool {
    match selector {
        "five_hour" => seconds == FIVE_HOURS_SECONDS,
        "weekly" => seconds == WEEK_SECONDS,
        "monthly" => MONTH_SECONDS.contains(&seconds),
        _ => false,
    }
}

fn window_label(seconds: i64) -> &'static str {
    match seconds {
        FIVE_HOURS_SECONDS => "5 小时",
        WEEK_SECONDS => "7 天",
        seconds if MONTH_SECONDS.contains(&seconds) => "每月",
        _ => "额度",
    }
}

fn selector_label(selector: &str) -> &str {
    match selector {
        "five_hour" => "5-hour",
        "weekly" => "weekly",
        "monthly" => "monthly",
        _ => selector,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recognizes_weekly_primary_window_by_duration() {
        let usage = parse_usage(
            &json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 30,
                        "limit_window_seconds": 604800,
                        "reset_at": 1787018614
                    },
                    "secondary_window": null
                }
            }),
            "weekly",
        )
        .unwrap();

        assert_eq!(usage.used, 30.0);
        assert_eq!(usage.reset_at, DateTime::from_timestamp(1_787_018_614, 0));
    }

    #[test]
    fn auto_uses_tighter_window_regardless_of_position() {
        let usage = parse_usage(
            &json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 25,
                        "limit_window_seconds": 18000
                    },
                    "secondary_window": {
                        "used_percent": 72,
                        "limit_window_seconds": 604800
                    }
                }
            }),
            "auto",
        )
        .unwrap();

        assert_eq!(usage.used, 72.0);
    }

    #[test]
    fn reports_a_missing_requested_window() {
        let error = parse_usage(
            &json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 30,
                        "limit_window_seconds": 604800
                    },
                    "secondary_window": null
                }
            }),
            "five_hour",
        )
        .unwrap_err();

        assert!(matches!(error, ProviderError::NotFound(_)));
    }

    #[test]
    fn retries_common_proxy_and_upstream_failures() {
        for status in [408, 429, 500, 502, 503, 504] {
            assert!(transient_status(status));
        }
        for status in [400, 401, 403, 404] {
            assert!(!transient_status(status));
        }
    }
}
