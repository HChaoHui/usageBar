//! MiniMax account Provider using usageBar's independent OAuth session.

use super::{
    McodeAccountDetails, McodeSignin, McodeSigninDay, Provider, ProviderError, Usage, UsageWindow,
};
use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use reqwest::{Client, Response, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const USER_INFO_PATH: &str = "/v1/api/user/info";
const WORKSPACE_PATH: &str = "/matrix/api/v1/user/get_user_extra_info";
const MEMBERSHIP_PATH: &str = "/matrix/api/v1/commerce/get_membership_info";
const QUOTA_PATH: &str = "/v1/api/openplatform/coding_plan/remains";
const SIGNIN_STATUS_PATH: &str = "/minimax-cloud/api/v1/signin/status";
const SIGNIN_CLAIM_PATH: &str = "/minimax-cloud/api/v1/signin/claim";
const AUTH_EXPIRED_CODE: i64 = 1_000_048;
const REQUEST_TIMEOUT_SECS: u64 = 10;
const SIGNATURE_SECRET: &str = "I*7Cf%WZ#S&%1RlZJ&C2";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McodeProvider {
    pub id: String,
    pub display_name: String,
    pub icon: String,
    pub color: String,
    pub unit: String,
}

#[derive(Clone, Copy)]
enum AccountRegion {
    Cn,
    En,
}

impl AccountRegion {
    fn language(self) -> &'static str {
        match self {
            Self::Cn => "zh",
            Self::En => "en",
        }
    }

    fn account_origin(self) -> &'static str {
        match self {
            Self::Cn => "https://agent.minimaxi.com",
            Self::En => "https://agent.minimax.io",
        }
    }

    fn quota_origin(self) -> &'static str {
        match self {
            Self::Cn => "https://www.minimaxi.com",
            Self::En => "https://platform.minimax.io",
        }
    }
}

#[derive(Clone)]
struct ManagedAuth {
    access_token: String,
    region: AccountRegion,
}

#[derive(Default, Clone)]
struct Membership {
    has_token_plan: Option<bool>,
    op_group_id: Option<String>,
    plan_type: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    credit_balance: Option<f64>,
}

struct PersonalWorkspace {
    id: Value,
    membership: Membership,
}

#[async_trait]
impl Provider for McodeProvider {
    async fn fetch(&self) -> Result<Usage, ProviderError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ProviderError::Other("无法创建 MCode 请求客户端".into()))?;
        // Serialize refresh rotation and requests for this card's account only.
        let mut session = crate::mcode_auth::session(&self.id)
            .await
            .map_err(ProviderError::Auth)?;
        let (access_token, region) = session.token(false).await.map_err(ProviderError::Auth)?;
        let mut auth = ManagedAuth {
            access_token,
            region: if region.is_global() {
                AccountRegion::En
            } else {
                AccountRegion::Cn
            },
        };
        match fetch_usage(&client, &auth).await {
            Err(ProviderError::Auth(_)) => {
                auth.access_token = session.token(true).await.map_err(ProviderError::Auth)?.0;
                fetch_usage(&client, &auth).await
            }
            result => result,
        }
    }
}

async fn fetch_usage(client: &Client, auth: &ManagedAuth) -> Result<Usage, ProviderError> {
    let (real_user_id, account_name) = fetch_identity(client, auth).await?;

    // Sign-in is available independently of Token Plan subscription/quota.
    let (account, signin) = tokio::join!(
        fetch_account_usage(client, auth, &real_user_id),
        fetch_signin(client, auth, &real_user_id),
    );
    // A failed optional sign-in query must not discard valid account/quota data.
    let signin = signin.ok();
    let mut usage = account?;
    if let Some(details) = &mut usage.mcode_account {
        details.account_id = Some(real_user_id);
        details.account_name = account_name;
        details.signin = signin;
    }
    Ok(usage)
}

async fn fetch_account_usage(
    client: &Client,
    auth: &ManagedAuth,
    real_user_id: &str,
) -> Result<Usage, ProviderError> {
    let workspace_response =
        signed_post(client, auth, real_user_id, WORKSPACE_PATH, json!({})).await;
    let workspace = match workspace_response {
        Ok(response) => parse_personal_workspace(&response),
        Err(ProviderError::Auth(error)) => return Err(ProviderError::Auth(error)),
        Err(_) => {
            let response =
                signed_post(client, auth, real_user_id, MEMBERSHIP_PATH, json!({})).await?;
            let membership = parse_membership(&response);
            return Ok(build_usage(membership, vec![], "unavailable"));
        }
    };

    let Some(workspace) = workspace else {
        return Ok(build_usage(Membership::default(), vec![], "unavailable"));
    };

    let personal_op_group_id = workspace.membership.op_group_id.clone();
    let membership_body = json!({ "workspace_id": workspace.id });
    let membership =
        match signed_post(client, auth, real_user_id, MEMBERSHIP_PATH, membership_body).await {
            Ok(response) => merge_membership(workspace.membership, parse_membership(&response)),
            Err(ProviderError::Auth(error)) => return Err(ProviderError::Auth(error)),
            Err(_) => workspace.membership,
        };

    if membership.has_token_plan == Some(false) {
        return Ok(build_usage(membership, vec![], "not-subscribed"));
    }
    let Some(op_group_id) = personal_op_group_id else {
        return Ok(build_usage(membership, vec![], "unavailable"));
    };

    match fetch_quota(client, auth, &op_group_id).await {
        Ok(windows) => Ok(build_usage(membership, windows, "available")),
        Err(ProviderError::Auth(error)) => Err(ProviderError::Auth(error)),
        Err(_) => Ok(build_usage(membership, vec![], "unavailable")),
    }
}

fn build_usage(membership: Membership, windows: Vec<UsageWindow>, quota_state: &str) -> Usage {
    let selected = windows.iter().max_by(|left, right| {
        usage_ratio(left)
            .total_cmp(&usage_ratio(right))
            .then_with(|| right.key.cmp(&left.key))
    });
    let (used, total, unit, label, reset_at) = match selected {
        Some(window) => (
            window.used,
            window.total,
            window.unit.clone(),
            Some(window.label.clone()),
            window.reset_at,
        ),
        None => (0.0, 0.0, "%".into(), Some("账号额度".into()), None),
    };

    Usage {
        used,
        total,
        unit,
        label,
        reset_at,
        fetched_at: Some(Utc::now()),
        windows,
        balance: None,
        reset_credits: None,
        codex_account: None,
        mcode_account: Some(McodeAccountDetails {
            account_id: None,
            account_name: None,
            plan_type: membership.plan_type,
            subscription_active_until: membership.expires_at,
            credit_balance: membership.credit_balance,
            has_token_plan: membership.has_token_plan,
            quota_state: quota_state.into(),
            signin: None,
        }),
        cline_account: None,
        cline_usage: Vec::new(),
    }
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SigninOutcome {
    Claimed,
    AlreadyClaimed,
    NotClaimable,
}

#[derive(Debug, Serialize)]
pub struct SigninResult {
    pub result: SigninOutcome,
    pub points: Option<f64>,
}

fn parse_signin(panel: &Value) -> Result<McodeSignin, ProviderError> {
    #[derive(Deserialize)]
    struct Panel {
        days: Vec<McodeSigninDay>,
    }
    let mut panel: Panel = serde_json::from_value(panel.clone())
        .map_err(|_| ProviderError::Parse("MiniMax 签到状态响应无效".into()))?;
    panel.days.sort_by_key(|day| day.day_no);
    if panel.days.iter().any(|day| {
        day.day_no == 0
            || [day.points, day.bonus_points]
                .into_iter()
                .flatten()
                .any(|points| !points.is_finite() || points < 0.0)
    }) || panel
        .days
        .windows(2)
        .any(|pair| pair[0].day_no == pair[1].day_no)
    {
        return Err(ProviderError::Parse("MiniMax 签到每日奖励数据无效".into()));
    }
    Ok(McodeSignin {
        can_claim: panel.days.iter().any(|day| day.status == 2),
        claimed_today: panel.days.iter().any(|day| day.is_today && day.status == 3),
        days: panel.days,
    })
}

async fn fetch_signin(
    client: &Client,
    auth: &ManagedAuth,
    user_id: &str,
) -> Result<McodeSignin, ProviderError> {
    let response = signed_get(client, auth, Some(user_id), SIGNIN_STATUS_PATH).await?;
    parse_signin(&response["data"])
}

#[derive(Clone, Copy)]
enum SigninRequest {
    Status,
    Claim,
}

async fn claim_with_api<F, Fut>(mut request: F) -> Result<SigninResult, ProviderError>
where
    F: FnMut(SigninRequest) -> Fut,
    Fut: std::future::Future<Output = Result<Value, ProviderError>>,
{
    // Never trust the cached button state, including after an ambiguous timeout.
    let response = request(SigninRequest::Status).await?;
    let status = parse_signin(&response["data"])?;
    if !status.can_claim {
        return Ok(SigninResult {
            result: if status.claimed_today {
                SigninOutcome::AlreadyClaimed
            } else {
                SigninOutcome::NotClaimable
            },
            points: None,
        });
    }
    let response = request(SigninRequest::Claim).await?;
    let data = &response["data"];
    let result = data["claim_result"].as_i64();
    let points = data["points"]
        .as_f64()
        .filter(|points| points.is_finite() && *points >= 0.0);
    if !matches!(result, Some(1 | 2))
        || points.is_none()
        || !data["claim_id"].as_str().is_some_and(|id| !id.is_empty())
        || data["expire_at_ms"].as_i64().is_none()
    {
        return Err(ProviderError::Parse(
            "MiniMax 签到领取响应无效，请刷新签到状态".into(),
        ));
    }
    parse_signin(&data["panel"])?;
    Ok(SigninResult {
        result: if result == Some(1) {
            SigninOutcome::Claimed
        } else {
            SigninOutcome::AlreadyClaimed
        },
        points,
    })
}

async fn claim_for_auth(
    client: &Client,
    auth: &ManagedAuth,
) -> Result<SigninResult, ProviderError> {
    let user_id = fetch_real_user_id(client, auth).await?;
    claim_with_api(|operation| {
        let user_id = &user_id;
        async move {
            match operation {
                SigninRequest::Status => {
                    signed_get(client, auth, Some(user_id), SIGNIN_STATUS_PATH).await
                }
                SigninRequest::Claim => {
                    signed_post(client, auth, user_id, SIGNIN_CLAIM_PATH, json!({})).await
                }
            }
        }
    })
    .await
}

pub async fn claim_signin(id: &str) -> Result<SigninResult, ProviderError> {
    let client = Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ProviderError::Other("无法创建 MiniMax 签到客户端".into()))?;
    let mut session = crate::mcode_auth::session(id)
        .await
        .map_err(ProviderError::Auth)?;
    let (access_token, region) = session.token(false).await.map_err(ProviderError::Auth)?;
    let mut auth = ManagedAuth {
        access_token,
        region: if region.is_global() {
            AccountRegion::En
        } else {
            AccountRegion::Cn
        },
    };
    match claim_for_auth(&client, &auth).await {
        Err(ProviderError::Auth(_)) => {
            auth.access_token = session.token(true).await.map_err(ProviderError::Auth)?.0;
            claim_for_auth(&client, &auth).await
        }
        result => result,
    }
}

fn usage_ratio(window: &UsageWindow) -> f64 {
    if window.total > 0.0 {
        (window.used / window.total).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

async fn fetch_real_user_id(client: &Client, auth: &ManagedAuth) -> Result<String, ProviderError> {
    Ok(fetch_identity(client, auth).await?.0)
}

async fn fetch_identity(
    client: &Client,
    auth: &ManagedAuth,
) -> Result<(String, Option<String>), ProviderError> {
    let response = signed_get(client, auth, None, USER_INFO_PATH).await?;
    let data = response.get("data").unwrap_or(&response);
    let user_info = data
        .get("userInfo")
        .or_else(|| data.get("user_info"))
        .or_else(|| response.get("userInfo"))
        .or_else(|| response.get("user_info"))
        .ok_or_else(|| ProviderError::Parse("MCode 账号信息中缺少 userInfo".into()))?;
    let id = text_field(user_info, &["realUserID", "real_user_id"])
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| ProviderError::Parse("MCode 账号信息中缺少 realUserID".into()))?;
    let name = text_field(user_info, &["userName", "user_name", "name"])
        .filter(|name| !name.trim().is_empty());
    Ok((id, name))
}

async fn signed_get(
    client: &Client,
    auth: &ManagedAuth,
    real_user_id: Option<&str>,
    path: &str,
) -> Result<Value, ProviderError> {
    let now_ms = unix_time_ms()?;
    let url = build_account_url(auth, real_user_id, path, now_ms)?;
    let path_and_query = path_and_query(&url);
    let now_seconds = now_ms / 1000;
    let yy = yy_signature(&path_and_query, "{}", now_ms);
    let x_signature = md5_hex(format!("{now_seconds}{SIGNATURE_SECRET}"));
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", "MiniMaxCode")
        .bearer_auth(&auth.access_token)
        .header("yy", yy)
        .header("x-timestamp", now_seconds.to_string())
        .header("x-signature", x_signature)
        .send()
        .await
        .map_err(|error| sanitized_network_error("MCode 账号信息", &error))?;
    parse_response(response, "MCode 账号信息").await
}

async fn signed_post(
    client: &Client,
    auth: &ManagedAuth,
    real_user_id: &str,
    path: &str,
    body: Value,
) -> Result<Value, ProviderError> {
    let now_ms = unix_time_ms()?;
    let url = build_account_url(auth, Some(real_user_id), path, now_ms)?;
    let path_and_query = path_and_query(&url);
    let now_seconds = now_ms / 1000;
    let body = serde_json::to_string(&body)
        .map_err(|_| ProviderError::Other("无法编码 MCode 请求".into()))?;
    let yy = yy_signature(&path_and_query, &body, now_ms);
    let x_signature = md5_hex(format!("{now_seconds}{SIGNATURE_SECRET}{body}"));
    let response = client
        .post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", "MiniMaxCode")
        .bearer_auth(&auth.access_token)
        .header("yy", yy)
        .header("x-timestamp", now_seconds.to_string())
        .header("x-signature", x_signature)
        .body(body)
        .send()
        .await
        .map_err(|error| sanitized_network_error("MCode 会员信息", &error))?;
    parse_response(response, "MCode 会员信息").await
}

async fn fetch_quota(
    client: &Client,
    auth: &ManagedAuth,
    op_group_id: &str,
) -> Result<Vec<UsageWindow>, ProviderError> {
    let endpoint = format!("{}{}", auth.region.quota_origin(), QUOTA_PATH);
    let response = client
        .get(endpoint)
        .header("Accept", "application/json")
        .bearer_auth(&auth.access_token)
        .header("X-Group-Id", op_group_id)
        .send()
        .await
        .map_err(|error| sanitized_network_error("MCode 额度", &error))?;
    let json = parse_response(response, "MCode 额度").await?;
    parse_quota_windows(&json)
}

fn build_account_url(
    auth: &ManagedAuth,
    real_user_id: Option<&str>,
    path: &str,
    now_ms: i64,
) -> Result<Url, ProviderError> {
    let mut url = Url::parse(&format!("{}{}", auth.region.account_origin(), path))
        .map_err(|_| ProviderError::Other("MCode 账号接口地址无效".into()))?;
    let timezone_offset = Local::now().offset().local_minus_utc();
    let language = auth.region.language();
    let user_id = real_user_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or("0");
    let now_ms = now_ms.to_string();
    let timezone_offset = timezone_offset.to_string();
    let os_name = mcode_os_name();
    let cloud = path.starts_with("/minimax-cloud/");
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("device_platform", if cloud { "web" } else { "mcode" })
            .append_pair("biz_id", "3")
            .append_pair("app_id", "3001")
            .append_pair("version_code", "22201");
        if cloud {
            query
                .append_pair("is_desktop", "1")
                .append_pair("desktop_version", "0.4.12");
        }
        query
            .append_pair("unix", &now_ms)
            .append_pair("timezone_offset", &timezone_offset)
            .append_pair("sys_language", language)
            .append_pair("lang", language)
            .append_pair("device_id", "0")
            .append_pair("os_name", os_name)
            .append_pair("browser_name", "mcode")
            .append_pair("user_id", user_id)
            .append_pair("client", "mcode");
    }
    Ok(url)
}

fn mcode_os_name() -> &'static str {
    match env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        value => value,
    }
}

fn path_and_query(url: &Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().into(),
    }
}

fn yy_signature(path_and_query: &str, body: &str, now_ms: i64) -> String {
    let encoded = encode_uri_component(path_and_query);
    let timestamp_hash = md5_hex(now_ms.to_string());
    md5_hex(format!("{encoded}_{body}{timestamp_hash}ooui"))
}

fn md5_hex(value: impl AsRef<[u8]>) -> String {
    format!("{:x}", md5::compute(value))
}

fn encode_uri_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn unix_time_ms() -> Result<i64, ProviderError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .map_err(|_| ProviderError::Other("系统时间早于 Unix Epoch".into()))
}

fn sanitized_network_error(label: &str, error: &reqwest::Error) -> ProviderError {
    let detail = if error.is_timeout() {
        "请求超时"
    } else if error.is_connect() {
        "连接失败"
    } else {
        "请求失败"
    };
    ProviderError::Network(format!("{label}{detail}"))
}

async fn parse_response(response: Response, label: &str) -> Result<Value, ProviderError> {
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err(expired_auth_error());
    }
    if !status.is_success() {
        return Err(ProviderError::Other(format!("{label}返回 HTTP {status}")));
    }
    let json: Value = response
        .json()
        .await
        .map_err(|_| ProviderError::Parse(format!("{label}返回了无效 JSON")))?;
    check_api_status(&json, label)?;
    Ok(json)
}

fn check_api_status(json: &Value, label: &str) -> Result<(), ProviderError> {
    let status_info = json.get("statusInfo").or_else(|| json.get("status_info"));
    if let Some(code) = status_info
        .and_then(|status| status.get("code"))
        .and_then(number)
        .map(|value| value as i64)
    {
        if code == AUTH_EXPIRED_CODE {
            return Err(expired_auth_error());
        }
        if code != 0 {
            return Err(ProviderError::Other(format!("{label}返回状态码 {code}")));
        }
    }
    if let Some(code) = root_or_data_field(json, "base_resp")
        .and_then(|status| status.get("status_code"))
        .and_then(number)
        .map(|value| value as i64)
    {
        if code == AUTH_EXPIRED_CODE {
            return Err(expired_auth_error());
        }
        if code != 0 {
            return Err(ProviderError::Other(format!("{label}返回状态码 {code}")));
        }
    }
    Ok(())
}

fn expired_auth_error() -> ProviderError {
    ProviderError::Auth("MiniMax 授权已失效，请在设置中重新登录".into())
}

fn parse_personal_workspace(json: &Value) -> Option<PersonalWorkspace> {
    let workspaces = json
        .get("workspaces")
        .or_else(|| json.get("data").and_then(|data| data.get("workspaces")))
        .and_then(Value::as_array)?;
    workspaces.iter().find_map(|workspace| {
        let workspace_type = workspace.get("workspace_type").and_then(number)? as i64;
        if workspace_type != 0 {
            return None;
        }
        let id = workspace.get("workspace_id")?.clone();
        if !id.is_string() && !id.is_number() {
            return None;
        }
        Some(PersonalWorkspace {
            id,
            membership: parse_membership(workspace),
        })
    })
}

fn parse_membership(json: &Value) -> Membership {
    let credit_summary = root_or_data_field(json, "op_credit_summary");
    Membership {
        has_token_plan: root_or_data_field(json, "has_token_plan").and_then(boolean),
        op_group_id: root_or_data_field(json, "op_group_id").and_then(text_value),
        plan_type: root_or_data_field(json, "token_plan_tier").and_then(text_value),
        expires_at: root_or_data_field(json, "token_plan_expires_at")
            .and_then(number)
            .and_then(timestamp),
        credit_balance: credit_summary
            .and_then(|summary| summary.get("total_remaining_amount"))
            .and_then(number)
            .or_else(|| root_or_data_field(json, "opcredit_balance").and_then(number)),
    }
}

fn merge_membership(base: Membership, update: Membership) -> Membership {
    let has_token_plan = match (base.has_token_plan, update.has_token_plan) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), _) | (_, Some(false)) => Some(false),
        _ => None,
    };
    Membership {
        has_token_plan,
        op_group_id: base.op_group_id.or(update.op_group_id),
        plan_type: update.plan_type.or(base.plan_type),
        expires_at: update.expires_at.or(base.expires_at),
        credit_balance: update.credit_balance.or(base.credit_balance),
    }
}

fn parse_quota_windows(json: &Value) -> Result<Vec<UsageWindow>, ProviderError> {
    let entries = json
        .get("model_remains")
        .or_else(|| json.get("data").and_then(|data| data.get("model_remains")))
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Parse("MCode 额度响应缺少 model_remains".into()))?;
    let coding = entries
        .first()
        .ok_or_else(|| ProviderError::Parse("MCode 额度响应为空".into()))?;
    let mut windows = Vec::new();
    if let Some(window) = parse_percent_window(
        coding,
        "five_hour",
        "5 小时",
        "current_interval",
        "end_time",
    ) {
        windows.push(window);
    }
    if let Some(window) = parse_percent_window(
        coding,
        "weekly",
        "7 天",
        "current_weekly",
        "weekly_end_time",
    ) {
        windows.push(window);
    }
    if let Some(video) = entries.iter().find(|entry| {
        text_field(entry, &["model_name"])
            .map(|name| name.to_ascii_lowercase().contains("video"))
            .unwrap_or(false)
            && field(entry, &["current_interval_total_count"])
                .and_then(number)
                .unwrap_or(0.0)
                > 0.0
    }) {
        if let Some(window) = parse_video_window(video) {
            windows.push(window);
        }
    }
    if windows.is_empty() {
        return Err(ProviderError::Parse("MCode 未返回可显示的额度窗口".into()));
    }
    Ok(windows)
}

fn parse_percent_window(
    entry: &Value,
    key: &str,
    label: &str,
    prefix: &str,
    reset_key: &str,
) -> Option<UsageWindow> {
    let status = entry
        .get(format!("{prefix}_status"))
        .and_then(number)
        .map(|value| value as i64);
    let unlimited = status == Some(3);
    let remaining_percent = if unlimited {
        Some(100.0)
    } else {
        entry
            .get(format!("{prefix}_remaining_percent"))
            .and_then(number)
            .or_else(|| {
                let total = entry
                    .get(format!("{prefix}_total_count"))
                    .and_then(number)?;
                let remaining = entry
                    .get(format!("{prefix}_usage_count"))
                    .and_then(number)?;
                (total > 0.0).then_some(remaining / total * 100.0)
            })
            .or_else(|| (status == Some(2)).then_some(0.0))
    }?;
    let remaining_percent = remaining_percent.clamp(0.0, 100.0).round();
    Some(UsageWindow {
        key: key.into(),
        label: if unlimited {
            format!("{label} · 无限")
        } else {
            label.into()
        },
        used: if unlimited {
            0.0
        } else {
            100.0 - remaining_percent
        },
        total: 100.0,
        unit: "%".into(),
        reset_at: entry.get(reset_key).and_then(number).and_then(timestamp),
    })
}

fn parse_video_window(entry: &Value) -> Option<UsageWindow> {
    let status = entry
        .get("current_interval_status")
        .and_then(number)
        .map(|value| value as i64);
    let unlimited = status == Some(3);
    let total = entry.get("current_interval_total_count").and_then(number)?;
    if total <= 0.0 {
        return None;
    }
    let remaining = entry
        .get("current_interval_usage_count")
        .and_then(number)
        .unwrap_or(0.0)
        .clamp(0.0, total);
    Some(UsageWindow {
        key: "video".into(),
        label: if unlimited {
            "视频 · 无限"
        } else {
            "视频"
        }
        .into(),
        used: if unlimited { 0.0 } else { total - remaining },
        total,
        unit: "次".into(),
        reset_at: entry.get("end_time").and_then(number).and_then(timestamp),
    })
}

fn root_or_data_field<'a>(json: &'a Value, key: &str) -> Option<&'a Value> {
    json.get(key)
        .or_else(|| json.get("data").and_then(|data| data.get(key)))
}

fn field<'a>(json: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| json.get(key))
}

fn text_field(json: &Value, keys: &[&str]) -> Option<String> {
    field(json, keys).and_then(text_value)
}

fn text_value(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

fn boolean(value: &Value) -> Option<bool> {
    value
        .as_bool()
        .or_else(|| value.as_i64().map(|value| value != 0))
        .or_else(
            || match value.as_str()?.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            },
        )
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn timestamp(value: f64) -> Option<DateTime<Utc>> {
    let value = value as i64;
    if value.abs() >= 10_000_000_000 {
        DateTime::from_timestamp_millis(value)
    } else {
        DateTime::from_timestamp(value, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signin_panel(status: i64) -> Value {
        json!({ "days": [
            { "day_no": 1, "is_today": false, "status": 3 },
            { "day_no": 2, "is_today": true, "status": status },
            { "day_no": 3, "is_today": false, "status": 1 }
        ] })
    }

    #[test]
    fn signin_preserves_all_seven_days_rewards_and_server_states() {
        let mut days = (1..=7)
            .map(|day| {
                json!({
                    "day_no": day, "points": day * 10,
                    "bonus_points": if day == 7 { 100 } else { 0 },
                    "status": if day < 3 { 3 } else if day == 3 { 2 } else { 1 },
                    "is_today": day == 3,
                    "private_field": "must-not-be-exposed"
                })
            })
            .collect::<Vec<_>>();
        days.reverse();
        let signin = parse_signin(&json!({"days": days})).unwrap();
        assert_eq!(signin.days.len(), 7);
        assert_eq!(
            signin.days.iter().map(|day| day.day_no).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(signin.days[6].points, Some(70.0));
        assert_eq!(signin.days[6].bonus_points, Some(100.0));
        assert!(signin.days[2].is_today);
        assert_eq!(signin.days[0].status, 3);
        assert!(signin.can_claim && !signin.claimed_today);
        let serialized = serde_json::to_string(&signin).unwrap();
        assert!(!serialized.contains("private_field"));
        assert!(!serialized.contains("must-not-be-exposed"));
    }

    #[test]
    fn signin_does_not_invent_missing_rewards_or_duplicate_days() {
        let signin = parse_signin(&signin_panel(2)).unwrap();
        assert_eq!(signin.days[0].points, None);
        assert_eq!(signin.days[0].bonus_points, None);
        assert_eq!(signin.days.len(), 3);
        for points in [json!(-1), json!("invalid")] {
            assert!(parse_signin(
                &json!({"days":[{"day_no":1,"status":2,"is_today":true,"points":points}]})
            )
            .is_err());
        }
        assert!(parse_signin(&json!({"days":[
            {"day_no":1,"status":2,"is_today":true},
            {"day_no":1,"status":1,"is_today":false}
        ]}))
        .is_err());
        let unknown =
            parse_signin(&json!({"days":[{"day_no":1,"status":99,"is_today":true}]})).unwrap();
        assert!(!unknown.can_claim && !unknown.claimed_today);
        assert_eq!(unknown.days[0].status, 99);
    }

    #[test]
    fn signin_status_uses_server_eligibility_not_local_date() {
        let claimable = parse_signin(&signin_panel(2)).unwrap();
        assert!(claimable.can_claim);
        assert!(!claimable.claimed_today);
        let claimed = parse_signin(&signin_panel(3)).unwrap();
        assert!(!claimed.can_claim);
        assert!(claimed.claimed_today);
        let locked = parse_signin(&signin_panel(1)).unwrap();
        assert!(!locked.can_claim && !locked.claimed_today);
        assert!(parse_signin(&json!({})).is_err());
        assert!(parse_signin(&json!({"days":[{"status":2}]})).is_err());
        assert!(!parse_signin(&json!({"days":[]})).unwrap().can_claim);
    }

    #[tokio::test]
    async fn signin_rechecks_eligibility_and_returns_actual_award() {
        let mut requests = Vec::new();
        let result = claim_with_api(|operation| {
            requests.push(operation);
            std::future::ready(Ok(match operation {
                SigninRequest::Status => json!({ "data": signin_panel(2) }),
                SigninRequest::Claim => json!({ "data": {
                    "claim_id": "claim-1", "claim_result": 1, "points": 15,
                    "expire_at_ms": 1_800_000_000_000_i64, "panel": signin_panel(3)
                } }),
            }))
        })
        .await
        .unwrap();
        assert_eq!(result.result, SigninOutcome::Claimed);
        assert_eq!(result.points, Some(15.0));
        assert!(matches!(
            requests.as_slice(),
            [SigninRequest::Status, SigninRequest::Claim]
        ));
    }

    #[tokio::test]
    async fn signin_does_not_post_if_already_claimed_or_not_eligible() {
        for (status, expected) in [
            (3, SigninOutcome::AlreadyClaimed),
            (1, SigninOutcome::NotClaimable),
        ] {
            let result = claim_with_api(|operation| {
                assert!(matches!(operation, SigninRequest::Status));
                std::future::ready(Ok(json!({"data": signin_panel(status)})))
            })
            .await
            .unwrap();
            assert_eq!(result.result, expected);
            assert_eq!(result.points, None);
        }
        let error = claim_with_api(|operation| {
            assert!(matches!(operation, SigninRequest::Status));
            std::future::ready(Ok(json!({"data": {}})))
        })
        .await
        .unwrap_err();
        assert!(matches!(error, ProviderError::Parse(_)));
    }

    #[tokio::test]
    async fn signin_timeout_is_not_replayed_and_next_attempt_rechecks_status() {
        let mut claimed = false;
        let mut posts = 0;
        for attempt in 0..2 {
            let result = claim_with_api(|operation| {
                std::future::ready(match operation {
                    SigninRequest::Status => {
                        Ok(json!({"data": signin_panel(if claimed { 3 } else { 2 })}))
                    }
                    SigninRequest::Claim => {
                        claimed = true;
                        posts += 1;
                        Err(ProviderError::Network("请求超时".into()))
                    }
                })
            })
            .await;
            if attempt == 0 {
                assert!(result.is_err());
            } else {
                assert_eq!(result.unwrap().result, SigninOutcome::AlreadyClaimed);
            }
        }
        assert_eq!(posts, 1);
    }

    #[tokio::test]
    async fn signin_handles_server_deduplication_and_rejects_invalid_award() {
        for points in [json!(10), json!(-1)] {
            let result = claim_with_api(|operation| {
                std::future::ready(Ok(match operation {
                    SigninRequest::Status => json!({"data": signin_panel(2)}),
                    SigninRequest::Claim => json!({"data": {
                        "claim_id": "existing-claim", "claim_result": 2, "points": points,
                        "expire_at_ms": 1_800_000_000_000_i64, "panel": signin_panel(3)
                    }}),
                }))
            })
            .await;
            if points == json!(10) {
                assert_eq!(result.unwrap().result, SigninOutcome::AlreadyClaimed);
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[test]
    fn signin_requests_use_dashboard_cloud_parameters() {
        let auth = ManagedAuth {
            access_token: "secret".into(),
            region: AccountRegion::En,
        };
        for path in [SIGNIN_STATUS_PATH, SIGNIN_CLAIM_PATH] {
            let url = build_account_url(&auth, Some("account-1"), path, 1_700_000_000_123).unwrap();
            assert_eq!(url.host_str(), Some("agent.minimax.io"));
            let query = url
                .query_pairs()
                .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(query["device_platform"], "web");
            assert_eq!(query["is_desktop"], "1");
            assert_eq!(query["desktop_version"], "0.4.12");
            assert_eq!(query["user_id"], "account-1");
            assert_eq!(query["lang"], "en");
            assert!(!query.contains_key("token"));
        }
    }

    #[test]
    fn oauth_account_url_contains_no_credential() {
        let auth = ManagedAuth {
            access_token: "private-oauth-token".into(),
            region: AccountRegion::Cn,
        };
        let url = build_account_url(&auth, Some("123"), USER_INFO_PATH, 1_700_000_000_123).unwrap();
        assert!(!url.as_str().contains("private-oauth-token"));
        assert!(!url.query_pairs().any(|(key, _)| key == "token"));
        assert_eq!(url.host_str(), Some("agent.minimaxi.com"));
    }

    #[test]
    fn parses_personal_workspace_account_details() {
        let workspace = parse_personal_workspace(&json!({
            "data": {
                "workspaces": [{
                    "workspace_type": 0,
                    "workspace_id": 123,
                    "has_token_plan": true,
                    "op_group_id": "group-1",
                    "token_plan_tier": "Plus Plan",
                    "token_plan_expires_at": 1_812_412_800_000_i64,
                    "op_credit_summary": { "total_remaining_amount": "11308" }
                }]
            }
        }))
        .unwrap();

        assert_eq!(workspace.id, json!(123));
        assert_eq!(workspace.membership.has_token_plan, Some(true));
        assert_eq!(workspace.membership.op_group_id.as_deref(), Some("group-1"));
        assert_eq!(workspace.membership.plan_type.as_deref(), Some("Plus Plan"));
        assert_eq!(workspace.membership.credit_balance, Some(11_308.0));
    }

    #[test]
    fn parses_five_hour_weekly_and_video_quota() {
        let windows = parse_quota_windows(&json!({
            "base_resp": { "status_code": 0 },
            "model_remains": [
                {
                    "model_name": "general",
                    "current_interval_remaining_percent": 92,
                    "end_time": 1_800_000_000_000_i64,
                    "current_weekly_status": 3,
                    "weekly_end_time": 1_900_000_000_000_i64
                },
                {
                    "model_name": "video-01",
                    "current_interval_usage_count": 3,
                    "current_interval_total_count": 5,
                    "end_time": 1_800_000_000_000_i64
                }
            ]
        }))
        .unwrap();

        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].key, "five_hour");
        assert_eq!(windows[0].used, 8.0);
        assert_eq!(windows[1].label, "7 天 · 无限");
        assert_eq!(windows[2].unit, "次");
        assert_eq!(windows[2].used, 2.0);
    }

    #[test]
    fn fallback_count_is_treated_as_remaining() {
        let window = parse_percent_window(
            &json!({
                "current_interval_total_count": 1000,
                "current_interval_usage_count": 250
            }),
            "five_hour",
            "5 小时",
            "current_interval",
            "end_time",
        )
        .unwrap();

        assert_eq!(window.used, 75.0);
    }

    #[test]
    fn active_membership_wins_over_stale_inactive_value() {
        let merged = merge_membership(
            Membership {
                has_token_plan: Some(false),
                ..Membership::default()
            },
            Membership {
                has_token_plan: Some(true),
                ..Membership::default()
            },
        );

        assert_eq!(merged.has_token_plan, Some(true));
    }

    #[test]
    fn usage_summary_selects_the_tightest_window() {
        let usage = build_usage(
            Membership::default(),
            vec![
                UsageWindow {
                    key: "five_hour".into(),
                    label: "5 小时".into(),
                    used: 8.0,
                    total: 100.0,
                    unit: "%".into(),
                    reset_at: None,
                },
                UsageWindow {
                    key: "weekly".into(),
                    label: "7 天".into(),
                    used: 100.0,
                    total: 100.0,
                    unit: "%".into(),
                    reset_at: None,
                },
            ],
            "available",
        );

        assert_eq!(usage.label.as_deref(), Some("7 天"));
        assert_eq!(usage.used, 100.0);
    }

    #[test]
    fn provider_serialization_contains_no_credentials() {
        let provider = McodeProvider {
            id: "mcode".into(),
            display_name: "MCode".into(),
            icon: "M".into(),
            color: "#5e5ce6".into(),
            unit: "%".into(),
        };
        let serialized = serde_json::to_string(&provider).unwrap();
        assert!(!serialized.contains("accessToken"));
        assert!(!serialized.contains("op_group_id"));
    }

    #[test]
    fn uri_component_encoding_matches_javascript_safe_set() {
        assert_eq!(
            encode_uri_component("/a?x=1&y=hello world"),
            "%2Fa%3Fx%3D1%26y%3Dhello%20world"
        );
        assert_eq!(encode_uri_component("/a?x=1"), "%2Fa%3Fx%3D1");
    }

    #[test]
    fn signature_matches_mcode_vector() {
        let path = "/matrix/api/v1/user/get_user_extra_info?device_platform=mcode&biz_id=3&token=test.token";
        let now_ms = 1_700_000_000_123_i64;
        assert_eq!(
            yy_signature(path, "{}", now_ms),
            "c8f6f181508f7ee6529aa8612671890a"
        );
        assert_eq!(
            md5_hex(format!("{}{}{}", now_ms / 1000, SIGNATURE_SECRET, "{}")),
            "b0c74b46e28053fb22e53239b1061505"
        );
    }
}
