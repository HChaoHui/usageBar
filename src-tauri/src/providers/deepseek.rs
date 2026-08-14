//! DeepSeek account balance Provider.

use super::{
    network_error, secure_http_client, validate_endpoint, BalanceDetails, Provider, ProviderError,
    Usage,
};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const DEEPSEEK_BALANCE_ENDPOINT: &str = "https://api.deepseek.com/user/balance";

#[derive(Clone, Serialize, Deserialize)]
pub struct DeepSeekProvider {
    pub id: String,
    pub display_name: String,
    pub icon: String,
    pub color: String,
    pub unit: String,
    pub api_key: String,
    pub endpoint: String,
    pub currency: String,
}

#[async_trait]
impl Provider for DeepSeekProvider {
    async fn fetch(&self) -> Result<Usage, ProviderError> {
        let endpoint = if self.endpoint.trim().is_empty() {
            DEEPSEEK_BALANCE_ENDPOINT
        } else {
            self.endpoint.trim()
        };
        if self.api_key.trim().is_empty() {
            return Err(ProviderError::Auth("DeepSeek API key is required".into()));
        }
        validate_endpoint(endpoint, true)?;

        let resp = secure_http_client()?
            .get(endpoint)
            .bearer_auth(self.api_key.trim())
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|error| network_error(&error))?;

        let status = resp.status();
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProviderError::Auth(format!("HTTP {status}")));
        }
        if !status.is_success() {
            return Err(ProviderError::Other(format!("HTTP {status}")));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|error| ProviderError::Parse(error.to_string()))?;
        parse_usage(&json, &self.currency)
    }
}

fn parse_usage(json: &serde_json::Value, preferred_currency: &str) -> Result<Usage, ProviderError> {
    let available = json
        .get("is_available")
        .and_then(|value| value.as_bool())
        .ok_or_else(|| ProviderError::Parse("missing is_available".into()))?;
    let balances = json
        .get("balance_infos")
        .and_then(|value| value.as_array())
        .ok_or_else(|| ProviderError::Parse("missing balance_infos array".into()))?;
    let preferred = preferred_currency.trim();
    let selected = balances
        .iter()
        .find(|item| {
            item.get("currency")
                .and_then(|value| value.as_str())
                .map(|currency| currency.eq_ignore_ascii_case(preferred))
                .unwrap_or(false)
        })
        .or_else(|| balances.first())
        .ok_or_else(|| ProviderError::NotFound("DeepSeek returned no balances".into()))?;

    let currency = selected
        .get("currency")
        .and_then(|value| value.as_str())
        .ok_or_else(|| ProviderError::Parse("missing balance currency".into()))?
        .to_uppercase();
    let total = amount(selected, "total_balance")?;
    let granted = amount(selected, "granted_balance")?;
    let topped_up = amount(selected, "topped_up_balance")?;

    Ok(Usage {
        used: 0.0,
        total,
        unit: currency.clone(),
        label: Some("账户余额".into()),
        reset_at: None,
        fetched_at: Some(Utc::now()),
        windows: vec![],
        balance: Some(BalanceDetails {
            currency,
            total,
            granted,
            topped_up,
            available,
        }),
    })
}

fn amount(item: &serde_json::Value, key: &str) -> Result<f64, ProviderError> {
    item.get(key)
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
        .ok_or_else(|| ProviderError::Parse(format!("missing/invalid {key}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selects_preferred_currency_and_parses_string_amounts() {
        let usage = parse_usage(
            &json!({
                "is_available": true,
                "balance_infos": [
                    {
                        "currency": "USD",
                        "total_balance": "0.00",
                        "granted_balance": "0.00",
                        "topped_up_balance": "0.00"
                    },
                    {
                        "currency": "CNY",
                        "total_balance": "0.75",
                        "granted_balance": "0.00",
                        "topped_up_balance": "0.75"
                    }
                ]
            }),
            "CNY",
        )
        .unwrap();

        let balance = usage.balance.unwrap();
        assert_eq!(balance.currency, "CNY");
        assert_eq!(balance.total, 0.75);
        assert_eq!(balance.topped_up, 0.75);
        assert!(balance.available);
    }

    #[test]
    fn falls_back_to_first_returned_currency() {
        let usage = parse_usage(
            &json!({
                "is_available": false,
                "balance_infos": [{
                    "currency": "USD",
                    "total_balance": "2.50",
                    "granted_balance": "1.00",
                    "topped_up_balance": "1.50"
                }]
            }),
            "CNY",
        )
        .unwrap();

        let balance = usage.balance.unwrap();
        assert_eq!(balance.currency, "USD");
        assert_eq!(balance.total, 2.5);
        assert!(!balance.available);
    }
}
