//! 配置持久化
//!
//! JSON 存储在 Tauri 的 app_config_dir()：
//! - macOS: ~/Library/Application Support/com.usagebar.desktop/config.json
//! - Linux: 系统配置目录下的 com.usagebar.desktop/config.json

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    /// Provider 类型：`"manual"` / `"http"` / `"minimax"` / `"cpa_direct"` 等
    #[serde(rename = "type")]
    pub kind: String,
    pub display_name: String,
    pub icon: String,
    pub color: String,
    pub unit: String,
    pub enabled: bool,

    // ----- manual 类型字段 -----
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub total: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reset_at: Option<DateTime<Utc>>,

    // ----- http / 真实 API 类型字段（Phase 4+ 使用） -----
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub json_used: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub json_total: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub json_unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub json_reset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timeout_secs: Option<u64>,

    // ----- cpa_keeper 专属 -----
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub auth_index: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub row_key: Option<String>,

    // ----- CLIProxyAPI 直连 Codex 专属 -----
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub quota_window: Option<String>,

    // ----- DeepSeek 余额专属 -----
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub currency: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub schema_version: u32,
    pub providers: Vec<ProviderConfig>,
    pub refresh_interval_secs: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            providers: vec![],
            refresh_interval_secs: 300,
        }
    }
}

impl AppConfig {
    pub fn migrate_legacy(path: &Path) -> io::Result<bool> {
        if path.exists() {
            return Ok(false);
        }
        let Some(config_root) = path.parent().and_then(Path::parent) else {
            return Ok(false);
        };
        let legacy_path = config_root.join("com.usagebar.app").join("config.json");
        if !legacy_path.exists() {
            return Ok(false);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(legacy_path, path)?;
        Ok(true)
    }

    /// 读取配置；解析失败回退到默认
    pub fn load(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(content) => match serde_json::from_str::<AppConfig>(&content) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("usageBar: config parse failed: {e}, using defaults");
                    Self::default()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                eprintln!("usageBar: config read failed: {e}, using defaults");
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(path, json)
    }

    pub fn find_provider_mut(&mut self, id: &str) -> Option<&mut ProviderConfig> {
        self.providers.iter_mut().find(|p| p.id == id)
    }

    pub fn find_provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.id == id)
    }
}
