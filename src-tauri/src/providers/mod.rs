//! Provider 适配器模块：每个 AI 订阅服务实现 `Provider` trait
//!
//! 所有 Provider 都通过统一的 `fetch()` 接口返回 `Usage`，前端只关心 Usage 结构。

pub mod cpa_direct;
pub mod cpa_keeper;
pub mod deepseek;
pub mod http;
pub mod manual;
pub mod minimax;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::ProviderConfig;
pub use cpa_direct::CpaDirectProvider;
pub use cpa_keeper::CpaKeeperProvider;
pub use deepseek::DeepSeekProvider;
pub use http::HttpProvider;
pub use manual::ManualProvider;
pub use minimax::MinimaxProvider;

/// 单个订阅的用量快照
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageWindow {
    pub key: String,
    pub label: String,
    pub used: f64,
    pub total: f64,
    pub unit: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reset_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceDetails {
    pub currency: String,
    pub total: f64,
    pub granted: f64,
    pub topped_up: f64,
    pub available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetCreditEntry {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetCreditDetails {
    pub available_count: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub applicable_available_count: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub credits: Vec<ResetCreditEntry>,
    pub immediate_reset_purchase_eligible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexAccountDetails {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub plan_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub subscription_active_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub used: f64,
    pub total: f64,
    pub unit: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reset_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fetched_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub windows: Vec<UsageWindow>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub balance: Option<BalanceDetails>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reset_credits: Option<ResetCreditDetails>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub codex_account: Option<CodexAccountDetails>,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("network error: {0}")]
    Network(String),
    #[error("temporary error: {0}")]
    Transient(String),
    #[error("authentication error: {0}")]
    Auth(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("other: {0}")]
    Other(String),
}

impl ProviderError {
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

/// Provider 接口
#[async_trait]
pub trait Provider: Send + Sync {
    async fn fetch(&self) -> Result<Usage, ProviderError>;
}

/// Provider 快照（送往前端的数据）
#[derive(Debug, Clone, Serialize)]
pub struct ProviderSnapshot {
    pub id: String,
    pub kind: String,
    pub display_name: String,
    pub icon: String,
    pub color: String,
    pub unit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 根据配置构造具体 Provider 实例
pub fn build_provider(pc: &ProviderConfig) -> Option<Box<dyn Provider>> {
    match pc.kind.as_str() {
        "manual" => Some(Box::new(ManualProvider {
            id: pc.id.clone(),
            display_name: pc.display_name.clone(),
            icon: pc.icon.clone(),
            color: pc.color.clone(),
            unit: pc.unit.clone(),
            total: pc.total.unwrap_or(0.0),
            used: pc.used.unwrap_or(0.0),
            reset_at: pc.reset_at,
        })),
        "http" => Some(Box::new(HttpProvider {
            id: pc.id.clone(),
            display_name: pc.display_name.clone(),
            icon: pc.icon.clone(),
            color: pc.color.clone(),
            unit: pc.unit.clone(),
            endpoint: pc.endpoint.clone().unwrap_or_default(),
            api_key: pc.api_key.clone(),
            json_used: pc.json_used.clone().unwrap_or_else(|| "used".into()),
            json_total: pc.json_total.clone().unwrap_or_else(|| "total".into()),
            json_unit: pc.json_unit.clone(),
            json_reset_at: pc.json_reset.clone(),
            timeout_secs: pc.timeout_secs,
        })),
        "minimax" => Some(Box::new(MinimaxProvider {
            id: pc.id.clone(),
            display_name: pc.display_name.clone(),
            icon: pc.icon.clone(),
            color: pc.color.clone(),
            unit: pc.unit.clone(),
            api_key: pc.api_key.clone().unwrap_or_default(),
            endpoint: pc.endpoint.clone().unwrap_or_default(),
        })),
        "deepseek" => Some(Box::new(DeepSeekProvider {
            id: pc.id.clone(),
            display_name: pc.display_name.clone(),
            icon: pc.icon.clone(),
            color: pc.color.clone(),
            unit: pc.unit.clone(),
            api_key: pc.api_key.clone().unwrap_or_default(),
            endpoint: pc.endpoint.clone().unwrap_or_default(),
            currency: pc.currency.clone().unwrap_or_else(|| "CNY".into()),
        })),
        "cpa_direct" => {
            let base_url = pc.endpoint.clone().unwrap_or_default();
            if base_url.is_empty() {
                return None;
            }
            Some(Box::new(CpaDirectProvider {
                id: pc.id.clone(),
                display_name: pc.display_name.clone(),
                icon: pc.icon.clone(),
                color: pc.color.clone(),
                unit: pc.unit.clone(),
                base_url,
                management_key: pc.api_key.clone().unwrap_or_default(),
                auth_index: pc.auth_index.clone().unwrap_or_default(),
                account_id: pc.account_id.clone(),
                quota_window: pc.quota_window.clone().unwrap_or_else(|| "auto".into()),
            }))
        }
        "cpa_keeper" => {
            let base_url = pc.endpoint.clone().unwrap_or_default();
            if base_url.is_empty() {
                return None;
            }
            let row_key = pc
                .row_key
                .clone()
                .unwrap_or_else(|| "rate_limit.primary_window".into());
            let path = match pc.path.as_deref().map(str::trim) {
                None | Some("") | Some("/quota/cache") => "/api/v1/quota/cache".into(),
                Some(path) => path.to_string(),
            };
            Some(Box::new(CpaKeeperProvider {
                id: pc.id.clone(),
                display_name: pc.display_name.clone(),
                icon: pc.icon.clone(),
                color: pc.color.clone(),
                unit: pc.unit.clone(),
                base_url,
                path,
                api_key: pc.api_key.clone(),
                auth_index: pc.auth_index.clone(),
                row_key,
            }))
        }
        _ => None,
    }
}
