//! Independent Cline OAuth (WorkOS Device Flow) for the ClinePass provider.
//! Secrets never cross the IPC boundary.
use crate::oauth_vault::{now, Vault};
use chrono::DateTime;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::Duration,
};
#[cfg(test)]
use std::{fs, path::Path};
use tokio::sync::{Mutex, OwnedMutexGuard};

const WORKOS_ORIGIN: &str = "https://api.workos.com";
const WORKOS_CLIENT_ID: &str = "client_01K3A541FN8TA3EPPHTD2325AR";
const CLINE_ORIGIN: &str = "https://api.cline.bot";
const DEVICE_PATH: &str = "/user_management/authorize/device";
const AUTHENTICATE_PATH: &str = "/user_management/authenticate";
const REGISTER_PATH: &str = "/api/v1/auth/register";
const REFRESH_PATH: &str = "/api/v1/auth/refresh";
const VAULT_LABEL: &str = "Cline";
const VAULT_AAD: &[u8] = b"usagebar-cline-v1";
const VAULT_LOCK: &str = "另一个 usageBar 实例正在使用 Cline 登录凭证";
static ACCOUNTS: OnceLock<StdMutex<Accounts>> = OnceLock::new();

#[derive(Clone, Serialize, Deserialize)]
struct Grant {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    #[serde(default)]
    reauth_required: bool,
}

struct Device {
    id: String,
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_at: i64,
    interval_ms: i64,
    next_poll: i64,
}

#[derive(Serialize)]
pub struct AuthView {
    pub phase: &'static str,
    session_id: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    expires_at: Option<i64>,
}

pub struct Session {
    vault: Vault,
    client: Client,
    grant: Option<Grant>,
    device: Option<Device>,
    dirty: bool,
    #[cfg(test)]
    test_origin: Option<String>,
}

struct Accounts {
    root: Vault,
    client: Client,
    sessions: HashMap<String, Arc<Mutex<Session>>>,
}

impl Accounts {
    fn open(dir: PathBuf) -> Result<Self, String> {
        let root = Vault::open(dir, VAULT_LABEL, VAULT_AAD, VAULT_LOCK)?;
        let client = Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "无法创建 Cline OAuth 客户端")?;
        Ok(Self {
            root,
            client,
            sessions: HashMap::new(),
        })
    }

    fn account_dir(&self, id: &str) -> PathBuf {
        self.root.account_dir(id)
    }

    fn get(&mut self, id: &str) -> Result<Arc<Mutex<Session>>, String> {
        if id.trim().is_empty() {
            return Err("ClinePass Provider ID 不能为空".into());
        }
        if let Some(session) = self.sessions.get(id) {
            return Ok(session.clone());
        }
        let vault = Vault::open(self.account_dir(id), VAULT_LABEL, VAULT_AAD, VAULT_LOCK)?;
        let grant = vault.load::<Grant>()?;
        let session = Arc::new(Mutex::new(Session {
            vault,
            client: self.client.clone(),
            grant,
            device: None,
            dirty: false,
            #[cfg(test)]
            test_origin: None,
        }));
        self.sessions.insert(id.to_owned(), session.clone());
        Ok(session)
    }
}

pub fn init(dir: PathBuf) -> Result<(), String> {
    ACCOUNTS
        .set(StdMutex::new(Accounts::open(dir)?))
        .map_err(|_| "Cline OAuth 已初始化".into())
}

pub async fn session(id: &str) -> Result<OwnedMutexGuard<Session>, String> {
    let session = ACCOUNTS
        .get()
        .ok_or("Cline OAuth 尚未初始化")?
        .lock()
        .map_err(|_| "Cline 账号存储不可用")?
        .get(id)?;
    Ok(session.lock_owned().await)
}

async fn post(
    client: &Client,
    origin: &str,
    path: &str,
    form: Option<&[(&str, &str)]>,
    json_body: Option<&Value>,
) -> Result<Value, String> {
    let mut request = client
        .post(format!("{origin}{path}"))
        .header("Accept", "application/json")
        .header("User-Agent", "usageBar");
    request = match json_body {
        Some(body) => request
            .header("Content-Type", "application/json")
            .json(body),
        None => request.form(form.unwrap_or(&[])),
    };
    let response = request
        .send()
        .await
        .map_err(|_| "Cline OAuth 网络请求失败，请重试")?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|_| "Cline OAuth 响应读取失败")?;
    let body: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&text).map_err(|_| "Cline OAuth 返回无效 JSON")?
    };
    if !body.is_object() {
        return Err("Cline OAuth 返回无效 JSON 对象".into());
    }
    if let Some(error) = body.get("error").and_then(Value::as_str) {
        return Err(match error {
            "authorization_pending" => "authorization_pending",
            "slow_down" => "slow_down",
            "access_denied" => "access_denied",
            "expired_token" => "expired_token",
            "invalid_grant" => "invalid_grant",
            "invalid_token" => "invalid_token",
            _ => "Cline OAuth 授权请求失败",
        }
        .into());
    }
    if !status.is_success() || body.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(format!("Cline OAuth 请求失败（HTTP {}）", status.as_u16()));
    }
    Ok(body)
}

fn string(body: &Value, key: &str) -> Result<String, String> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("Cline OAuth 响应缺少 {key}"))
}

fn parse_iso_millis(value: &Value) -> Option<i64> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|time| time.timestamp_millis())
}

fn parse_grant(body: &Value, previous_refresh: Option<&str>) -> Result<Grant, String> {
    let data = body
        .get("data")
        .filter(|data| data.is_object())
        .ok_or("Cline OAuth 响应缺少 data")?;
    let access_token = string(data, "accessToken")?;
    let refresh_token = match data.get("refreshToken").and_then(Value::as_str) {
        Some(token) if !token.is_empty() => token.to_owned(),
        _ => previous_refresh
            .filter(|token| !token.is_empty())
            .ok_or("Cline OAuth 响应缺少 Refresh Token")?
            .to_owned(),
    };
    let expires_at = data
        .get("expiresAt")
        .and_then(parse_iso_millis)
        .ok_or("Cline OAuth 响应缺少有效期")?;
    Ok(Grant {
        access_token,
        refresh_token,
        expires_at,
        reauth_required: false,
    })
}

fn parse_device(body: &Value) -> Result<Device, String> {
    let user_code = string(body, "user_code")?;
    let device_code = string(body, "device_code")?;
    let uri = body
        .get("verification_uri_complete")
        .or_else(|| body.get("verification_uri"))
        .and_then(Value::as_str)
        .filter(|uri| uri.starts_with("https://"))
        .ok_or("Cline OAuth 缺少授权地址")?;
    let seconds = match body.get("expires_in").and_then(Value::as_i64) {
        Some(seconds) if (1..=86400).contains(&seconds) => seconds,
        _ => return Err("Cline OAuth 授权响应无效".into()),
    };
    let interval = body
        .get("interval")
        .and_then(Value::as_i64)
        .filter(|interval| *interval > 0)
        .map(|interval| interval.saturating_mul(1000))
        .unwrap_or(5000)
        .clamp(1000, 86400000);
    Ok(Device {
        id: uuid::Uuid::new_v4().to_string(),
        device_code,
        user_code,
        verification_uri: uri.to_owned(),
        expires_at: now() + seconds * 1000,
        interval_ms: interval,
        next_poll: now() + interval,
    })
}

impl Session {
    fn api_origin(&self) -> &str {
        #[cfg(test)]
        if let Some(origin) = &self.test_origin {
            return origin;
        }
        CLINE_ORIGIN
    }

    fn workos_origin(&self) -> &str {
        #[cfg(test)]
        if let Some(origin) = &self.test_origin {
            return origin;
        }
        WORKOS_ORIGIN
    }

    pub fn view(&self) -> AuthView {
        if let Some(device) = &self.device {
            return AuthView {
                phase: "pending",
                session_id: Some(device.id.clone()),
                user_code: Some(device.user_code.clone()),
                verification_uri: Some(device.verification_uri.clone()),
                expires_at: Some(device.expires_at),
            };
        }
        AuthView {
            phase: match &self.grant {
                Some(grant) if grant.reauth_required => "reauth_required",
                Some(_) => "connected",
                None => "idle",
            },
            session_id: None,
            user_code: None,
            verification_uri: None,
            expires_at: self.grant.as_ref().map(|grant| grant.expires_at),
        }
    }

    fn persist(&mut self) -> Result<(), String> {
        if let Some(grant) = &self.grant {
            self.vault.save(grant)?;
        }
        self.dirty = false;
        Ok(())
    }

    pub async fn token(&mut self, force: bool) -> Result<String, String> {
        if self.dirty {
            self.persist()?;
        }
        let grant = self.grant.as_ref().ok_or("请在设置中登录 ClinePass 账号")?;
        if grant.reauth_required {
            return Err("ClinePass 授权已失效，请在设置中退出后重新登录".into());
        }
        if force || grant.expires_at <= now() + 60_000 {
            let body = json!({
                "refreshToken": &grant.refresh_token,
                "grantType": "refresh_token",
            });
            match post(
                &self.client,
                self.api_origin(),
                REFRESH_PATH,
                None,
                Some(&body),
            )
            .await
            {
                Ok(body) => {
                    let renewed = parse_grant(&body, Some(&grant.refresh_token))?;
                    self.grant = Some(renewed);
                    self.dirty = true;
                    self.persist()?;
                }
                Err(error) => {
                    if matches!(
                        error.as_str(),
                        "invalid_grant" | "invalid_token" | "access_denied"
                    ) {
                        self.grant.as_mut().unwrap().reauth_required = true;
                        self.dirty = true;
                        self.persist()?;
                        return Err("ClinePass 授权已失效，请在设置中退出后重新登录".into());
                    }
                    return Err(error);
                }
            }
        }
        Ok(self.grant.as_ref().unwrap().access_token.clone())
    }

    pub async fn start(&mut self) -> Result<AuthView, String> {
        if self.grant.is_some() {
            return Err("请先退出当前 ClinePass 账号".into());
        }
        self.device = None;
        let body = post(
            &self.client,
            self.workos_origin(),
            DEVICE_PATH,
            Some(&[("client_id", WORKOS_CLIENT_ID)]),
            None,
        )
        .await?;
        self.device = Some(parse_device(&body)?);
        Ok(self.view())
    }

    pub async fn poll(&mut self, id: &str) -> Result<AuthView, String> {
        let workos_origin = self.workos_origin().to_owned();
        let device = self
            .device
            .as_mut()
            .filter(|device| device.id == id)
            .ok_or("授权会话已结束")?;
        if now() >= device.expires_at {
            self.device = None;
            return Err("授权码已过期，请重新登录".into());
        }
        if now() < device.next_poll {
            return Ok(self.view());
        }
        device.next_poll = now() + device.interval_ms;
        let result = post(
            &self.client,
            &workos_origin,
            AUTHENTICATE_PATH,
            Some(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", &device.device_code),
                ("client_id", WORKOS_CLIENT_ID),
            ]),
            None,
        )
        .await;
        let workos = match result {
            Ok(body) => body,
            Err(error) if error == "authorization_pending" => return Ok(self.view()),
            Err(error) if error == "slow_down" => {
                device.interval_ms += 5000;
                device.next_poll = now() + device.interval_ms;
                return Ok(self.view());
            }
            Err(error) => {
                self.device = None;
                return Err(error);
            }
        };
        let access_token = string(&workos, "access_token").inspect_err(|_| self.device = None)?;
        let refresh_token = string(&workos, "refresh_token").inspect_err(|_| self.device = None)?;
        self.device = None;
        // Exchange the WorkOS session for Cline API credentials.
        let body = post(
            &self.client,
            self.api_origin(),
            REGISTER_PATH,
            None,
            Some(&json!({
                "accessToken": access_token,
                "refreshToken": refresh_token,
            })),
        )
        .await?;
        self.grant = Some(parse_grant(&body, None)?);
        self.dirty = true;
        self.persist()?;
        Ok(self.view())
    }

    pub fn cancel(&mut self) {
        self.device = None;
    }

    pub fn verification_uri(&self) -> Result<&str, String> {
        self.device
            .as_ref()
            .filter(|device| device.expires_at > now())
            .map(|device| device.verification_uri.as_str())
            .ok_or("没有待完成的授权".into())
    }

    pub async fn logout(&mut self) -> Result<Option<String>, String> {
        self.forget()?;
        Ok(None)
    }

    pub fn forget(&mut self) -> Result<(), String> {
        self.vault.clear()?;
        self.device = None;
        self.grant = None;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_vault(path: PathBuf) -> Result<Vault, String> {
        Vault::open(path, VAULT_LABEL, VAULT_AAD, VAULT_LOCK)
    }

    fn test_session(dir: &Path, origin: String) -> Session {
        Session {
            vault: test_vault(dir.join("vault")).unwrap(),
            client: Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap(),
            grant: None,
            device: None,
            dirty: false,
            test_origin: Some(origin),
        }
    }

    fn grant_body(access: &str, refresh: &str) -> Value {
        json!({"success": true, "data": {
            "accessToken": access,
            "refreshToken": refresh,
            "tokenType": "Bearer",
            "expiresAt": "2030-01-01T00:00:00.000Z",
            "userInfo": { "clineUserId": "user-1", "email": "user@example.com", "name": "Cline User" }
        }})
    }

    async fn mock_server(
        responses: Vec<(u16, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<(String, Value)>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut bytes = Vec::new();
                let (header_end, length) = loop {
                    let mut buffer = [0; 4096];
                    let size = socket.read(&mut buffer).await.unwrap();
                    assert!(size > 0);
                    bytes.extend_from_slice(&buffer[..size]);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        break (end + 4, length);
                    }
                };
                while bytes.len() < header_end + length {
                    let mut buffer = [0; 4096];
                    let size = socket.read(&mut buffer).await.unwrap();
                    assert!(size > 0);
                    bytes.extend_from_slice(&buffer[..size]);
                }
                let request = String::from_utf8_lossy(&bytes).to_string();
                let request_line = request.lines().next().unwrap().to_string();
                let raw_body = &request[header_end..];
                let values: Value = if raw_body.trim_start().starts_with('{') {
                    serde_json::from_str(raw_body).unwrap()
                } else {
                    let pairs = reqwest::Url::parse(&format!("http://localhost/?{raw_body}"))
                        .unwrap()
                        .query_pairs()
                        .map(|(key, value)| (key.into_owned(), Value::String(value.into_owned())))
                        .collect();
                    Value::Object(pairs)
                };
                requests.push((request_line, values));
                let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (origin, task)
    }

    #[tokio::test]
    async fn device_flow_registers_cline_tokens_and_refreshes_after_expiry() {
        let (origin, server) = mock_server(vec![
            (200, json!({"device_code":"device-secret", "user_code":"ABCD-EFGH", "verification_uri":"https://app.cline.bot/device", "expires_in":600, "interval":1}).to_string()),
            (404, json!({"error":"authorization_pending"}).to_string()),
            (200, json!({"access_token":"workos-access", "refresh_token":"workos-refresh", "token_type":"Bearer"}).to_string()),
            (200, grant_body("cline-access", "cline-refresh").to_string()),
            (200, grant_body("cline-access-2", "cline-refresh-2").to_string()),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let mut session = test_session(dir.path(), origin);
        let view = session.start().await.unwrap();
        let id = view.session_id.unwrap();
        assert_eq!(view.phase, "pending");
        assert_eq!(view.user_code.as_deref(), Some("ABCD-EFGH"));
        assert!(view
            .verification_uri
            .as_deref()
            .unwrap()
            .starts_with("https://"));
        // First poll happens before the device interval elapses.
        assert_eq!(session.poll(&id).await.unwrap().phase, "pending");
        session.device.as_mut().unwrap().next_poll = 0;
        assert_eq!(session.poll(&id).await.unwrap().phase, "pending");
        session.device.as_mut().unwrap().next_poll = 0;
        assert_eq!(session.poll(&id).await.unwrap().phase, "connected");
        assert_eq!(session.token(false).await.unwrap(), "cline-access");
        session.grant.as_mut().unwrap().expires_at = now();
        assert_eq!(session.token(false).await.unwrap(), "cline-access-2");
        assert_eq!(
            session
                .vault
                .load::<Grant>()
                .unwrap()
                .unwrap()
                .refresh_token,
            "cline-refresh-2"
        );
        let view = serde_json::to_string(&session.view()).unwrap();
        assert!(!view.contains("cline-access-2") && !view.contains("cline-refresh-2"));
        drop(session);
        let mut restored = test_session(dir.path(), "http://127.0.0.1:1".into());
        restored.grant = restored.vault.load::<Grant>().unwrap();
        assert_eq!(restored.token(false).await.unwrap(), "cline-access-2");
        let requests = server.await.unwrap();
        assert!(requests[0]
            .0
            .starts_with("POST /user_management/authorize/device "));
        assert_eq!(requests[0].1["client_id"], WORKOS_CLIENT_ID);
        assert_eq!(requests[1].1["device_code"], "device-secret");
        assert_eq!(
            requests[2].1["grant_type"],
            "urn:ietf:params:oauth:grant-type:device_code"
        );
        assert!(requests[3].0.starts_with("POST /api/v1/auth/register "));
        assert_eq!(requests[3].1["accessToken"], "workos-access");
        assert_eq!(requests[3].1["refreshToken"], "workos-refresh");
        assert_eq!(requests[4].1["grantType"], "refresh_token");
        assert_eq!(requests[4].1["refreshToken"], "cline-refresh");
    }

    #[tokio::test]
    async fn rejected_refresh_requires_reauthorization() {
        let (origin, server) = mock_server(vec![(
            400,
            json!({"error":"invalid_grant", "error_description":"secret-refresh"}).to_string(),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let mut session = test_session(dir.path(), origin);
        session.grant = Some(Grant {
            access_token: "access".into(),
            refresh_token: "secret-refresh".into(),
            expires_at: now(),
            reauth_required: false,
        });
        let error = session.token(true).await.err().unwrap();
        assert!(!error.contains("secret-refresh"));
        assert_eq!(session.view().phase, "reauth_required");
        assert!(
            session
                .vault
                .load::<Grant>()
                .unwrap()
                .unwrap()
                .reauth_required
        );
        assert!(session.token(false).await.is_err());
        assert!(session.logout().await.unwrap().is_none());
        assert!(session.vault.load::<Grant>().unwrap().is_none());
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn accounts_are_isolated_and_invalid_responses_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut accounts = Accounts::open(dir.path().join("oauth")).unwrap();
        let a = accounts.get("card-a").unwrap();
        let b = accounts.get("card-b").unwrap();
        assert_ne!(
            accounts.account_dir("card-a"),
            accounts.account_dir("card-b")
        );
        {
            let mut a = a.lock().await;
            let mut b = b
                .try_lock()
                .expect("another account must not share the lock");
            a.grant = Some(parse_grant(&grant_body("a-access", "a-refresh"), None).unwrap());
            a.persist().unwrap();
            b.grant = Some(parse_grant(&grant_body("b-access", "b-refresh"), None).unwrap());
            b.persist().unwrap();
            assert_eq!(
                a.vault.load::<Grant>().unwrap().unwrap().access_token,
                "a-access"
            );
            assert_eq!(
                b.vault.load::<Grant>().unwrap().unwrap().access_token,
                "b-access"
            );
            a.forget().unwrap();
            assert!(a.vault.load::<Grant>().unwrap().is_none());
            assert_eq!(
                b.vault.load::<Grant>().unwrap().unwrap().access_token,
                "b-access"
            );
        }
        let encrypted = fs::read(accounts.account_dir("card-b").join("account.enc")).unwrap();
        assert!(!String::from_utf8_lossy(&encrypted).contains("b-refresh"));
        let grant = parse_grant(&grant_body("access", "refresh"), None).unwrap();
        assert_eq!(grant.expires_at, 1_893_456_000_000);
        assert!(parse_grant(&json!({"data": {"accessToken": "a"}}), None).is_err());
        assert!(parse_grant(&json!({"success": false}), None).is_err());
        assert!(
            parse_device(&json!({"user_code": "A", "verification_uri": "http://insecure"}))
                .is_err()
        );
        assert!(parse_device(&json!({"device_code": "d", "user_code": "A", "verification_uri": "https://x", "expires_in": 0})).is_err());
    }

    #[tokio::test]
    async fn expired_cancelled_and_stale_sessions_do_not_poll() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = test_session(dir.path(), "http://127.0.0.1:1".into());
        session.device = Some(
            parse_device(&json!({
                "device_code": "device-secret",
                "user_code": "ABCD",
                "verification_uri": "https://app.cline.bot/device",
                "expires_in": 600
            }))
            .unwrap(),
        );
        assert!(session.poll("stale-id").await.is_err());
        let id = session.device.as_ref().unwrap().id.clone();
        session.device.as_mut().unwrap().expires_at = 0;
        assert!(session.poll(&id).await.is_err());
        assert!(session.device.is_none());
        session.cancel();
        assert!(session.poll(&id).await.is_err());
    }
}
