//! 手动 Provider：用户自行维护已用 / 总额度
//!
//! 适用：没有公开 API 的服务（如 OpenCode Go、个人配额池）
//! 每次 `fetch()` 直接返回缓存值；用户可在 UI 中编辑

use super::{Provider, ProviderError, Usage};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualProvider {
    pub id: String,
    pub display_name: String,
    pub icon: String,
    pub color: String,
    pub unit: String,
    pub total: f64,
    pub used: f64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reset_at: Option<DateTime<Utc>>,
}

#[async_trait]
impl Provider for ManualProvider {
    async fn fetch(&self) -> Result<Usage, ProviderError> {
        Ok(Usage {
            used: self.used,
            total: self.total,
            unit: self.unit.clone(),
            label: None,
            reset_at: self.reset_at,
            fetched_at: Some(Utc::now()),
            windows: vec![],
            balance: None,
        })
    }
}
