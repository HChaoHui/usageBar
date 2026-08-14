//! MiniMax (MiniMax) 订阅用量 Provider
//!
//! Endpoint: GET https://www.minimaxi.com/v1/token_plan/remains
//! Auth: Authorization: Bearer <订阅 Key>
//!
//! 返回 model_remains 数组，每个元素对应一个 model (general / video / ...)。
//! 编程套餐以 general 为主，并同时受 5 小时窗口和周窗口限制。

use super::{
    network_error, secure_http_client, validate_endpoint, Provider, ProviderError, Usage,
    UsageWindow,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const MINIMAX_ENDPOINT: &str = "https://www.minimaxi.com/v1/token_plan/remains";

#[derive(Clone, Serialize, Deserialize)]
pub struct MinimaxProvider {
    pub id: String,
    pub display_name: String,
    pub icon: String,
    pub color: String,
    pub unit: String,
    pub api_key: String,
    pub endpoint: String,
}

#[derive(Debug, Clone)]
struct QuotaCandidate {
    key: &'static str,
    label: String,
    used_percent: f64,
    reset_at: Option<DateTime<Utc>>,
}

struct QuotaWindowSpec {
    key: &'static str,
    label: &'static str,
    remaining_percent_keys: &'static [&'static str],
    total_keys: &'static [&'static str],
    remaining_count_keys: &'static [&'static str],
    status_keys: &'static [&'static str],
    reset_keys: &'static [&'static str],
}

const FIVE_HOUR_WINDOW: QuotaWindowSpec = QuotaWindowSpec {
    key: "five_hour",
    label: "5 小时",
    remaining_percent_keys: &[
        "current_interval_remaining_percent",
        "currentIntervalRemainingPercent",
    ],
    total_keys: &["current_interval_total_count", "currentIntervalTotalCount"],
    remaining_count_keys: &["current_interval_usage_count", "currentIntervalUsageCount"],
    status_keys: &["current_interval_status", "currentIntervalStatus"],
    reset_keys: &["end_time", "endTime"],
};

const WEEKLY_WINDOW: QuotaWindowSpec = QuotaWindowSpec {
    key: "weekly",
    label: "7 天",
    remaining_percent_keys: &[
        "current_weekly_remaining_percent",
        "currentWeeklyRemainingPercent",
        "weekly_remaining_percent",
    ],
    total_keys: &["current_weekly_total_count", "currentWeeklyTotalCount"],
    remaining_count_keys: &["current_weekly_usage_count", "currentWeeklyUsageCount"],
    status_keys: &["current_weekly_status", "currentWeeklyStatus"],
    reset_keys: &["weekly_end_time", "weeklyEndTime"],
};

#[async_trait]
impl Provider for MinimaxProvider {
    async fn fetch(&self) -> Result<Usage, ProviderError> {
        let endpoint = if self.endpoint.trim().is_empty() {
            MINIMAX_ENDPOINT
        } else {
            self.endpoint.trim()
        };
        validate_endpoint(endpoint, true)?;
        let resp = secure_http_client()?
            .get(endpoint)
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("MM-API-Source", "usageBar")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|error| network_error(&error))?;

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ProviderError::Auth(format!("HTTP {}", status)));
        }
        if !status.is_success() {
            return Err(ProviderError::Other(format!("HTTP {}", status)));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;

        parse_usage(&json)
    }
}

fn parse_usage(json: &serde_json::Value) -> Result<Usage, ProviderError> {
    let base_resp = json
        .get("base_resp")
        .or_else(|| json.get("data").and_then(|data| data.get("base_resp")));
    if let Some(code) = base_resp
        .and_then(|resp| resp.get("status_code"))
        .and_then(number)
        .map(|code| code as i64)
    {
        if code != 0 {
            let detail = format!("MiniMax API error (code={code})");
            return if matches!(code, 1004 | 2049) {
                Err(ProviderError::Auth(detail))
            } else {
                Err(ProviderError::Other(detail))
            };
        }
    }

    let entries = json
        .get("model_remains")
        .or_else(|| json.get("modelRemains"))
        .or_else(|| json.get("data").and_then(|data| data.get("model_remains")))
        .or_else(|| json.get("data").and_then(|data| data.get("modelRemains")))
        .and_then(|entries| entries.as_array())
        .ok_or_else(|| ProviderError::Parse("missing model_remains array".into()))?;

    if entries.is_empty() {
        return Err(ProviderError::Parse("empty model_remains array".into()));
    }

    let mut general = None;
    let mut non_video = None;
    let mut any = None;

    for entry in entries {
        let candidates = candidates_for_entry(entry);
        if candidates.is_empty() {
            continue;
        }
        let model_name = entry
            .get("model_name")
            .or_else(|| entry.get("modelName"))
            .or_else(|| entry.get("model"))
            .and_then(|name| name.as_str())
            .unwrap_or("");

        for candidate in candidates {
            update_window(&mut any, candidate.clone());
            if !model_name.eq_ignore_ascii_case("video") {
                update_window(&mut non_video, candidate.clone());
            }
            if model_name.eq_ignore_ascii_case("general") {
                update_window(&mut general, candidate);
            }
        }
    }

    let candidates = select_windows(general.as_deref(), non_video.as_deref(), any.as_deref())
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(ProviderError::Parse(
            "no bounded MiniMax quota window found".into(),
        ));
    }
    let selected = candidates
        .iter()
        .max_by(|left, right| left.used_percent.total_cmp(&right.used_percent))
        .cloned()
        .ok_or_else(|| ProviderError::Parse("no bounded MiniMax quota window found".into()))?;
    let windows = candidates
        .into_iter()
        .map(|candidate| UsageWindow {
            key: candidate.key.into(),
            label: candidate.label.clone(),
            used: candidate.used_percent,
            total: 100.0,
            unit: "%".into(),
            reset_at: candidate.reset_at,
        })
        .collect();

    Ok(Usage {
        used: selected.used_percent,
        total: 100.0,
        unit: "%".into(),
        label: Some(selected.label),
        reset_at: selected.reset_at,
        fetched_at: Some(Utc::now()),
        windows,
        balance: None,
    })
}

fn candidates_for_entry(entry: &serde_json::Value) -> Vec<QuotaCandidate> {
    if unavailable_plan(entry) {
        return vec![];
    }
    let interval = quota_window(entry, &FIVE_HOUR_WINDOW);
    let weekly = quota_window(entry, &WEEKLY_WINDOW);

    [interval, weekly].into_iter().flatten().collect()
}

fn quota_window(entry: &serde_json::Value, spec: &QuotaWindowSpec) -> Option<QuotaCandidate> {
    let status = field(entry, spec.status_keys)
        .and_then(number)
        .map(|value| value as i64);
    if status == Some(0) {
        return None;
    }

    let used_percent = if status == Some(2) {
        100.0
    } else if let Some(remaining) = field(entry, spec.remaining_percent_keys).and_then(number) {
        100.0 - remaining.clamp(0.0, 100.0)
    } else if status == Some(3) {
        0.0
    } else {
        let total = field(entry, spec.total_keys).and_then(number)?;
        let remaining = field(entry, spec.remaining_count_keys).and_then(number)?;
        if total <= 0.0 {
            return None;
        }
        ((total - remaining) / total * 100.0).clamp(0.0, 100.0)
    };

    Some(QuotaCandidate {
        key: spec.key,
        label: if status == Some(3) {
            format!("{} · 无限", spec.label)
        } else {
            spec.label.into()
        },
        used_percent: used_percent.clamp(0.0, 100.0),
        reset_at: field(entry, spec.reset_keys).and_then(timestamp),
    })
}

fn unavailable_plan(entry: &serde_json::Value) -> bool {
    let interval_status = field(entry, &["current_interval_status", "currentIntervalStatus"])
        .and_then(number)
        .map(|value| value as i64);
    let weekly_status = field(entry, &["current_weekly_status", "currentWeeklyStatus"])
        .and_then(number)
        .map(|value| value as i64);
    let interval_total = field(
        entry,
        &["current_interval_total_count", "currentIntervalTotalCount"],
    )
    .and_then(number)
    .unwrap_or(0.0);
    let weekly_total = field(
        entry,
        &["current_weekly_total_count", "currentWeeklyTotalCount"],
    )
    .and_then(number)
    .unwrap_or(0.0);
    interval_status == Some(3)
        && weekly_status == Some(3)
        && interval_total <= 0.0
        && weekly_total <= 0.0
}

fn select_windows(
    general: Option<&[QuotaCandidate]>,
    non_video: Option<&[QuotaCandidate]>,
    any: Option<&[QuotaCandidate]>,
) -> [Option<QuotaCandidate>; 2] {
    ["five_hour", "weekly"].map(|key| {
        [general, non_video, any]
            .into_iter()
            .flatten()
            .find_map(|candidates| {
                candidates
                    .iter()
                    .find(|candidate| candidate.key == key)
                    .cloned()
            })
    })
}

fn field<'a>(entry: &'a serde_json::Value, keys: &[&str]) -> Option<&'a serde_json::Value> {
    keys.iter().find_map(|key| entry.get(key))
}

fn update_window(current: &mut Option<Vec<QuotaCandidate>>, candidate: QuotaCandidate) {
    let candidates = current.get_or_insert_with(Vec::new);
    if let Some(existing) = candidates
        .iter_mut()
        .find(|existing| existing.key == candidate.key)
    {
        if candidate.used_percent > existing.used_percent {
            *existing = candidate;
        }
    } else {
        candidates.push(candidate);
    }
}

fn number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn timestamp(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    let value = number(value)? as i64;
    if value.abs() >= 10_000_000_000 {
        DateTime::from_timestamp_millis(value)
    } else {
        DateTime::from_timestamp(value, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn returns_both_general_windows_and_keeps_tighter_summary() {
        let usage = parse_usage(&json!({
            "base_resp": { "status_code": 0 },
            "model_remains": [
                {
                    "model_name": "general",
                    "current_interval_remaining_percent": 80,
                    "end_time": 1_700_000_000_000_i64,
                    "current_weekly_remaining_percent": 25,
                    "weekly_end_time": 1_800_000_000_000_i64
                },
                {
                    "model_name": "video",
                    "current_interval_remaining_percent": 1,
                    "end_time": 1_900_000_000_000_i64
                }
            ]
        }))
        .unwrap();

        assert_eq!(usage.used, 75.0);
        assert_eq!(usage.total, 100.0);
        assert_eq!(usage.unit, "%");
        assert_eq!(usage.label.as_deref(), Some("7 天"));
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[0].label, "5 小时");
        assert_eq!(usage.windows[0].used, 20.0);
        assert_eq!(usage.windows[1].label, "7 天");
        assert_eq!(usage.windows[1].used, 75.0);
        assert_eq!(
            usage.reset_at,
            DateTime::from_timestamp_millis(1_800_000_000_000_i64)
        );
    }

    #[test]
    fn accepts_wrapped_numeric_strings_and_inverts_remaining_count() {
        let usage = parse_usage(&json!({
            "data": {
                "base_resp": { "status_code": "0" },
                "model_remains": [{
                    "model_name": "general",
                    "current_interval_total_count": "1000",
                    "current_interval_usage_count": "250",
                    "end_time": "1700000000000",
                    "current_weekly_status": "3"
                }]
            }
        }))
        .unwrap();

        assert_eq!(usage.used, 75.0);
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[0].label, "5 小时");
        assert_eq!(usage.windows[1].label, "7 天 · 无限");
        assert_eq!(usage.windows[1].used, 0.0);
        assert_eq!(
            usage.reset_at,
            DateTime::from_timestamp_millis(1_700_000_000_000_i64)
        );
    }

    #[test]
    fn keeps_unlimited_status_weekly_window_visible() {
        let usage = parse_usage(&json!({
            "model_remains": [{
                "model_name": "general",
                "current_interval_status": 2,
                "current_interval_remaining_percent": 0,
                "end_time": 1_700_000_000_000_i64,
                "current_weekly_status": 3,
                "current_weekly_remaining_percent": 100,
                "weekly_end_time": 1_800_000_000_000_i64
            }]
        }))
        .unwrap();

        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[1].label, "7 天 · 无限");
        assert_eq!(usage.windows[1].used, 0.0);
    }

    #[test]
    fn fills_missing_general_weekly_window_from_another_non_video_model() {
        let usage = parse_usage(&json!({
            "model_remains": [
                {
                    "model_name": "general",
                    "current_interval_remaining_percent": 80,
                    "end_time": 1_700_000_000_000_i64
                },
                {
                    "model_name": "coding",
                    "current_weekly_status": 1,
                    "current_weekly_remaining_percent": 45,
                    "weekly_end_time": 1_800_000_000_000_i64
                }
            ]
        }))
        .unwrap();

        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[0].label, "5 小时");
        assert_eq!(usage.windows[1].label, "7 天");
        assert_eq!(usage.windows[1].used, 55.0);
    }

    #[test]
    fn maps_in_band_login_failure_to_auth_error() {
        let error = parse_usage(&json!({
            "base_resp": {
                "status_code": "1004",
                "status_msg": "login fail"
            }
        }))
        .unwrap_err();

        assert!(matches!(error, ProviderError::Auth(_)));
    }

    #[test]
    fn rejects_unlimited_only_response() {
        let error = parse_usage(&json!({
            "model_remains": [{
                "model_name": "general",
                "current_interval_status": 3,
                "current_weekly_status": 3
            }]
        }))
        .unwrap_err();

        assert!(matches!(error, ProviderError::Parse(_)));
    }

    #[test]
    fn ignores_inactive_weekly_window() {
        let usage = parse_usage(&json!({
            "model_remains": [{
                "model_name": "general",
                "current_interval_status": 1,
                "current_interval_remaining_percent": 80,
                "current_weekly_status": 0,
                "current_weekly_remaining_percent": 0
            }]
        }))
        .unwrap();

        assert_eq!(usage.used, 20.0);
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].label, "5 小时");
    }
}
