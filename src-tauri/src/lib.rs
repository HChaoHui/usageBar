mod config;
mod providers;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, LogicalSize, Manager, Size, WebviewWindow,
};
use tauri_plugin_positioner::{on_tray_event, Position, WindowExt};

use crate::config::{AppConfig, ProviderConfig};
use crate::providers::{build_provider, validate_endpoint, ProviderSnapshot, Usage};

/// 单个 Provider 的缓存快照
#[derive(Debug, Clone)]
pub struct CachedUsage {
    pub usage: Option<Usage>,
    pub error: Option<String>,
    pub fetched_at: DateTime<Utc>,
}

/// 全局应用状态
pub struct AppState {
    pub config: RwLock<AppConfig>,
    pub config_path: PathBuf,
    pub usage_cache: RwLock<HashMap<String, CachedUsage>>,
}

// ===================== Tauri commands =====================

#[tauri::command]
fn get_config(state: tauri::State<AppState>) -> AppConfig {
    state.config.read().unwrap().redacted_for_frontend()
}

#[tauri::command]
fn update_config(
    state: tauri::State<AppState>,
    refresh_interval_secs: Option<u64>,
) -> Result<(), String> {
    let mut cfg = state.config.write().unwrap();
    if let Some(secs) = refresh_interval_secs {
        if secs < 30 {
            return Err("刷新间隔不能小于 30 秒".into());
        }
        cfg.refresh_interval_secs = secs;
    }
    cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn list_providers(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ProviderSnapshot>, String> {
    let cfg = state.config.read().unwrap().clone();
    let mut snapshots = Vec::new();

    for pc in cfg.providers.iter().filter(|p| p.enabled) {
        // 优先用 cache；cache miss 则即时 fetch
        let cached = {
            let cache = state.usage_cache.read().unwrap();
            cache.get(&pc.id).cloned()
        };
        let entry = match cached {
            Some(e) => e,
            None => match build_provider(pc) {
                Some(provider) => {
                    let result = provider.fetch().await;
                    let e = CachedUsage {
                        usage: result.as_ref().ok().cloned(),
                        error: result.err().map(|err| err.to_string()),
                        fetched_at: Utc::now(),
                    };
                    state
                        .usage_cache
                        .write()
                        .unwrap()
                        .insert(pc.id.clone(), e.clone());
                    e
                }
                None => CachedUsage {
                    usage: None,
                    error: Some(format!("unknown provider kind: {}", pc.kind)),
                    fetched_at: Utc::now(),
                },
            },
        };

        snapshots.push(ProviderSnapshot {
            id: pc.id.clone(),
            kind: pc.kind.clone(),
            display_name: pc.display_name.clone(),
            icon: pc.icon.clone(),
            color: pc.color.clone(),
            unit: pc.unit.clone(),
            usage: entry.usage,
            error: entry.error,
        });
    }
    Ok(snapshots)
}

#[tauri::command]
async fn refresh_now(app: tauri::AppHandle) -> Result<(), String> {
    fetch_all_into_cache(&app).await;
    let _ = app.emit("usagebar-updated", ());
    Ok(())
}

#[tauri::command]
async fn update_manual_used(
    state: tauri::State<'_, AppState>,
    id: String,
    used: f64,
) -> Result<(), String> {
    // 1. 更新 config
    {
        let mut cfg = state.config.write().unwrap();
        let pc = cfg
            .find_provider_mut(&id)
            .ok_or_else(|| format!("provider not found: {id}"))?;
        if pc.kind != "manual" {
            return Err(format!("provider {id} is not manual (kind={})", pc.kind));
        }
        pc.used = Some(used.max(0.0));
        cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    }

    // 2. 立即重 fetch 该 provider 写回 cache
    let cfg = state.config.read().unwrap().clone();
    if let Some(pc) = cfg.find_provider(&id) {
        if let Some(provider) = build_provider(pc) {
            let result = provider.fetch().await;
            let entry = CachedUsage {
                usage: result.as_ref().ok().cloned(),
                error: result.err().map(|err| err.to_string()),
                fetched_at: Utc::now(),
            };
            state.usage_cache.write().unwrap().insert(id, entry);
        }
    }
    Ok(())
}

#[tauri::command]
fn add_provider(state: tauri::State<AppState>, provider: ProviderConfig) -> Result<(), String> {
    validate_provider_config(&provider)?;
    let mut cfg = state.config.write().unwrap();
    if cfg.find_provider(&provider.id).is_some() {
        return Err(format!("provider id already exists: {}", provider.id));
    }
    cfg.providers.push(provider);
    cfg.save(&state.config_path).map_err(|e| e.to_string())
}

#[tauri::command]
fn update_provider(
    state: tauri::State<AppState>,
    mut provider: ProviderConfig,
    clear_api_key: Option<bool>,
) -> Result<(), String> {
    {
        let mut cfg = state.config.write().unwrap();
        let existing = cfg
            .find_provider_mut(&provider.id)
            .ok_or_else(|| format!("provider not found: {}", provider.id))?;
        if provider.kind != existing.kind {
            return Err("provider type cannot be changed".into());
        }
        let clear_api_key = clear_api_key.unwrap_or(false);
        if !clear_api_key && provider.endpoint_change_requires_secret(existing) {
            return Err("修改 API 地址时需重新填写凭据".into());
        }
        if clear_api_key {
            provider.api_key = None;
        } else {
            provider.preserve_secret_from(existing);
        }
        validate_provider_config(&provider)?;
        *existing = provider.clone();
        cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    }
    state.usage_cache.write().unwrap().remove(&provider.id);
    Ok(())
}

fn validate_provider_config(provider: &ProviderConfig) -> Result<(), String> {
    let Some(endpoint) = provider.endpoint.as_deref().map(str::trim) else {
        return Ok(());
    };
    if endpoint.is_empty() {
        return Ok(());
    }
    let carries_credentials = matches!(
        provider.kind.as_str(),
        "minimax" | "deepseek" | "cpa_direct" | "cpa_keeper"
    ) || provider
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some();
    validate_endpoint(endpoint, carries_credentials).map_err(|error| error.to_string())
}

#[tauri::command]
fn remove_provider(state: tauri::State<AppState>, id: String) -> Result<(), String> {
    {
        let mut cfg = state.config.write().unwrap();
        let before = cfg.providers.len();
        cfg.providers.retain(|p| p.id != id);
        if cfg.providers.len() == before {
            return Err(format!("provider not found: {id}"));
        }
        cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    }
    state.usage_cache.write().unwrap().remove(&id);
    Ok(())
}

// 窗口控制
#[tauri::command]
fn toggle_window(window: WebviewWindow) -> Result<bool, String> {
    let visible = window.is_visible().unwrap_or(false);
    if visible {
        window.hide().map_err(|e| e.to_string())?;
        Ok(false)
    } else {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        Ok(true)
    }
}

#[tauri::command]
fn hide_window(window: WebviewWindow) -> Result<(), String> {
    window.hide().map_err(|e| e.to_string())
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn resize_window_to_content(window: WebviewWindow, content_height: f64) -> Result<f64, String> {
    let scale_factor = window.scale_factor().map_err(|error| error.to_string())?;
    let monitor_height = window
        .current_monitor()
        .map_err(|error| error.to_string())?
        .map(|monitor| monitor.size().height as f64 / scale_factor)
        .unwrap_or(900.0);
    let max_height = (monitor_height - 64.0).clamp(320.0, 900.0);
    let target_height = content_height.ceil().clamp(180.0, max_height);
    window
        .set_size(Size::Logical(LogicalSize::new(340.0, target_height)))
        .map_err(|error| error.to_string())?;
    let _ = window.move_window(Position::TrayBottomCenter);
    Ok(target_height)
}

// ===================== Scheduler =====================

/// 触发一次完整拉取，结果写入 cache；UI 下次 list_providers 即可读到
async fn fetch_all_into_cache(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let cfg = state.config.read().unwrap().clone();

    for pc in cfg.providers.iter().filter(|p| p.enabled) {
        if let Some(provider) = build_provider(pc) {
            let id = pc.id.clone();
            let fetched_at = Utc::now();
            let previous = state.usage_cache.read().unwrap().get(&id).cloned();
            let result = provider.fetch().await;
            let entry = match result {
                Ok(usage) => CachedUsage {
                    usage: Some(usage),
                    error: None,
                    fetched_at,
                },
                Err(error) if error.is_transient() => previous.unwrap_or_else(|| CachedUsage {
                    usage: None,
                    error: Some(error.to_string()),
                    fetched_at,
                }),
                Err(error) => CachedUsage {
                    usage: None,
                    error: Some(error.to_string()),
                    fetched_at,
                },
            };
            let mut cache = state.usage_cache.write().unwrap();
            cache.insert(id, entry);
        }
    }
}

fn scheduler_loop(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        // 启动后稍等再开始第一次拉取（让前端 ready）
        tokio::time::sleep(Duration::from_secs(2)).await;
        loop {
            // 读 interval（可能用户改了）
            let interval_secs = {
                let state = app.state::<AppState>();
                let cfg = state.config.read().unwrap();
                cfg.refresh_interval_secs.max(30)
            };

            fetch_all_into_cache(&app).await;
            let _ = app.emit("usagebar-updated", ());

            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        }
    });
}

// ===================== App entry =====================

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .invoke_handler(tauri::generate_handler![
            toggle_window,
            hide_window,
            quit_app,
            resize_window_to_content,
            get_config,
            update_config,
            list_providers,
            refresh_now,
            update_manual_used,
            add_provider,
            update_provider,
            remove_provider,
        ])
        .setup(|app| {
            // macOS: 作为菜单栏应用运行，不显示 Dock 图标。
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // 配置加载
            let config_dir = app
                .path()
                .app_config_dir()
                .expect("failed to get app config dir");
            let config_path = config_dir.join("config.json");
            match AppConfig::migrate_legacy(&config_path) {
                Ok(true) => eprintln!("usageBar: migrated legacy config"),
                Ok(false) => {}
                Err(error) => eprintln!("usageBar: legacy config migration failed: {error}"),
            }
            let config = AppConfig::load(&config_path);

            // 状态注入
            app.manage(AppState {
                config: RwLock::new(config),
                config_path,
                usage_cache: RwLock::new(HashMap::new()),
            });

            // 启动后台调度
            scheduler_loop(app.handle().clone());

            // popup 窗口：阻止 close（关闭即隐藏，而非退出应用）
            let window = app
                .get_webview_window("main")
                .expect("failed to get main window");
            let window_clone = window.clone();
            window.on_window_event(move |event| match event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window_clone.hide();
                }
                tauri::WindowEvent::Focused(false) => {
                    let _ = window_clone.hide();
                }
                _ => {}
            });

            // tray 右键菜单
            let show_item = MenuItem::with_id(app, "show", "显示面板", true, None::<&str>)?;
            let refresh_item = MenuItem::with_id(app, "refresh", "立即刷新", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let quit_item = MenuItem::with_id(app, "quit", "退出 usageBar", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &refresh_item, &sep, &quit_item])?;

            // tray 图标
            let _tray = TrayIconBuilder::with_id("main-tray")
                .icon(
                    app.default_window_icon()
                        .expect("missing default icon")
                        .clone(),
                )
                .icon_as_template(true)
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("usageBar — AI 订阅用量")
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "quit" => app.exit(0),
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.move_window(Position::TrayBottomCenter);
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "refresh" => {
                        // 直接触发后端刷新
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            fetch_all_into_cache(&app).await;
                            let _ = app.emit("usagebar-updated", ());
                        });
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // 必须把事件转发给 positioner，让它追踪 tray 图标位置
                    on_tray_event(tray.app_handle(), &event);

                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let visible = window.is_visible().unwrap_or(false);
                            if visible {
                                let _ = window.hide();
                            } else {
                                // 定位到 tray 图标正下方，跨屏幕自动跟随
                                let _ = window.move_window(Position::TrayBottomCenter);
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
