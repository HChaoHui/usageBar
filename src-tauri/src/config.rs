//! 配置持久化
//!
//! JSON 存储在 Tauri 的 app_config_dir()：
//! - macOS: ~/Library/Application Support/com.usagebar.desktop/config.json
//! - Linux: 系统配置目录下的 com.usagebar.desktop/config.json

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
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

#[derive(Clone, Serialize, Deserialize)]
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
            ensure_private_dir(parent)?;
        }
        fs::copy(legacy_path, path)?;
        secure_file_permissions(path)?;
        Ok(true)
    }

    /// 读取配置；解析失败回退到默认
    pub fn load(path: &Path) -> Self {
        if let Some(parent) = path.parent() {
            if let Err(error) = ensure_private_dir(parent) {
                eprintln!("usageBar: failed to secure config directory: {error}");
                return Self::default();
            }
        }
        if let Err(error) = secure_file_permissions(path) {
            eprintln!("usageBar: failed to secure config file: {error}");
            return Self::default();
        }
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
            ensure_private_dir(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        secure_file_permissions(path)?;

        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);

        let mut file = options.open(path)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()
    }

    pub fn redacted_for_frontend(&self) -> Self {
        let mut redacted = self.clone();
        for provider in &mut redacted.providers {
            let carries_credentials = matches!(
                provider.kind.as_str(),
                "minimax" | "deepseek" | "cpa_direct" | "cpa_keeper"
            ) || provider
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_some();
            if carries_credentials {
                if let Some(endpoint) = provider.endpoint.as_deref() {
                    if let Ok(mut url) = reqwest::Url::parse(endpoint) {
                        url.set_query(None);
                        url.set_fragment(None);
                        provider.endpoint = Some(url.to_string());
                    }
                }
            }
            provider.api_key = None;
        }
        redacted
    }

    pub fn find_provider_mut(&mut self, id: &str) -> Option<&mut ProviderConfig> {
        self.providers.iter_mut().find(|p| p.id == id)
    }

    pub fn find_provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.id == id)
    }
}

impl ProviderConfig {
    pub fn endpoint_change_requires_secret(&self, existing: &Self) -> bool {
        let endpoint_changed =
            self.endpoint.as_deref().map(str::trim) != existing.endpoint.as_deref().map(str::trim);
        let existing_has_secret = existing
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_some();
        let update_has_secret = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_some();
        endpoint_changed && existing_has_secret && !update_has_secret
    }

    pub fn preserve_secret_from(&mut self, existing: &Self) {
        let missing_secret = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none();
        if missing_secret {
            self.api_key = existing.api_key.clone();
        }
    }
}

fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn secure_file_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if path.exists() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_with_key(key: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            id: "provider".into(),
            kind: "minimax".into(),
            display_name: "Provider".into(),
            icon: "M".into(),
            color: "#000000".into(),
            unit: "%".into(),
            enabled: true,
            total: None,
            used: None,
            reset_at: None,
            endpoint: None,
            api_key: key.map(str::to_string),
            json_used: None,
            json_total: None,
            json_unit: None,
            json_reset: None,
            timeout_secs: None,
            path: None,
            auth_index: None,
            row_key: None,
            account_id: None,
            quota_window: None,
            currency: None,
        }
    }

    #[test]
    fn frontend_config_does_not_include_secrets() {
        let mut provider = provider_with_key(Some("private-key"));
        provider.endpoint = Some("https://api.example.com/usage?ticket=private#secret".into());
        let config = AppConfig {
            schema_version: 1,
            providers: vec![provider],
            refresh_interval_secs: 300,
        };

        let redacted = config.redacted_for_frontend();

        assert_eq!(redacted.providers[0].api_key, None);
        assert_eq!(
            redacted.providers[0].endpoint.as_deref(),
            Some("https://api.example.com/usage")
        );
        assert_eq!(config.providers[0].api_key.as_deref(), Some("private-key"));
    }

    #[test]
    fn blank_secret_update_preserves_existing_value() {
        let existing = provider_with_key(Some("private-key"));
        let mut update = provider_with_key(Some("  "));

        update.preserve_secret_from(&existing);

        assert_eq!(update.api_key.as_deref(), Some("private-key"));
    }

    #[test]
    fn endpoint_change_requires_secret_reentry() {
        let mut existing = provider_with_key(Some("private-key"));
        existing.endpoint = Some("https://api.example.com/usage".into());
        let mut update = provider_with_key(None);
        update.endpoint = Some("https://other.example.com/usage".into());

        assert!(update.endpoint_change_requires_secret(&existing));

        update.api_key = Some("replacement-key".into());
        assert!(!update.endpoint_change_requires_secret(&existing));
    }

    #[cfg(unix)]
    #[test]
    fn saved_config_is_private() {
        let test_dir = std::env::temp_dir().join(format!(
            "usagebar-config-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let path = test_dir.join("config.json");

        AppConfig::default().save(&path).unwrap();

        let initial_file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let initial_dir_mode = fs::metadata(&test_dir).unwrap().permissions().mode() & 0o777;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&test_dir, fs::Permissions::from_mode(0o755)).unwrap();

        AppConfig::load(&path);

        let loaded_file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let loaded_dir_mode = fs::metadata(&test_dir).unwrap().permissions().mode() & 0o777;
        fs::remove_dir_all(&test_dir).unwrap();
        assert_eq!(initial_file_mode, 0o600);
        assert_eq!(initial_dir_mode, 0o700);
        assert_eq!(loaded_file_mode, 0o600);
        assert_eq!(loaded_dir_mode, 0o700);
    }
}
