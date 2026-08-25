//! CPA Usage Keeper Provider
//!
//! CLIProxyAPI (CPA) 自 v6.10.0 移除了内置持久化用量面板；其姊妹项目
//! [CPA Usage Keeper](https://github.com/Willxup/cpa-usage-keeper) 提供
//! 独立的 SQLite 持久化 + 仪表盘，并暴露 HTTP API。
//!
//! 本 Provider 调 Keeper 的 `POST /api/v1/quota/cache` 端点，按 `auth_index`
//! 请求指定账号的缓存 quota，找到匹配 `row_key` 的行，把 `usedPercent`
//! 直接作为进度值展示（used = percent, total = 100, unit = "%"）。
//!
//! Keeper 配置建议：`AUTH_ENABLED=false`（内网/本机使用）；如启用登录，
//! 需要先 POST `/api/v1/auth/login` 拿 `cpa_usage_keeper_session` cookie，
//! 再把完整 cookie 串作为 `api_key` 传进来。

use super::{Provider, ProviderError, Usage};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpaKeeperProvider {
    pub id: String,
    pub display_name: String,
    pub icon: String,
    pub color: String,
    pub unit: String,
    /// Keeper 根地址，如 `http://127.0.0.1:8080`
    pub base_url: String,
    /// 缓存端点路径，默认 `/api/v1/quota/cache`
    #[serde(default = "default_path")]
    pub path: String,
    /// Admin session cookie（Keeper 启用 AUTH_ENABLED 时使用）
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub api_key: Option<String>,
    /// CLIProxyAPI auth_index；当前 Keeper API 要求显式指定账号
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub auth_index: Option<String>,
    /// quota 行 key。Codex 的 primary/secondary 只是响应位置，实际周期需看
    /// `window.seconds`；Claude 使用 `five_hour` / `seven_day` 等稳定语义 key。
    pub row_key: String,
}

fn default_path() -> String {
    "/api/v1/quota/cache".to_string()
}

#[async_trait]
impl Provider for CpaKeeperProvider {
    async fn fetch(&self) -> Result<Usage, ProviderError> {
        let auth_index = self
            .auth_index
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ProviderError::Other(
                    "CPA Usage Keeper requires auth_index; get it from /api/v1/usage/identities"
                        .into(),
                )
            })?;
        let base = self.base_url.trim_end_matches('/');
        let path = if self.path.starts_with('/') {
            self.path.clone()
        } else {
            format!("/{}", self.path)
        };
        let url = format!("{}{}", base, path);

        let mut req = reqwest::Client::new()
            .post(&url)
            .header("Content-Type", "application/json")
            .header("X-CPA-Usage-Keeper-Request", "fetch")
            .json(&serde_json::json!({ "auth_indexes": [auth_index] }))
            .timeout(Duration::from_secs(15));
        if let Some(key) = &self.api_key {
            let key = key.trim();
            if !key.is_empty() {
                let cookie = if key.contains('=') {
                    key.to_string()
                } else {
                    format!("cpa_usage_keeper_session={key}")
                };
                req = req.header(reqwest::header::COOKIE, cookie);
            }
        }

        let resp = req
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ProviderError::Auth(format!(
                "HTTP {} (Keeper 鉴权失败；这里需要 Keeper session cookie，不是 CPA 管理密码)",
                status
            )));
        }
        if !status.is_success() {
            return Err(ProviderError::Other(format!("HTTP {}", status)));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Parse(e.to_string()))?;

        parse_usage(&json, auth_index, &self.row_key)
    }
}

fn parse_usage(
    json: &serde_json::Value,
    auth_index: &str,
    row_key: &str,
) -> Result<Usage, ProviderError> {
    let items = json
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ProviderError::Parse("missing items[] in response".into()))?;

    let item = items
        .iter()
        .find(|item| item.get("auth_index").and_then(|value| value.as_str()) == Some(auth_index))
        .ok_or_else(|| {
            ProviderError::NotFound(format!(
                "auth_index '{auth_index}' has no cached quota; refresh it in Keeper first"
            ))
        })?;

    if item.get("status").and_then(|value| value.as_str()) == Some("failed") {
        let message = item
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("quota refresh failed");
        let status = item
            .get("http_status_code")
            .and_then(|value| value.as_i64())
            .map(|value| format!(" (HTTP {value})"))
            .unwrap_or_default();
        return Err(ProviderError::Other(format!("{message}{status}")));
    }

    let quota_rows = item
        .get("quota")
        .and_then(|q| q.get("quota"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| ProviderError::Parse("missing quota.quota[]".into()))?;

    let row = quota_rows
        .iter()
        .find(|row| row.get("key").and_then(|value| value.as_str()) == Some(row_key))
        .ok_or_else(|| {
            ProviderError::NotFound(format!("row_key '{row_key}' not found in this account"))
        })?;

    let used_percent = row
        .get("usedPercent")
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
        .ok_or_else(|| {
            ProviderError::Parse(format!("usedPercent missing/not-number in row '{row_key}'"))
        })?;

    let reset_at = row
        .get("resetAt")
        .and_then(|value| value.as_str())
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc));

    Ok(Usage {
        used: used_percent.clamp(0.0, 100.0),
        total: 100.0,
        unit: "%".to_string(),
        label: keeper_window_label(row),
        reset_at,
        fetched_at: Some(Utc::now()),
        windows: vec![],
        balance: None,
        reset_credits: None,
        codex_account: None,
    })
}

fn keeper_window_label(row: &serde_json::Value) -> Option<String> {
    let seconds = row
        .get("window")
        .and_then(|window| window.get("seconds"))
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        });
    match seconds {
        Some(18_000) => Some("5 小时".into()),
        Some(604_800) => Some("7 天".into()),
        Some(2_592_000 | 2_628_000) => Some("每月".into()),
        _ => row
            .get("label")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_current_keeper_cache_response() {
        let usage = parse_usage(
            &json!({
                "items": [{
                    "auth_index": "0123456789abcdef",
                    "status": "completed",
                    "quota": {
                        "quota": [{
                            "key": "rate_limit.primary_window",
                            "usedPercent": 64,
                            "resetAt": "2026-05-11T21:11:11+08:00"
                        }]
                    }
                }]
            }),
            "0123456789abcdef",
            "rate_limit.primary_window",
        )
        .unwrap();

        assert_eq!(usage.used, 64.0);
        assert_eq!(usage.total, 100.0);
        assert!(usage.reset_at.is_some());
    }

    #[test]
    fn surfaces_cached_failure() {
        let error = parse_usage(
            &json!({
                "items": [{
                    "auth_index": "0123456789abcdef",
                    "status": "failed",
                    "error": "invalid credential",
                    "http_status_code": 401
                }]
            }),
            "0123456789abcdef",
            "rate_limit.primary_window",
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid credential"));
    }
}
