//! CLIProxyAPI direct Codex quota Provider.
//!
//! Calls CPA's management `api-call` endpoint. CPA replaces `$TOKEN$` with the
//! OAuth token belonging to `auth_index`, then proxies the Codex usage request.

use super::{
    CodexAccountDetails, Provider, ProviderError, ResetCreditDetails, ResetCreditEntry, Usage,
    UsageWindow,
};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const API_CALL_PATH: &str = "/v0/management/api-call";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_RESET_CREDITS_URL: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const CODEX_RESET_CREDITS_CONSUME_URL: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";
const FIVE_HOURS_SECONDS: i64 = 5 * 60 * 60;
const WEEK_SECONDS: i64 = 7 * 24 * 60 * 60;
const MIN_MONTH_SECONDS: i64 = 28 * 24 * 60 * 60;
const MAX_MONTH_SECONDS: i64 = 31 * 24 * 60 * 60;
const MAX_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpaDirectProvider {
    pub id: String,
    pub display_name: String,
    pub icon: String,
    pub color: String,
    pub unit: String,
    /// CLIProxyAPI root URL, for example `http://127.0.0.1:8317`.
    pub base_url: String,
    /// CLIProxyAPI management key, not an API key from `api-keys`.
    pub management_key: String,
    pub auth_index: String,
    /// Optional ChatGPT account ID. Current Codex usage API also accepts no ID.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub account_id: Option<String>,
    /// `auto`, `five_hour`, `weekly`, or `monthly`.
    pub quota_window: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CpaCodexAccount {
    pub auth_index: String,
    pub display_name: String,
    pub account_id: Option<String>,
    pub plan_type: Option<String>,
    pub disabled: bool,
}

#[derive(Debug, Clone)]
struct QuotaCandidate {
    key: String,
    label: String,
    used_percent: f64,
    window_seconds: i64,
    reset_at: Option<DateTime<Utc>>,
}

#[async_trait]
impl Provider for CpaDirectProvider {
    async fn fetch(&self) -> Result<Usage, ProviderError> {
        validate_cpa_config(&self.base_url, &self.management_key, Some(&self.auth_index))?;
        let api_url = api_call_url(&self.base_url);
        let client = build_client()?;
        let upstream_headers = build_codex_headers(self.account_id.as_deref());
        let reset_headers = build_reset_credit_headers(&upstream_headers);
        let usage_request = proxy_get_json(
            &client,
            &api_url,
            self.management_key.trim(),
            self.auth_index.trim(),
            CODEX_USAGE_URL,
            &upstream_headers,
        );
        let reset_request = proxy_get_json(
            &client,
            &api_url,
            self.management_key.trim(),
            self.auth_index.trim(),
            CODEX_RESET_CREDITS_URL,
            &reset_headers,
        );
        let metadata_request = fetch_codex_account(
            &client,
            &self.base_url,
            self.management_key.trim(),
            self.auth_index.trim(),
        );
        let (usage_result, reset_result, metadata_result) =
            tokio::join!(usage_request, reset_request, metadata_request);
        let usage_json = usage_result?;
        let mut usage = parse_usage(&usage_json, &self.quota_window)?;
        if let Ok(reset_json) = reset_result {
            if let Ok(reset_credits) = parse_reset_credits(&reset_json) {
                usage.reset_credits = Some(reset_credits);
            }
        }
        if let Ok(Some(metadata)) = metadata_result {
            let details = usage.codex_account.get_or_insert(CodexAccountDetails {
                plan_type: None,
                subscription_active_until: None,
            });
            if details.plan_type.is_none() {
                details.plan_type = metadata.plan_type;
            }
        }

        Ok(usage)
    }
}

impl CpaDirectProvider {
    pub async fn consume_reset_credit(&self) -> Result<(), ProviderError> {
        validate_cpa_config(&self.base_url, &self.management_key, Some(&self.auth_index))?;
        let api_url = api_call_url(&self.base_url);
        let client = build_client()?;
        let headers = build_codex_headers(self.account_id.as_deref());
        let reset_headers = build_reset_credit_headers(&headers);
        let reset_json = proxy_get_json(
            &client,
            &api_url,
            self.management_key.trim(),
            self.auth_index.trim(),
            CODEX_RESET_CREDITS_URL,
            &reset_headers,
        )
        .await?;
        let reset_credits = parse_reset_credits(&reset_json)?;
        if reset_credits.available_count == 0 {
            return Err(ProviderError::NotFound(
                "no Codex reset credits are currently available".into(),
            ));
        }

        let data = serde_json::json!({
            "redeem_request_id": uuid::Uuid::new_v4().to_string(),
        })
        .to_string();
        proxy_call_body(
            &client,
            &api_url,
            self.management_key.trim(),
            self.auth_index.trim(),
            "POST",
            CODEX_RESET_CREDITS_CONSUME_URL,
            &headers,
            Some(&data),
        )
        .await?;
        Ok(())
    }
}

pub async fn discover_codex_accounts(
    base_url: &str,
    management_key: &str,
) -> Result<Vec<CpaCodexAccount>, ProviderError> {
    validate_cpa_config(base_url, management_key, None)?;
    let client = build_client()?;
    fetch_codex_accounts(&client, base_url, management_key).await
}

async fn proxy_get_json(
    client: &reqwest::Client,
    api_url: &str,
    management_key: &str,
    auth_index: &str,
    upstream_url: &str,
    upstream_headers: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, ProviderError> {
    let body = proxy_call_body(
        client,
        api_url,
        management_key,
        auth_index,
        "GET",
        upstream_url,
        upstream_headers,
        None,
    )
    .await?;
    serde_json::from_str(&body)
        .map_err(|error| ProviderError::Parse(format!("invalid Codex response body: {error}")))
}

#[allow(clippy::too_many_arguments)]
async fn proxy_call_body(
    client: &reqwest::Client,
    api_url: &str,
    management_key: &str,
    auth_index: &str,
    method: &str,
    upstream_url: &str,
    upstream_headers: &serde_json::Map<String, serde_json::Value>,
    data: Option<&str>,
) -> Result<String, ProviderError> {
    let mut request_body = serde_json::json!({
        "authIndex": auth_index,
        "method": method,
        "url": upstream_url,
        "header": upstream_headers,
    });
    if let Some(data) = data {
        request_body["data"] = serde_json::Value::String(data.into());
    }
    let mut last_transient = String::new();

    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(retry_delay(attempt)).await;
        }
        let resp = match client
            .post(api_url)
            .header("X-Management-Key", management_key)
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(error) => {
                last_transient = format!("CLIProxyAPI request failed: {error}");
                continue;
            }
        };

        let status = resp.status();
        let response_text = resp
            .text()
            .await
            .map_err(|error| ProviderError::Parse(error.to_string()))?;
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::Auth(format!(
                "CLIProxyAPI HTTP {status}: {}",
                response_error(&response_text)
            )));
        }
        if !status.is_success() {
            let message = format!(
                "CLIProxyAPI HTTP {status}: {}",
                response_error(&response_text)
            );
            if transient_status(status.as_u16() as u64) {
                last_transient = message;
                continue;
            }
            return Err(ProviderError::Other(message));
        }

        let envelope: serde_json::Value = serde_json::from_str(&response_text)
            .map_err(|error| ProviderError::Parse(format!("invalid CPA response: {error}")))?;
        let upstream_status = envelope
            .get("status_code")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| ProviderError::Parse("missing status_code in CPA response".into()))?;
        let body = envelope
            .get("body")
            .and_then(|value| value.as_str())
            .ok_or_else(|| ProviderError::Parse("missing body string in CPA response".into()))?;

        if matches!(upstream_status, 401 | 403) {
            return Err(ProviderError::Auth(format!(
                "Codex upstream HTTP {upstream_status}: {}",
                response_error(body)
            )));
        }
        if !(200..300).contains(&upstream_status) {
            let message = format!(
                "Codex upstream HTTP {upstream_status}: {}",
                response_error(body)
            );
            if transient_status(upstream_status) {
                last_transient = message;
                continue;
            }
            return Err(ProviderError::Other(message));
        }

        return Ok(body.to_string());
    }

    Err(ProviderError::Transient(format!(
        "{last_transient} (retried {MAX_ATTEMPTS} times)"
    )))
}

fn validate_cpa_config(
    base_url: &str,
    management_key: &str,
    auth_index: Option<&str>,
) -> Result<(), ProviderError> {
    if base_url.trim().is_empty() {
        return Err(ProviderError::Other(
            "CLIProxyAPI address is required".into(),
        ));
    }
    if management_key.trim().is_empty() {
        return Err(ProviderError::Auth(
            "CLIProxyAPI management key is required".into(),
        ));
    }
    if auth_index.is_some_and(|value| value.trim().is_empty()) {
        return Err(ProviderError::Other(
            "CLIProxyAPI auth_index is required".into(),
        ));
    }
    Ok(())
}

fn management_root(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    base.strip_suffix(API_CALL_PATH)
        .or_else(|| base.strip_suffix("/v0/management"))
        .unwrap_or(base)
        .to_string()
}

fn api_call_url(base_url: &str) -> String {
    let root = management_root(base_url);
    format!("{root}{API_CALL_PATH}")
}

fn auth_files_url(base_url: &str) -> String {
    format!("{}/v0/management/auth-files", management_root(base_url))
}

fn build_client() -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| ProviderError::Other(error.to_string()))
}

fn build_codex_headers(account_id: Option<&str>) -> serde_json::Map<String, serde_json::Value> {
    let mut headers = serde_json::Map::new();
    headers.insert(
        "Authorization".into(),
        serde_json::Value::String("Bearer $TOKEN$".into()),
    );
    headers.insert(
        "Content-Type".into(),
        serde_json::Value::String("application/json".into()),
    );
    headers.insert(
        "User-Agent".into(),
        serde_json::Value::String(
            "codex_cli_rs/0.76.0 (Debian 13.0.0; x86_64) WindowsTerminal".into(),
        ),
    );
    if let Some(account_id) = account_id.map(str::trim).filter(|value| !value.is_empty()) {
        headers.insert(
            "Chatgpt-Account-Id".into(),
            serde_json::Value::String(account_id.into()),
        );
    }
    headers
}

fn build_reset_credit_headers(
    headers: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut headers = headers.clone();
    headers.insert(
        "Accept".into(),
        serde_json::Value::String("application/json".into()),
    );
    headers.insert(
        "OpenAI-Beta".into(),
        serde_json::Value::String("codex-1".into()),
    );
    headers.insert(
        "Originator".into(),
        serde_json::Value::String("Codex Desktop".into()),
    );
    headers
}

async fn fetch_codex_accounts(
    client: &reqwest::Client,
    base_url: &str,
    management_key: &str,
) -> Result<Vec<CpaCodexAccount>, ProviderError> {
    let response = client
        .get(auth_files_url(base_url))
        .header("X-Management-Key", management_key)
        .send()
        .await
        .map_err(|error| ProviderError::Network(error.to_string()))?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err(ProviderError::Auth(format!("CLIProxyAPI HTTP {status}")));
    }
    if !status.is_success() {
        return Err(ProviderError::Other(format!("CLIProxyAPI HTTP {status}")));
    }
    let json: serde_json::Value = response
        .json()
        .await
        .map_err(|error| ProviderError::Parse(error.to_string()))?;
    Ok(parse_codex_accounts(&json))
}

async fn fetch_codex_account(
    client: &reqwest::Client,
    base_url: &str,
    management_key: &str,
    auth_index: &str,
) -> Result<Option<CpaCodexAccount>, ProviderError> {
    Ok(fetch_codex_accounts(client, base_url, management_key)
        .await?
        .into_iter()
        .find(|account| account.auth_index == auth_index))
}

fn parse_codex_accounts(json: &serde_json::Value) -> Vec<CpaCodexAccount> {
    let mut accounts = json
        .get("files")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter(|item| {
            let account_type = value_by_keys(item, &["account_type", "accountType"])
                .map(|value| value.to_ascii_lowercase());
            if account_type.as_deref() == Some("api_key") {
                return false;
            }
            ["provider", "type"]
                .iter()
                .filter_map(|key| item.get(key).and_then(value_string))
                .any(|value| matches!(value.to_ascii_lowercase().as_str(), "codex" | "openai"))
        })
        .filter_map(|item| {
            let auth_index = value_by_keys(item, &["auth_index", "authIndex"])?;
            let account_id =
                claim_by_keys(item, &["chatgpt_account_id", "account_id", "accountId"]);
            let plan_type = claim_by_keys(item, &["plan_type", "planType"]);
            let display_name = value_by_keys(item, &["label", "email", "name", "id"])
                .unwrap_or_else(|| format!("Codex {}", short_auth_index(&auth_index)));
            Some(CpaCodexAccount {
                auth_index,
                display_name,
                account_id,
                plan_type,
                disabled: item
                    .get("disabled")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
            })
        })
        .collect::<Vec<_>>();
    accounts.sort_by(|left, right| {
        left.display_name
            .to_ascii_lowercase()
            .cmp(&right.display_name.to_ascii_lowercase())
    });
    accounts
}

fn short_auth_index(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}

fn value_string(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
}

fn value_by_keys(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(key).and_then(value_string))
}

fn claim_containers(value: &serde_json::Value) -> Vec<&serde_json::Value> {
    let metadata = value.get("metadata");
    let attributes = value.get("attributes");
    [
        value.get("id_token"),
        metadata.and_then(|item| item.get("id_token")),
        attributes.and_then(|item| item.get("id_token")),
        Some(value),
        metadata,
        attributes,
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn claim_by_keys(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    claim_containers(value)
        .into_iter()
        .find_map(|container| value_by_keys(container, keys))
}

fn parse_datetime_value(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .or_else(|| number(value).and_then(|value| DateTime::from_timestamp(value as i64, 0)))
}

fn transient_status(status: u64) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

fn retry_delay(attempt: usize) -> Duration {
    match attempt {
        1 => Duration::from_millis(400),
        _ => Duration::from_millis(1_200),
    }
}

fn response_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|json| {
            json.get("error")
                .and_then(|value| value.as_str())
                .or_else(|| json.get("message").and_then(|value| value.as_str()))
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            let compact = body.split_whitespace().collect::<Vec<_>>().join(" ");
            if compact.is_empty() {
                "empty response".into()
            } else {
                compact.chars().take(240).collect()
            }
        })
}

fn parse_usage(json: &serde_json::Value, quota_window: &str) -> Result<Usage, ProviderError> {
    let mut candidates = Vec::new();
    if let Some(rate_limit) = value_by_aliases(json, &["rate_limit", "rateLimit"]) {
        collect_rate_limit_windows(&mut candidates, rate_limit, "codex", None);
    }
    if let Some(rate_limit) =
        value_by_aliases(json, &["code_review_rate_limit", "codeReviewRateLimit"])
    {
        collect_rate_limit_windows(
            &mut candidates,
            rate_limit,
            "code-review",
            Some("Code Review"),
        );
    }
    if let Some(additional) =
        value_by_aliases(json, &["additional_rate_limits", "additionalRateLimits"])
            .and_then(|value| value.as_array())
    {
        for (index, item) in additional.iter().enumerate() {
            let Some(rate_limit) = value_by_aliases(item, &["rate_limit", "rateLimit"]) else {
                continue;
            };
            let name = value_by_aliases(
                item,
                &[
                    "limit_name",
                    "limitName",
                    "metered_feature",
                    "meteredFeature",
                ],
            )
            .and_then(value_string)
            .unwrap_or_else(|| format!("额外额度 {}", index + 1));
            collect_rate_limit_windows(
                &mut candidates,
                rate_limit,
                &format!("additional-{}", index + 1),
                Some(&name),
            );
        }
    }
    if candidates.is_empty() {
        return Err(ProviderError::NotFound(
            "Codex response contains no quota windows".into(),
        ));
    }

    let selector = quota_window.trim();
    let summary_candidates = if !selector.is_empty() && selector != "auto" {
        let matches = candidates
            .iter()
            .filter(|candidate| window_matches(selector, candidate.window_seconds))
            .cloned()
            .collect::<Vec<_>>();
        if matches.is_empty() {
            candidates.clone()
        } else {
            matches
        }
    } else {
        candidates.clone()
    };

    let selected = summary_candidates
        .iter()
        .max_by(|left, right| left.used_percent.total_cmp(&right.used_percent))
        .cloned()
        .ok_or_else(|| ProviderError::NotFound("no matching Codex quota window".into()))?;
    let windows = candidates
        .into_iter()
        .map(|candidate| UsageWindow {
            key: candidate.key,
            label: candidate.label,
            used: candidate.used_percent.clamp(0.0, 100.0),
            total: 100.0,
            unit: "%".into(),
            reset_at: candidate.reset_at,
        })
        .collect();
    let plan_type = value_by_aliases(json, &["plan_type", "planType"])
        .and_then(value_string)
        .map(|value| value.to_ascii_lowercase());
    let subscription_active_until = value_by_aliases(
        json,
        &[
            "chatgpt_subscription_active_until",
            "subscription_active_until",
            "subscriptionActiveUntil",
        ],
    )
    .and_then(parse_datetime_value);
    let reset_credits =
        value_by_aliases(json, &["rate_limit_reset_credits", "rateLimitResetCredits"])
            .and_then(|value| parse_reset_credits(value).ok());

    Ok(Usage {
        used: selected.used_percent.clamp(0.0, 100.0),
        total: 100.0,
        unit: "%".into(),
        label: Some(selected.label),
        reset_at: selected.reset_at,
        fetched_at: Some(Utc::now()),
        windows,
        balance: None,
        reset_credits,
        codex_account: if plan_type.is_some() || subscription_active_until.is_some() {
            Some(CodexAccountDetails {
                plan_type,
                subscription_active_until,
            })
        } else {
            None
        },
    })
}

fn parse_reset_credits(json: &serde_json::Value) -> Result<ResetCreditDetails, ProviderError> {
    if ![
        "credits",
        "available_count",
        "availableCount",
        "applicable_available_count",
        "applicableAvailableCount",
    ]
    .iter()
    .any(|key| json.get(key).is_some())
    {
        return Err(ProviderError::Parse(
            "invalid Codex reset credits response".into(),
        ));
    }
    let mut credits = json
        .get("credits")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter(|credit| {
            value_by_aliases(credit, &["reset_type", "resetType"])
                .and_then(value_string)
                .as_deref()
                == Some("codex_rate_limits")
        })
        .filter(|credit| credit.get("status").and_then(|value| value.as_str()) == Some("available"))
        .filter_map(|credit| {
            let expires_at = value_by_aliases(credit, &["expires_at", "expiresAt"])
                .and_then(|value| value.as_str())
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc))?;
            Some(ResetCreditEntry {
                expires_at: Some(expires_at),
            })
        })
        .collect::<Vec<_>>();
    credits.sort_by(|left, right| match (&left.expires_at, &right.expires_at) {
        (Some(left), Some(right)) => left.cmp(right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    let available_count = value_by_aliases(json, &["available_count", "availableCount"])
        .and_then(number)
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as u64)
        .unwrap_or(credits.len() as u64);
    let applicable_available_count = value_by_aliases(
        json,
        &["applicable_available_count", "applicableAvailableCount"],
    )
    .and_then(number)
    .filter(|value| value.is_finite() && *value >= 0.0)
    .map(|value| value as u64);

    Ok(ResetCreditDetails {
        available_count,
        applicable_available_count,
        credits,
        immediate_reset_purchase_eligible: json
            .get("immediate_reset_purchase_eligible")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    })
}

fn collect_rate_limit_windows(
    candidates: &mut Vec<QuotaCandidate>,
    rate_limit: &serde_json::Value,
    key_prefix: &str,
    label_prefix: Option<&str>,
) {
    let limit_reached = value_by_aliases(rate_limit, &["limit_reached", "limitReached"])
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let allowed = rate_limit.get("allowed").and_then(|value| value.as_bool());
    let windows = [
        value_by_aliases(rate_limit, &["primary_window", "primaryWindow"]),
        value_by_aliases(rate_limit, &["secondary_window", "secondaryWindow"]),
    ];
    let durations = windows.map(|window| {
        window
            .and_then(|value| {
                value_by_aliases(value, &["limit_window_seconds", "limitWindowSeconds"])
            })
            .and_then(number)
            .map(|value| value as i64)
    });
    let mut five_hour_index = (0..windows.len())
        .find(|index| windows[*index].is_some() && durations[*index] == Some(FIVE_HOURS_SECONDS));
    let mut secondary_index = (0..windows.len()).find(|index| {
        windows[*index].is_some()
            && durations[*index]
                .map(|seconds| seconds == WEEK_SECONDS || is_monthly_window(seconds))
                .unwrap_or(false)
    });
    if five_hour_index.is_none() && windows[0].is_some() && secondary_index != Some(0) {
        five_hour_index = Some(0);
    }
    if secondary_index.is_none() && windows[1].is_some() && five_hour_index != Some(1) {
        secondary_index = Some(1);
    }

    let mut classified = Vec::new();
    if let Some(index) = five_hour_index {
        classified.push((index, FIVE_HOURS_SECONDS));
    }
    if let Some(index) = secondary_index {
        let seconds = durations[index]
            .filter(|seconds| is_monthly_window(*seconds))
            .unwrap_or(WEEK_SECONDS);
        classified.push((index, seconds));
    }
    for (index, semantic_seconds) in classified {
        let Some(window) = windows[index] else {
            continue;
        };
        let position = if index == 0 { "primary" } else { "secondary" };
        if let Some(candidate) = parse_candidate(
            window,
            key_prefix,
            position,
            label_prefix,
            semantic_seconds,
            limit_reached,
            allowed,
        ) {
            candidates.push(candidate);
        }
    }
}

fn parse_candidate(
    value: &serde_json::Value,
    key_prefix: &str,
    position: &str,
    label_prefix: Option<&str>,
    window_seconds: i64,
    limit_reached: bool,
    allowed: Option<bool>,
) -> Option<QuotaCandidate> {
    if value.is_null() {
        return None;
    }
    let used_percent = value_by_aliases(value, &["used_percent", "usedPercent"])
        .and_then(number)
        .or_else(|| (limit_reached || allowed == Some(false)).then_some(100.0))?;
    let reset_at = value_by_aliases(value, &["reset_at", "resetAt"])
        .and_then(number)
        .and_then(|value| DateTime::from_timestamp(value as i64, 0))
        .or_else(|| {
            value_by_aliases(value, &["reset_after_seconds", "resetAfterSeconds"])
                .and_then(number)
                .map(|seconds| Utc::now() + ChronoDuration::seconds(seconds as i64))
        });
    let duration_label = window_label(window_seconds);
    let label = label_prefix
        .map(|prefix| format!("{prefix} · {duration_label}"))
        .unwrap_or_else(|| duration_label.into());
    Some(QuotaCandidate {
        key: format!("{key_prefix}-{position}-{}", window_kind(window_seconds)),
        label,
        used_percent,
        window_seconds,
        reset_at,
    })
}

fn value_by_aliases<'a>(
    value: &'a serde_json::Value,
    keys: &[&str],
) -> Option<&'a serde_json::Value> {
    keys.iter().find_map(|key| value.get(key))
}

fn number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn window_matches(selector: &str, seconds: i64) -> bool {
    match selector {
        "five_hour" => seconds == FIVE_HOURS_SECONDS,
        "weekly" => seconds == WEEK_SECONDS,
        "monthly" => is_monthly_window(seconds),
        _ => false,
    }
}

fn window_label(seconds: i64) -> &'static str {
    match seconds {
        FIVE_HOURS_SECONDS => "5 小时",
        WEEK_SECONDS => "7 天",
        seconds if is_monthly_window(seconds) => "每月",
        _ => "额度",
    }
}

fn window_kind(seconds: i64) -> &'static str {
    match seconds {
        FIVE_HOURS_SECONDS => "five-hour",
        WEEK_SECONDS => "weekly",
        seconds if is_monthly_window(seconds) => "monthly",
        _ => "custom",
    }
}

fn is_monthly_window(seconds: i64) -> bool {
    (MIN_MONTH_SECONDS..=MAX_MONTH_SECONDS).contains(&seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recognizes_weekly_primary_window_by_duration() {
        let usage = parse_usage(
            &json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 30,
                        "limit_window_seconds": 604800,
                        "reset_at": 1787018614
                    },
                    "secondary_window": null
                }
            }),
            "weekly",
        )
        .unwrap();

        assert_eq!(usage.used, 30.0);
        assert_eq!(usage.reset_at, DateTime::from_timestamp(1_787_018_614, 0));
    }

    #[test]
    fn selected_window_does_not_hide_other_returned_windows() {
        let usage = parse_usage(
            &json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 20,
                        "limit_window_seconds": 18000
                    },
                    "secondary_window": {
                        "used_percent": 40,
                        "limit_window_seconds": 604800
                    }
                },
                "code_review_rate_limit": {
                    "primary_window": {
                        "used_percent": 60,
                        "limit_window_seconds": 18000
                    }
                }
            }),
            "weekly",
        )
        .unwrap();

        assert_eq!(usage.used, 40.0);
        assert_eq!(usage.windows.len(), 3);
    }

    #[test]
    fn legacy_primary_window_without_duration_is_five_hours() {
        let usage = parse_usage(
            &json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 35,
                        "reset_at": 1787018614
                    },
                    "secondary_window": {
                        "used_percent": 15,
                        "reset_at": 1787018614
                    }
                }
            }),
            "auto",
        )
        .unwrap();

        assert!(usage.windows.iter().any(|window| window.label == "5 小时"));
        assert!(usage.windows.iter().any(|window| window.label == "7 天"));
    }

    #[test]
    fn auto_uses_tighter_window_regardless_of_position() {
        let usage = parse_usage(
            &json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 25,
                        "limit_window_seconds": 18000
                    },
                    "secondary_window": {
                        "used_percent": 72,
                        "limit_window_seconds": 604800
                    }
                }
            }),
            "auto",
        )
        .unwrap();

        assert_eq!(usage.used, 72.0);
    }

    #[test]
    fn falls_back_when_requested_window_is_missing() {
        let usage = parse_usage(
            &json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 30,
                        "limit_window_seconds": 604800
                    },
                    "secondary_window": null
                }
            }),
            "five_hour",
        )
        .unwrap();

        assert_eq!(usage.used, 30.0);
        assert_eq!(usage.label.as_deref(), Some("7 天"));
    }

    #[test]
    fn unknown_primary_duration_uses_five_hour_semantics() {
        let usage = parse_usage(
            &json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 35,
                        "limit_window_seconds": 14400
                    },
                    "secondary_window": {
                        "used_percent": 20,
                        "limit_window_seconds": 604800
                    }
                }
            }),
            "auto",
        )
        .unwrap();

        assert!(usage.windows.iter().any(|window| window.label == "5 小时"));
    }

    #[test]
    fn parses_available_manual_reset_credits() {
        let credits = parse_reset_credits(&json!({
            "credits": [
                {
                    "reset_type": "codex_rate_limits",
                    "status": "available",
                    "expires_at": "2026-09-24T00:05:56.042191Z",
                    "redeemed_at": null
                },
                {
                    "reset_type": "codex_rate_limits",
                    "status": "available",
                    "expires_at": "2026-09-22T00:05:56.042191Z",
                    "redeemed_at": null
                },
                {
                    "reset_type": "codex_rate_limits",
                    "status": "available",
                    "expires_at": "2026-09-21T00:05:56.042191Z",
                    "redeemed_at": null
                },
                {
                    "reset_type": "codex_rate_limits",
                    "status": "available",
                    "expires_at": "2026-09-23T00:05:56.042191Z",
                    "redeemed_at": null
                },
                {
                    "reset_type": "codex_rate_limits",
                    "status": "redeemed",
                    "expires_at": "2026-09-01T00:05:56.042191Z",
                    "redeemed_at": "2026-08-25T00:05:56.042191Z"
                },
                {
                    "reset_type": "other_product",
                    "status": "available",
                    "expires_at": "2026-08-30T00:05:56.042191Z",
                    "redeemed_at": null
                }
            ],
            "available_count": 4,
            "applicable_available_count": 3,
            "total_earned_count": 5,
            "immediate_reset_purchase_eligible": true
        }))
        .unwrap();

        assert_eq!(credits.available_count, 4);
        assert_eq!(credits.applicable_available_count, Some(3));
        assert_eq!(
            credits.credits[0].expires_at,
            DateTime::parse_from_rfc3339("2026-09-21T00:05:56.042191Z")
                .ok()
                .map(|value| value.with_timezone(&Utc))
        );
        assert_eq!(credits.credits.len(), 4);
        assert_eq!(
            credits.credits[3].expires_at,
            DateTime::parse_from_rfc3339("2026-09-24T00:05:56.042191Z")
                .ok()
                .map(|value| value.with_timezone(&Utc))
        );
        assert!(credits.immediate_reset_purchase_eligible);
    }

    #[test]
    fn reset_credit_headers_match_management_center() {
        let headers = build_reset_credit_headers(&build_codex_headers(Some("account-id")));

        assert_eq!(
            headers.get("Accept").and_then(|value| value.as_str()),
            Some("application/json")
        );
        assert_eq!(
            headers.get("OpenAI-Beta").and_then(|value| value.as_str()),
            Some("codex-1")
        );
        assert_eq!(
            headers.get("Originator").and_then(|value| value.as_str()),
            Some("Codex Desktop")
        );
        assert_eq!(
            headers
                .get("Chatgpt-Account-Id")
                .and_then(|value| value.as_str()),
            Some("account-id")
        );
    }

    #[test]
    fn keeps_zero_reset_credit_count_visible() {
        let credits = parse_reset_credits(&json!({
            "credits": [],
            "available_count": 0,
            "total_earned_count": 0,
            "immediate_reset_purchase_eligible": false
        }))
        .unwrap();

        assert_eq!(credits.available_count, 0);
        assert_eq!(credits.applicable_available_count, None);
        assert!(credits.credits.is_empty());
    }

    #[test]
    fn rejects_unrecognized_reset_credit_payload() {
        assert!(parse_reset_credits(&json!({ "status": "ok" })).is_err());
    }

    #[test]
    fn normalizes_cpa_management_urls() {
        for base in [
            "http://127.0.0.1:8317",
            "http://127.0.0.1:8317/v0/management",
            "http://127.0.0.1:8317/v0/management/api-call",
        ] {
            assert_eq!(
                api_call_url(base),
                "http://127.0.0.1:8317/v0/management/api-call"
            );
            assert_eq!(
                auth_files_url(base),
                "http://127.0.0.1:8317/v0/management/auth-files"
            );
        }
    }

    #[test]
    fn parses_code_review_and_additional_windows() {
        let usage = parse_usage(
            &json!({
                "plan_type": "plus",
                "subscription_active_until": "2026-09-30T00:00:00Z",
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 20,
                        "limit_window_seconds": 18000,
                        "reset_at": 1787018614
                    },
                    "secondary_window": {
                        "used_percent": 30,
                        "limit_window_seconds": 604800,
                        "reset_at": 1787018614
                    }
                },
                "code_review_rate_limit": {
                    "primary_window": {
                        "used_percent": 45,
                        "limit_window_seconds": 18000,
                        "reset_at": 1787018614
                    }
                },
                "additional_rate_limits": [{
                    "limit_name": "Spark",
                    "rate_limit": {
                        "secondary_window": {
                            "used_percent": 65,
                            "limit_window_seconds": 2592000,
                            "reset_at": 1787018614
                        }
                    }
                }]
            }),
            "auto",
        )
        .unwrap();

        assert_eq!(usage.windows.len(), 4);
        assert_eq!(usage.used, 65.0);
        assert_eq!(usage.label.as_deref(), Some("Spark · 每月"));
        assert!(usage
            .windows
            .iter()
            .any(|window| window.label == "Code Review · 5 小时"));
        assert_eq!(
            usage
                .codex_account
                .as_ref()
                .and_then(|details| details.plan_type.as_deref()),
            Some("plus")
        );
        assert!(usage
            .codex_account
            .as_ref()
            .and_then(|details| details.subscription_active_until.as_ref())
            .is_some());
    }

    #[test]
    fn discovers_codex_accounts_from_auth_files() {
        let accounts = parse_codex_accounts(&json!({
            "files": [
                {
                    "provider": "codex",
                    "auth_index": "abcdef0123456789",
                    "email": "user@example.com",
                    "account": "user@example.com",
                    "disabled": false,
                    "id_token": {
                        "chatgpt_account_id": "00000000-0000-4000-8000-000000000000",
                        "plan_type": "pro",
                        "chatgpt_subscription_active_until": "2026-09-30T00:00:00Z"
                    }
                },
                {
                    "provider": "claude",
                    "auth_index": "ignored"
                },
                {
                    "provider": "codex",
                    "account_type": "api_key",
                    "auth_index": "api-key-entry"
                }
            ]
        }));

        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].auth_index, "abcdef0123456789");
        assert_eq!(accounts[0].display_name, "user@example.com");
        assert_eq!(
            accounts[0].account_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000000")
        );
        assert_eq!(accounts[0].plan_type.as_deref(), Some("pro"));
        assert!(serde_json::to_value(&accounts[0])
            .unwrap()
            .get("subscription_active_until")
            .is_none());
    }

    #[test]
    fn retries_common_proxy_and_upstream_failures() {
        for status in [408, 429, 500, 502, 503, 504] {
            assert!(transient_status(status));
        }
        for status in [400, 401, 403, 404] {
            assert!(!transient_status(status));
        }
    }
}
