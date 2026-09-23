//! ClinePass subscription Provider backed by usageBar's independent Cline login.
//!
//! Endpoints: GET /api/v1/users/me/plan/usage-limits (required), /api/v1/users/me
//! and /api/v1/users/me/plan (optional details), /api/v1/users/{id}/usages/daily
//! (token chart) with /api/v1/users/{id}/usages as fallback. Auth: `Bearer workos:<token>`.

use super::{ClinePassAccountDetails, ClineUsageDay, Provider, ProviderError, Usage, UsageWindow};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

const API_ORIGIN: &str = "https://api.cline.bot";
const ME_PATH: &str = "/api/v1/users/me";
const PLAN_PATH: &str = "/api/v1/users/me/plan";
const LIMITS_PATH: &str = "/api/v1/users/me/plan/usage-limits";
const REQUEST_TIMEOUT_SECS: u64 = 15;
/// 图表数据是可选信息，超时设置更短，避免拖慢卡片刷新
const USAGE_REQUEST_TIMEOUT_SECS: u64 = 8;
/// 图表最多展示最近多少天的用量
const USAGE_CHART_DAYS: usize = 30;
/// 退回明细流水时最多取多少条
const USAGE_PAGE_LIMIT: u32 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClinePassProvider {
    pub id: String,
    pub display_name: String,
    pub icon: String,
    pub color: String,
    pub unit: String,
    /// 折叠时显示的额度窗口：`five_hour`（默认）/ `weekly` / `monthly` / `auto`
    pub quota_window: String,
}

#[derive(Default, Clone)]
struct AccountInfo {
    id: Option<String>,
    email: Option<String>,
    name: Option<String>,
}

#[derive(Default, Clone)]
struct PlanInfo {
    name: Option<String>,
    period_end: Option<DateTime<Utc>>,
}

#[async_trait]
impl Provider for ClinePassProvider {
    async fn fetch(&self) -> Result<Usage, ProviderError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ProviderError::Other("无法创建 ClinePass 请求客户端".into()))?;
        // Serialize refresh rotation and requests for this card's account only.
        let mut session = crate::cline_auth::session(&self.id)
            .await
            .map_err(ProviderError::Auth)?;
        let token = session.token(false).await.map_err(ProviderError::Auth)?;
        match fetch_usage(&client, &token, &self.quota_window).await {
            Err(ProviderError::Auth(_)) => {
                let token = session.token(true).await.map_err(ProviderError::Auth)?;
                fetch_usage(&client, &token, &self.quota_window).await
            }
            result => result,
        }
    }
}

async fn fetch_usage(
    client: &Client,
    token: &str,
    quota_window: &str,
) -> Result<Usage, ProviderError> {
    let (limits, me, plan) = tokio::join!(
        get_json(client, token, LIMITS_PATH),
        get_json(client, token, ME_PATH),
        get_json(client, token, PLAN_PATH),
    );
    let windows = parse_limits(&limits?)?;
    if windows.is_empty() {
        return Err(ProviderError::Parse(
            "ClinePass 未返回可显示的额度窗口".into(),
        ));
    }
    let account = me.ok().as_ref().and_then(parse_account);
    let plan = plan.ok().as_ref().and_then(parse_plan);
    let usage_chart = match account.as_ref().and_then(|account| account.id.as_deref()) {
        Some(user_id) => fetch_usage_chart(client, token, user_id).await,
        None => UsageChart {
            error: Some("未获取到账号 ID".into()),
            ..UsageChart::default()
        },
    };
    Ok(build_usage(
        windows,
        account,
        plan,
        usage_chart,
        quota_window,
    ))
}

#[derive(Default)]
struct UsageChart {
    days: Vec<ClineUsageDay>,
    raw_items: u32,
    error: Option<String>,
}

/// 优先使用按天聚合接口，失败或为空时退回分页明细流水。
async fn fetch_usage_chart(client: &Client, token: &str, user_id: &str) -> UsageChart {
    let encoded = encode_path_segment(user_id);
    let (start_date, end_date) = usage_date_range(USAGE_CHART_DAYS);
    let daily_path =
        format!("/api/v1/users/{encoded}/usages/daily?startDate={start_date}&endDate={end_date}");
    let daily_error = match request_json(client, token, &daily_path).await {
        Ok(json) => {
            let raw_items = raw_item_count(&json);
            let days = parse_daily_usage_days(&json);
            if !days.is_empty() {
                return UsageChart {
                    days,
                    raw_items,
                    error: None,
                };
            }
            None
        }
        Err(error) => Some(describe_error(&error)),
    };
    let list_path = format!("/api/v1/users/{encoded}/usages?limit={USAGE_PAGE_LIMIT}");
    match request_json(client, token, &list_path).await {
        Ok(json) => {
            let raw_items = raw_item_count(&json);
            UsageChart {
                days: parse_usage_days(&json),
                raw_items,
                error: if raw_items == 0 { daily_error } else { None },
            }
        }
        Err(error) => UsageChart {
            days: Vec::new(),
            raw_items: 0,
            error: Some(describe_error(&error)),
        },
    }
}

async fn request_json(client: &Client, token: &str, path: &str) -> Result<Value, ProviderError> {
    let request = tokio::time::timeout(
        Duration::from_secs(USAGE_REQUEST_TIMEOUT_SECS),
        get_json(client, token, path),
    );
    match request.await {
        Ok(result) => result,
        Err(_) => Err(ProviderError::Network("ClinePass 请求超时".into())),
    }
}

fn describe_error(error: &ProviderError) -> String {
    match error {
        ProviderError::Network(_) => "网络请求失败或超时".into(),
        ProviderError::Auth(_) => "登录已失效".into(),
        ProviderError::Parse(_) => "响应解析失败".into(),
        ProviderError::Transient(_) => "接口临时不可用".into(),
        ProviderError::NotFound(_) => "接口不存在".into(),
        ProviderError::Other(message) => message.trim_start_matches("ClinePass ").to_string(),
    }
}

fn raw_item_count(json: &Value) -> u32 {
    items_from(json)
        .map(|items| items.len() as u32)
        .unwrap_or(0)
}

fn items_from(json: &Value) -> Option<&Vec<Value>> {
    json.pointer("/data/items")
        .and_then(Value::as_array)
        .or_else(|| json.get("items").and_then(Value::as_array))
}

fn usage_date_range(days: usize) -> (String, String) {
    let end = Utc::now().date_naive();
    let span = chrono::Duration::days(days.saturating_sub(1) as i64);
    let start = end - span;
    (
        start.format("%Y-%m-%d").to_string(),
        end.format("%Y-%m-%d").to_string(),
    )
}

fn encode_path_segment(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character.to_string()
            } else {
                character
                    .to_string()
                    .bytes()
                    .map(|byte| format!("%{byte:02X}"))
                    .collect()
            }
        })
        .collect()
}

async fn get_json(client: &Client, token: &str, path: &str) -> Result<Value, ProviderError> {
    let response = client
        .get(format!("{API_ORIGIN}{path}"))
        .header("Accept", "application/json")
        .header("User-Agent", "usageBar")
        .bearer_auth(format!("workos:{token}"))
        .send()
        .await
        .map_err(|error| sanitized_network_error(&error))?;
    parse_response(response).await
}

fn sanitized_network_error(error: &reqwest::Error) -> ProviderError {
    let detail = if error.is_timeout() {
        "请求超时"
    } else if error.is_connect() {
        "连接失败"
    } else {
        "请求失败"
    };
    ProviderError::Network(format!("ClinePass {detail}"))
}

async fn parse_response(response: Response) -> Result<Value, ProviderError> {
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err(auth_error());
    }
    if !status.is_success() {
        return Err(ProviderError::Other(format!(
            "ClinePass 接口返回 HTTP {status}"
        )));
    }
    let json: Value = response
        .json()
        .await
        .map_err(|_| ProviderError::Parse("ClinePass 返回了无效 JSON".into()))?;
    if json.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(ProviderError::Other("ClinePass 接口返回失败状态".into()));
    }
    Ok(json)
}

fn auth_error() -> ProviderError {
    ProviderError::Auth("ClinePass 登录已失效，请在设置中重新登录".into())
}

fn parse_limits(json: &Value) -> Result<Vec<UsageWindow>, ProviderError> {
    let entries = json
        .pointer("/data/limits")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Parse("ClinePass 额度响应缺少 limits".into()))?;
    let mut windows = Vec::new();
    for entry in entries {
        let (key, label) = match entry.get("type").and_then(Value::as_str) {
            Some("five_hour") => ("five_hour", "5 小时"),
            Some("weekly") => ("weekly", "每周"),
            Some("monthly") => ("monthly", "每月"),
            _ => continue,
        };
        let Some(used) = entry.get("percentUsed").and_then(number) else {
            continue;
        };
        windows.push(UsageWindow {
            key: key.into(),
            label: label.into(),
            used: used.clamp(0.0, 100.0),
            total: 100.0,
            unit: "%".into(),
            reset_at: entry.get("resetsAt").and_then(timestamp),
        });
    }
    windows.sort_by_key(|window| match window.key.as_str() {
        "five_hour" => 0,
        "weekly" => 1,
        _ => 2,
    });
    Ok(windows)
}

fn parse_account(json: &Value) -> Option<AccountInfo> {
    let data = json.get("data")?.as_object()?;
    Some(AccountInfo {
        id: text(data, "id"),
        email: text(data, "email"),
        name: text(data, "displayName").or_else(|| text(data, "name")),
    })
}

fn parse_plan(json: &Value) -> Option<PlanInfo> {
    let data = json.get("data")?.as_object()?;
    let plan = data.get("plan").and_then(Value::as_object);
    let name = plan
        .and_then(|plan| text(plan, "displayName").or_else(|| text(plan, "name")))
        .or_else(|| text(data, "displayName").or_else(|| text(data, "name")));
    Some(PlanInfo {
        name,
        period_end: data.get("currentPeriodEnd").and_then(timestamp),
    })
}

fn select_primary<'a>(windows: &'a [UsageWindow], preference: &str) -> Option<&'a UsageWindow> {
    if preference != "auto" {
        if let Some(window) = windows.iter().find(|window| window.key == preference) {
            return Some(window);
        }
    }
    windows
        .iter()
        .max_by(|left, right| left.used.total_cmp(&right.used))
}

fn build_usage(
    windows: Vec<UsageWindow>,
    account: Option<AccountInfo>,
    plan: Option<PlanInfo>,
    usage_chart: UsageChart,
    quota_window: &str,
) -> Usage {
    let (used, label, reset_at) = match select_primary(&windows, quota_window) {
        Some(window) => (window.used, Some(window.label.clone()), window.reset_at),
        None => (0.0, Some("ClinePass 额度".into()), None),
    };
    let account = account.unwrap_or_default();
    let plan = plan.unwrap_or_default();
    Usage {
        used,
        total: 100.0,
        unit: "%".into(),
        label,
        reset_at,
        fetched_at: Some(Utc::now()),
        windows,
        balance: None,
        reset_credits: None,
        codex_account: None,
        mcode_account: None,
        cline_account: Some(ClinePassAccountDetails {
            account_id: account.id,
            account_email: account.email,
            account_name: account.name,
            plan_name: plan.name,
            current_period_end: plan.period_end,
            usage_raw_items: (usage_chart.raw_items > 0).then_some(usage_chart.raw_items),
            usage_error: usage_chart.error,
        }),
        cline_usage: usage_chart.days,
    }
}

/// 把 `/api/v1/users/{id}/usages/daily` 的 items 聚合为按天数据。
fn parse_daily_usage_days(json: &Value) -> Vec<ClineUsageDay> {
    let Some(items) = items_from(json) else {
        return Vec::new();
    };
    let mut by_day: BTreeMap<String, ClineUsageDay> = BTreeMap::new();
    for item in items {
        let Some(date) = item.get("date").and_then(normalize_date) else {
            continue;
        };
        let prompt_tokens = item.get("promptTokens").and_then(number).unwrap_or(0.0);
        let completion_tokens = item.get("completionTokens").and_then(number).unwrap_or(0.0);
        let tokens = item
            .get("totalTokens")
            .and_then(number)
            .unwrap_or(prompt_tokens + completion_tokens)
            .max(prompt_tokens + completion_tokens)
            .max(0.0);
        let credits = item
            .get("creditsUsed")
            .and_then(number)
            .unwrap_or(0.0)
            .max(0.0);
        let cost_usd = item.get("costUsd").and_then(number).unwrap_or(0.0).max(0.0);
        accumulate_usage(
            &mut by_day,
            date,
            prompt_tokens,
            completion_tokens,
            tokens,
            credits,
            cost_usd,
        );
    }
    finish_usage_days(by_day)
}

/// 把 `/api/v1/users/{id}/usages` 的明细流水按 UTC 天聚合，只保留最近 [`USAGE_CHART_DAYS`] 天。
fn parse_usage_days(json: &Value) -> Vec<ClineUsageDay> {
    let Some(items) = items_from(json) else {
        return Vec::new();
    };
    let mut by_day: BTreeMap<String, ClineUsageDay> = BTreeMap::new();
    for item in items {
        let Some(created_at) = item
            .get("createdAt")
            .and_then(Value::as_str)
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        else {
            continue;
        };
        let prompt_tokens = item.get("promptTokens").and_then(number).unwrap_or(0.0);
        let completion_tokens = item.get("completionTokens").and_then(number).unwrap_or(0.0);
        let tokens = item
            .get("totalTokens")
            .and_then(number)
            .unwrap_or(prompt_tokens + completion_tokens)
            .max(prompt_tokens + completion_tokens)
            .max(0.0);
        let credits = item
            .get("creditsUsed")
            .and_then(number)
            .unwrap_or(0.0)
            .max(0.0);
        let cost_usd = item.get("costUsd").and_then(number).unwrap_or(0.0).max(0.0);
        let date = created_at
            .with_timezone(&Utc)
            .format("%Y-%m-%d")
            .to_string();
        accumulate_usage(
            &mut by_day,
            date,
            prompt_tokens,
            completion_tokens,
            tokens,
            credits,
            cost_usd,
        );
    }
    finish_usage_days(by_day)
}

#[allow(clippy::too_many_arguments)]
fn accumulate_usage(
    by_day: &mut BTreeMap<String, ClineUsageDay>,
    date: String,
    prompt_tokens: f64,
    completion_tokens: f64,
    tokens: f64,
    credits: f64,
    cost_usd: f64,
) {
    if tokens <= 0.0 && credits <= 0.0 && cost_usd <= 0.0 {
        return;
    }
    let day = by_day.entry(date.clone()).or_insert_with(|| ClineUsageDay {
        date,
        tokens: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
        credits: 0.0,
        cost_usd: 0.0,
    });
    day.prompt_tokens = day
        .prompt_tokens
        .saturating_add(prompt_tokens.max(0.0).round() as u64);
    day.completion_tokens = day
        .completion_tokens
        .saturating_add(completion_tokens.max(0.0).round() as u64);
    day.tokens = day.tokens.saturating_add(tokens.round() as u64);
    day.credits += credits;
    day.cost_usd += cost_usd;
}

fn finish_usage_days(by_day: BTreeMap<String, ClineUsageDay>) -> Vec<ClineUsageDay> {
    let mut days: Vec<ClineUsageDay> = by_day.into_values().collect();
    if days.len() > USAGE_CHART_DAYS {
        days.drain(..days.len() - USAGE_CHART_DAYS);
    }
    days
}

fn normalize_date(value: &Value) -> Option<String> {
    let text = value.as_str()?.trim();
    let bytes = text.as_bytes();
    if bytes.len() >= 10 && bytes[4] == b'-' && bytes[7] == b'-' {
        return Some(text[..10].to_string());
    }
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.with_timezone(&Utc).format("%Y-%m-%d").to_string())
}

fn text(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_and_orders_known_limits_only() {
        let windows = parse_limits(&json!({"success": true, "data": {"limits": [
            {"type": "monthly", "percentUsed": "12.5", "resetsAt": "2030-01-31T00:00:00Z"},
            {"type": "unknown", "percentUsed": 50},
            {"type": "five_hour", "percentUsed": 180},
            {"type": "weekly", "percentUsed": -3},
            {"type": "weekly"}
        ]}}))
        .unwrap();
        assert_eq!(
            windows
                .iter()
                .map(|window| window.key.as_str())
                .collect::<Vec<_>>(),
            vec!["five_hour", "weekly", "monthly"]
        );
        assert_eq!(windows[0].used, 100.0);
        assert_eq!(windows[1].used, 0.0);
        assert_eq!(windows[2].used, 12.5);
        assert_eq!(windows[0].total, 100.0);
        assert_eq!(windows[0].unit, "%");
        assert!(windows[2].reset_at.is_some());
        assert!(windows[0].reset_at.is_none());
        assert!(parse_limits(&json!({"data": {}})).is_err());
    }

    #[test]
    fn parses_account_and_plan_details() {
        let account = parse_account(
            &json!({"data": {"id": "user-1", "email": "a@b.c", "displayName": "Cline User"}}),
        )
        .unwrap();
        assert_eq!(account.id.as_deref(), Some("user-1"));
        assert_eq!(account.email.as_deref(), Some("a@b.c"));
        assert_eq!(account.name.as_deref(), Some("Cline User"));
        let plan = parse_plan(&json!({"data": {
            "plan": {"displayName": "ClinePass"},
            "currentPeriodEnd": "2030-02-01T00:00:00.000Z",
            "cancelAt": "2030-01-15T00:00:00Z"
        }}))
        .unwrap();
        assert_eq!(plan.name.as_deref(), Some("ClinePass"));
        assert_eq!(
            plan.period_end.unwrap().timestamp_millis(),
            1_896_134_400_000
        );
        assert!(parse_plan(&json!({})).is_none());
    }

    fn usage_from_limits(quota_window: &str) -> Usage {
        build_usage(
            parse_limits(&json!({"data": {"limits": [
                {"type": "five_hour", "percentUsed": 20},
                {"type": "weekly", "percentUsed": 95},
                {"type": "monthly", "percentUsed": 40}
            ]}}))
            .unwrap(),
            None,
            None,
            UsageChart::default(),
            quota_window,
        )
    }

    #[test]
    fn summary_defaults_to_five_hour_regardless_of_usage() {
        let usage = usage_from_limits("five_hour");
        assert_eq!(usage.used, 20.0);
        assert_eq!(usage.label.as_deref(), Some("5 小时"));
        assert_eq!(usage.windows.len(), 3);
        assert_eq!(usage.cline_account.unwrap().plan_name, None);
    }

    #[test]
    fn summary_honors_configured_window_and_falls_back_when_missing() {
        assert_eq!(usage_from_limits("weekly").label.as_deref(), Some("每周"));
        assert_eq!(usage_from_limits("auto").label.as_deref(), Some("每周"));
        assert_eq!(usage_from_limits("monthly").used, 40.0);
        // A window the account does not expose falls back to the tightest one.
        let usage = build_usage(
            parse_limits(&json!({"data": {"limits": [{"type": "monthly", "percentUsed": 12}]}}))
                .unwrap(),
            None,
            None,
            UsageChart::default(),
            "five_hour",
        );
        assert_eq!(usage.label.as_deref(), Some("每月"));
        assert_eq!(usage.used, 12.0);
    }

    #[test]
    fn aggregates_usage_transactions_by_utc_day() {
        let days = parse_usage_days(&json!({"success": true, "data": {"items": [
            {"createdAt": "2030-01-02T10:00:00Z", "promptTokens": 100, "completionTokens": "50", "totalTokens": 150, "creditsUsed": 1.5, "costUsd": "0.03"},
            {"createdAt": "2030-01-01T23:30:00Z", "promptTokens": 10, "completionTokens": 5, "totalTokens": 15, "creditsUsed": 0.1, "costUsd": 0.01},
            {"createdAt": "2030-01-02T23:59:59Z", "promptTokens": 20, "completionTokens": 30, "creditsUsed": "0.5"},
            {"createdAt": "2030-01-03T00:00:00Z", "totalTokens": 0, "creditsUsed": 0, "costUsd": 0},
            {"createdAt": "not-a-date", "totalTokens": 999}
        ]}}));
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].date, "2030-01-01");
        assert_eq!(days[0].tokens, 15);
        assert_eq!(days[1].date, "2030-01-02");
        assert_eq!(days[1].prompt_tokens, 120);
        assert_eq!(days[1].completion_tokens, 80);
        assert_eq!(days[1].tokens, 200);
        assert!((days[1].credits - 2.0).abs() < f64::EPSILON);
        assert!((days[1].cost_usd - 0.03).abs() < f64::EPSILON);
    }

    #[test]
    fn usage_days_keep_only_the_most_recent_window() {
        // 2030-01 只有 31 天，32 之后的日期会被解析器丢弃，正好用于验证截断逻辑。
        let items: Vec<Value> = (1..=35)
            .map(|day| {
                json!({
                    "createdAt": format!("2030-01-{day:02}T12:00:00Z"),
                    "totalTokens": day,
                })
            })
            .collect();
        let days = parse_usage_days(&json!({"data": {"items": items}}));
        assert_eq!(days.len(), USAGE_CHART_DAYS);
        assert_eq!(days[0].date, "2030-01-02");
        assert_eq!(days[days.len() - 1].date, "2030-01-31");
    }

    #[test]
    fn usage_days_handle_missing_payload() {
        assert!(parse_usage_days(&json!({})).is_empty());
        assert!(parse_usage_days(&json!({"data": {}})).is_empty());
        assert!(parse_daily_usage_days(&json!({})).is_empty());
        assert!(parse_daily_usage_days(&json!({"data": {}})).is_empty());
    }

    #[test]
    fn aggregates_daily_usage_items_by_date() {
        let days = parse_daily_usage_days(&json!({"success": true, "data": {"items": [
            {"date": "2030-01-02", "aiModelName": "model-a", "promptTokens": 100, "completionTokens": "50", "costUsd": "0.03"},
            {"date": "2030-01-02", "aiModelName": "model-b", "promptTokens": 10, "completionTokens": 5, "costUsd": 0.01},
            {"date": "2030-01-01T12:00:00Z", "promptTokens": 7, "completionTokens": 3, "costUsd": 0},
            {"date": "2030-01-03", "promptTokens": 0, "completionTokens": 0, "costUsd": 0},
            {"date": "not-a-date", "promptTokens": 999}
        ]}}));
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].date, "2030-01-01");
        assert_eq!(days[0].tokens, 10);
        assert_eq!(days[1].date, "2030-01-02");
        assert_eq!(days[1].prompt_tokens, 110);
        assert_eq!(days[1].completion_tokens, 55);
        assert_eq!(days[1].tokens, 165);
        assert!((days[1].cost_usd - 0.04).abs() < f64::EPSILON);
    }

    #[test]
    fn encodes_user_id_in_path_segment() {
        assert_eq!(encode_path_segment("user-1"), "user-1");
        assert_eq!(encode_path_segment("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn daily_range_spans_requested_days() {
        let (start, end) = usage_date_range(30);
        let start = chrono::NaiveDate::parse_from_str(&start, "%Y-%m-%d").unwrap();
        let end = chrono::NaiveDate::parse_from_str(&end, "%Y-%m-%d").unwrap();
        assert_eq!((end - start).num_days(), 29);
    }
}
