//! 通用 HTTP Provider：任何返回 JSON 的余额接口都能配
//!
//! 用法：用户填 endpoint + JSONPath（点号分隔）指向 used / total 字段
//! 可选 Bearer Token 鉴权、可选 unit / reset_at 路径

use super::{network_error, secure_http_client, validate_endpoint, Provider, ProviderError, Usage};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Serialize, Deserialize)]
pub struct HttpProvider {
    pub id: String,
    pub display_name: String,
    pub icon: String,
    pub color: String,
    pub unit: String,
    pub endpoint: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub api_key: Option<String>,
    pub json_used: String,
    pub json_total: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub json_unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub json_reset_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timeout_secs: Option<u64>,
}

/// 从 JSON 中按点号路径取值，如 "data.credits.used"
fn resolve_path<'a>(json: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = json;
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        cur = cur.get(segment)?;
    }
    Some(cur)
}

/// 把任意 JSON 数字解析成 f64
fn as_f64(v: &serde_json::Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_i64().map(|n| n as f64))
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
}

/// 把 JSON 字符串/数字解析成 ISO datetime；返回 None 表示路径不存在或解析失败
fn parse_reset_at(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    use chrono::DateTime;
    if let Some(s) = v.as_str() {
        // 尝试 RFC3339 / ISO8601
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    } else if let Some(n) = v.as_i64() {
        // 尝试当作 unix 秒
        DateTime::from_timestamp(n, 0)
    } else {
        None
    }
}

#[async_trait]
impl Provider for HttpProvider {
    async fn fetch(&self) -> Result<Usage, ProviderError> {
        let timeout = Duration::from_secs(self.timeout_secs.unwrap_or(15));
        let carries_credentials = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .is_some();
        validate_endpoint(&self.endpoint, carries_credentials)?;
        let mut req = secure_http_client()?.get(&self.endpoint).timeout(timeout);
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                req = req.bearer_auth(key);
            }
        }

        let resp = req.send().await.map_err(|error| network_error(&error))?;

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

        let used = resolve_path(&json, &self.json_used)
            .ok_or_else(|| ProviderError::Parse(format!("json path not found: {}", self.json_used)))
            .and_then(|v| {
                as_f64(v).ok_or_else(|| {
                    ProviderError::Parse(format!("value at '{}' is not a number", self.json_used))
                })
            })?;

        let total = resolve_path(&json, &self.json_total)
            .ok_or_else(|| {
                ProviderError::Parse(format!("json path not found: {}", self.json_total))
            })
            .and_then(|v| {
                as_f64(v).ok_or_else(|| {
                    ProviderError::Parse(format!("value at '{}' is not a number", self.json_total))
                })
            })?;

        let unit = self
            .json_unit
            .as_ref()
            .and_then(|p| resolve_path(&json, p))
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| self.unit.clone());

        let reset_at = self
            .json_reset_at
            .as_ref()
            .and_then(|p| resolve_path(&json, p))
            .and_then(parse_reset_at);

        Ok(Usage {
            used,
            total,
            unit,
            label: None,
            reset_at,
            fetched_at: Some(Utc::now()),
            windows: vec![],
            balance: None,
        })
    }
}
