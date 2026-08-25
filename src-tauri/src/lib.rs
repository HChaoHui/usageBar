mod config;
mod providers;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, LogicalSize, Manager, Size, WebviewWindow,
};
use tauri_plugin_positioner::{on_tray_event, Position, WindowExt};
use tokio::sync::Mutex as AsyncMutex;

use crate::config::{AppConfig, ProviderConfig};
use crate::providers::cpa_direct::{discover_codex_accounts, CpaCodexAccount};
use crate::providers::{build_provider, CpaDirectProvider, ProviderSnapshot, Usage};

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
    pub provider_locks: RwLock<HashMap<String, Arc<AsyncMutex<()>>>>,
    pub reset_in_flight: Mutex<HashSet<String>>,
}

fn provider_lock(state: &AppState, id: &str) -> Arc<AsyncMutex<()>> {
    let mut locks = state.provider_locks.write().unwrap();
    locks
        .entry(id.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

fn provider_config_is_current(state: &AppState, expected: &ProviderConfig) -> bool {
    state.config.read().unwrap().find_provider(&expected.id) == Some(expected)
}

fn provider_operation_key(config: &ProviderConfig) -> String {
    if config.kind != "cpa_direct" {
        return config.id.clone();
    }
    let endpoint = config
        .endpoint
        .as_deref()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .strip_suffix("/v0/management/api-call")
        .or_else(|| {
            config
                .endpoint
                .as_deref()
                .unwrap_or_default()
                .trim()
                .trim_end_matches('/')
                .strip_suffix("/v0/management")
        })
        .unwrap_or_else(|| {
            config
                .endpoint
                .as_deref()
                .unwrap_or_default()
                .trim()
                .trim_end_matches('/')
        });
    let auth_index = config.auth_index.as_deref().unwrap_or_default().trim();
    if endpoint.is_empty() || auth_index.is_empty() {
        config.id.clone()
    } else {
        format!("cpa_direct:{endpoint}:{auth_index}")
    }
}

struct ResetInFlightGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    key: String,
}

impl Drop for ResetInFlightGuard<'_> {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.key);
    }
}

fn begin_reset<'a>(state: &'a AppState, key: String) -> Result<ResetInFlightGuard<'a>, String> {
    if !state.reset_in_flight.lock().unwrap().insert(key.clone()) {
        return Err("该 Codex 账号正在执行完整重置".into());
    }
    Ok(ResetInFlightGuard {
        set: &state.reset_in_flight,
        key,
    })
}

// ===================== Tauri commands =====================

#[tauri::command]
fn get_config(state: tauri::State<AppState>) -> AppConfig {
    state.config.read().unwrap().clone()
}

#[tauri::command]
fn update_config(
    state: tauri::State<AppState>,
    refresh_interval_secs: Option<u64>,
) -> Result<AppConfig, String> {
    let mut cfg = state.config.write().unwrap();
    if let Some(secs) = refresh_interval_secs {
        if secs < 30 {
            return Err("刷新间隔不能小于 30 秒".into());
        }
        cfg.refresh_interval_secs = secs;
    }
    cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    Ok(cfg.clone())
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
        let entry = if let Some(entry) = cached {
            entry
        } else {
            let operation_key = provider_operation_key(pc);
            let lock = provider_lock(state.inner(), &operation_key);
            let _guard = lock.lock().await;
            let cached_after_lock = {
                let cache = state.usage_cache.read().unwrap();
                cache.get(&pc.id).cloned()
            };
            if let Some(entry) = cached_after_lock {
                entry
            } else {
                let entry = match build_provider(pc) {
                    Some(provider) => {
                        let result = provider.fetch().await;
                        CachedUsage {
                            usage: result.as_ref().ok().cloned(),
                            error: result.err().map(|err| err.to_string()),
                            fetched_at: Utc::now(),
                        }
                    }
                    None => CachedUsage {
                        usage: None,
                        error: Some(format!("unknown provider kind: {}", pc.kind)),
                        fetched_at: Utc::now(),
                    },
                };
                if !provider_config_is_current(state.inner(), pc) {
                    continue;
                }
                state
                    .usage_cache
                    .write()
                    .unwrap()
                    .insert(pc.id.clone(), entry.clone());
                entry
            }
        };
        if !provider_config_is_current(state.inner(), pc) {
            continue;
        }

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
async fn discover_cpa_codex_accounts(
    endpoint: String,
    api_key: String,
) -> Result<Vec<CpaCodexAccount>, String> {
    discover_codex_accounts(&endpoint, &api_key)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn consume_cpa_codex_reset(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let config = state
        .config
        .read()
        .unwrap()
        .find_provider(&id)
        .cloned()
        .ok_or_else(|| format!("provider not found: {id}"))?;
    if config.kind != "cpa_direct" {
        return Err("provider is not CLIProxyAPI Codex".into());
    }
    let operation_key = provider_operation_key(&config);
    let provider = CpaDirectProvider {
        id: config.id.clone(),
        display_name: config.display_name,
        icon: config.icon,
        color: config.color,
        unit: config.unit,
        base_url: config.endpoint.unwrap_or_default(),
        management_key: config.api_key.unwrap_or_default(),
        auth_index: config.auth_index.unwrap_or_default(),
        account_id: config.account_id,
        quota_window: config.quota_window.unwrap_or_else(|| "auto".into()),
    };
    let _reset_guard = begin_reset(state.inner(), operation_key.clone())?;
    let lock = provider_lock(state.inner(), &operation_key);
    let result = async {
        let _guard = lock.lock().await;
        provider
            .consume_reset_credit()
            .await
            .map_err(|error| error.to_string())?;
        let affected_ids = state
            .config
            .read()
            .unwrap()
            .providers
            .iter()
            .filter(|config| provider_operation_key(config) == operation_key)
            .map(|config| config.id.clone())
            .collect::<HashSet<_>>();
        state
            .usage_cache
            .write()
            .unwrap()
            .retain(|provider_id, _| !affected_ids.contains(provider_id));
        Ok(())
    }
    .await;

    if result.is_ok() {
        let _ = app.emit("usagebar-updated", ());
    }
    result
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
        let operation_key = provider_operation_key(pc);
        let lock = provider_lock(state.inner(), &operation_key);
        let _guard = lock.lock().await;
        if let Some(provider) = build_provider(pc) {
            let result = provider.fetch().await;
            let entry = CachedUsage {
                usage: result.as_ref().ok().cloned(),
                error: result.err().map(|err| err.to_string()),
                fetched_at: Utc::now(),
            };
            if provider_config_is_current(state.inner(), pc) {
                state.usage_cache.write().unwrap().insert(id, entry);
            }
        }
    }
    Ok(())
}

#[tauri::command]
async fn add_provider(
    state: tauri::State<'_, AppState>,
    provider: ProviderConfig,
) -> Result<(), String> {
    let operation_key = provider_operation_key(&provider);
    let lock = provider_lock(state.inner(), &operation_key);
    let _guard = lock.lock().await;
    let mut cfg = state.config.write().unwrap();
    if cfg.find_provider(&provider.id).is_some() {
        return Err(format!("provider id already exists: {}", provider.id));
    }
    cfg.providers.push(provider);
    cfg.save(&state.config_path).map_err(|e| e.to_string())
}

#[tauri::command]
async fn update_provider(
    state: tauri::State<'_, AppState>,
    provider: ProviderConfig,
) -> Result<(), String> {
    let previous = state
        .config
        .read()
        .unwrap()
        .find_provider(&provider.id)
        .cloned()
        .ok_or_else(|| format!("provider not found: {}", provider.id))?;
    let mut operation_keys = vec![
        provider_operation_key(&previous),
        provider_operation_key(&provider),
    ];
    operation_keys.sort();
    operation_keys.dedup();
    let mut guards = Vec::with_capacity(operation_keys.len());
    for operation_key in operation_keys {
        let lock = provider_lock(state.inner(), &operation_key);
        guards.push(lock.lock_owned().await);
    }
    {
        let mut cfg = state.config.write().unwrap();
        let existing = cfg
            .find_provider_mut(&provider.id)
            .ok_or_else(|| format!("provider not found: {}", provider.id))?;
        if *existing != previous {
            return Err("provider changed while waiting; retry the update".into());
        }
        *existing = provider.clone();
        cfg.save(&state.config_path).map_err(|e| e.to_string())?;
    }
    state.usage_cache.write().unwrap().remove(&provider.id);
    Ok(())
}

#[tauri::command]
async fn remove_provider(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    let previous = state
        .config
        .read()
        .unwrap()
        .find_provider(&id)
        .cloned()
        .ok_or_else(|| format!("provider not found: {id}"))?;
    let operation_key = provider_operation_key(&previous);
    let lock = provider_lock(state.inner(), &operation_key);
    let _guard = lock.lock().await;
    {
        let mut cfg = state.config.write().unwrap();
        if cfg.find_provider(&id) != Some(&previous) {
            return Err("provider changed while waiting; retry the removal".into());
        }
        cfg.providers.retain(|p| p.id != id);
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

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! usageBar Phase 5", name)
}

// ===================== Scheduler =====================

/// 触发一次完整拉取，结果写入 cache；UI 下次 list_providers 即可读到
async fn fetch_all_into_cache(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let cfg = state.config.read().unwrap().clone();

    for pc in cfg.providers.iter().filter(|p| p.enabled) {
        if let Some(provider) = build_provider(pc) {
            let id = pc.id.clone();
            let operation_key = provider_operation_key(pc);
            let lock = provider_lock(state.inner(), &operation_key);
            let _guard = lock.lock().await;
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
            if !provider_config_is_current(state.inner(), pc) {
                continue;
            }
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
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_positioner::init())
        .invoke_handler(tauri::generate_handler![
            greet,
            toggle_window,
            hide_window,
            quit_app,
            resize_window_to_content,
            get_config,
            update_config,
            list_providers,
            refresh_now,
            discover_cpa_codex_accounts,
            consume_cpa_codex_reset,
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
            eprintln!("usageBar: config path = {}", config_path.display());
            let config = AppConfig::load(&config_path);

            // 状态注入
            app.manage(AppState {
                config: RwLock::new(config),
                config_path,
                usage_cache: RwLock::new(HashMap::new()),
                provider_locks: RwLock::new(HashMap::new()),
                reset_in_flight: Mutex::new(HashSet::new()),
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
