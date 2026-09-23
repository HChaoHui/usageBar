//! Independent MiniMax Device Flow + PKCE. Secrets never cross the IPC boundary.
use crate::oauth_vault::{now, random, Vault};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::{Client, Url};
use ring::digest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::Duration,
};
#[cfg(test)]
use std::{fs, path::Path};
use tokio::sync::{Mutex, OwnedMutexGuard};

const CLIENT_ID: &str = "mcode-public";
const VAULT_LABEL: &str = "MiniMax";
const VAULT_AAD: &[u8] = b"usagebar-minimax-v1";
const VAULT_LOCK: &str = "另一个 usageBar 实例正在使用 MiniMax 登录凭证";
static ACCOUNTS: OnceLock<StdMutex<Accounts>> = OnceLock::new();

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    Cn,
    En,
}

impl Region {
    fn origin(self) -> &'static str {
        match self {
            Self::Cn => "https://account.minimax.cn",
            Self::En => "https://account.minimax.io",
        }
    }
    pub fn is_global(self) -> bool {
        matches!(self, Self::En)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Grant {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    region: Region,
    #[serde(default)]
    reauth_required: bool,
}

struct Device {
    id: String,
    region: Region,
    verifier: String,
    code: String,
    parameter: &'static str,
    user_code: String,
    verification_uri: String,
    expires_at: i64,
    interval_ms: i64,
    next_poll: i64,
}

#[derive(Serialize)]
pub struct AuthView {
    pub phase: &'static str,
    region: Option<Region>,
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
    // Hold the root process lock even though each account has its own vault.
    root: Vault,
    legacy: Option<Grant>,
    client: Client,
    sessions: HashMap<String, Arc<Mutex<Session>>>,
}

impl Accounts {
    fn open(dir: PathBuf, legacy_provider: Option<&str>) -> Result<Self, String> {
        let root = Vault::open(dir, VAULT_LABEL, VAULT_AAD, VAULT_LOCK)?;
        let legacy = root.load::<Grant>()?;
        let client = Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "无法创建 MiniMax OAuth 客户端")?;
        let mut accounts = Self {
            root,
            legacy,
            client,
            sessions: HashMap::new(),
        };
        if let Some(id) = legacy_provider {
            // Complete migration before any provider can refresh the old token.
            if accounts.legacy.is_some() {
                accounts.get(id)?;
            }
        }
        Ok(accounts)
    }

    fn account_dir(&self, id: &str) -> PathBuf {
        self.root.account_dir(id)
    }

    fn get(&mut self, id: &str) -> Result<Arc<Mutex<Session>>, String> {
        if id.trim().is_empty() {
            return Err("MiniMax Provider ID 不能为空".into());
        }
        if let Some(session) = self.sessions.get(id) {
            return Ok(session.clone());
        }
        let vault = Vault::open(self.account_dir(id), VAULT_LABEL, VAULT_AAD, VAULT_LOCK)?;
        let mut grant = vault.load::<Grant>()?;
        if let Some(legacy) = &self.legacy {
            // If interrupted after saving the destination, keep that newer grant.
            if grant.is_none() {
                vault.save(legacy)?;
                grant = Some(legacy.clone());
            }
            self.root.clear()?;
            self.legacy = None;
        }
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

pub fn init(dir: PathBuf, legacy_provider: Option<&str>) -> Result<(), String> {
    ACCOUNTS
        .set(StdMutex::new(Accounts::open(dir, legacy_provider)?))
        .map_err(|_| "MiniMax OAuth 已初始化".into())
}

pub async fn session(id: &str) -> Result<OwnedMutexGuard<Session>, String> {
    let session = ACCOUNTS
        .get()
        .ok_or("MiniMax OAuth 尚未初始化")?
        .lock()
        .map_err(|_| "MiniMax 账号存储不可用")?
        .get(id)?;
    Ok(session.lock_owned().await)
}

async fn form(
    client: &Client,
    origin: &str,
    path: &str,
    values: &[(&str, &str)],
) -> Result<Value, String> {
    let response = client
        .post(format!("{origin}{path}"))
        .header("Accept", "application/json")
        .form(values)
        .send()
        .await
        .map_err(|_| "MiniMax OAuth 网络请求失败，请重试")?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|_| "MiniMax OAuth 响应读取失败")?;
    let body: Value = if text.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&text).map_err(|_| "MiniMax OAuth 返回无效 JSON")?
    };
    if !body.is_object() {
        return Err("MiniMax OAuth 返回无效 JSON 对象".into());
    }
    if let Some(error) = body.get("error") {
        return Err(match error.as_str().unwrap_or("") {
            "authorization_pending" => "authorization_pending",
            "slow_down" => "slow_down",
            "access_denied" => "access_denied",
            "expired_token" => "expired_token",
            "invalid_grant" => "invalid_grant",
            "invalid_token" => "invalid_token",
            _ => "MiniMax OAuth 授权请求失败",
        }
        .into());
    }
    if !status.is_success() {
        return Err(format!("MiniMax OAuth HTTP {}", status.as_u16()));
    }
    for code in [
        body.pointer("/base_resp/status_code"),
        body.pointer("/statusInfo/code"),
        body.pointer("/status_info/code"),
    ]
    .into_iter()
    .flatten()
    {
        if code.as_i64().unwrap_or(-1) != 0 {
            return Err("MiniMax OAuth 返回业务错误".into());
        }
    }
    Ok(body)
}

fn string(body: &Value, key: &str) -> Result<String, String> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("MiniMax OAuth 响应缺少 {key}"))
}

fn parse_grant(body: &Value, previous: Option<&str>, region: Region) -> Result<Grant, String> {
    let access_token = string(body, "access_token")?;
    let claims = access_token
        .split('.')
        .nth(1)
        .and_then(|part| URL_SAFE_NO_PAD.decode(part).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    let scope = body
        .get("scope")
        .or_else(|| claims.get("scope"))
        .or_else(|| claims.get("scp"));
    let valid_scope = scope.is_some_and(|value| {
        value
            .as_str()
            .is_some_and(|s| s.split_whitespace().any(|s| s == "agent.default"))
            || value
                .as_array()
                .is_some_and(|items| items.iter().any(|v| v.as_str() == Some("agent.default")))
    });
    let seconds = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0 && *n < 315_360_000);
    if !valid_scope
        || !body
            .get("token_type")
            .and_then(Value::as_str)
            .is_some_and(|s| s.eq_ignore_ascii_case("bearer"))
        || seconds.is_none()
    {
        return Err("MiniMax OAuth Token 响应无效".into());
    }
    let refresh_token = match body.get("refresh_token") {
        Some(_) => string(body, "refresh_token")?,
        None => previous
            .filter(|s| !s.is_empty())
            .ok_or("MiniMax OAuth 缺少 Refresh Token")?
            .into(),
    };
    Ok(Grant {
        access_token,
        refresh_token,
        expires_at: now() + seconds.unwrap() * 1000,
        region,
        reauth_required: false,
    })
}

fn parse_device(body: &Value, region: Region, verifier: String) -> Result<Device, String> {
    let user_code = string(body, "user_code")?;
    let legacy_expiry = body
        .get("expired_in")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
        .unwrap_or(0);
    let alternate = body
        .get("device_code")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .is_none()
        && legacy_expiry > 0;
    let seconds = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| {
            if legacy_expiry >= 1_000_000_000_000 {
                (legacy_expiry - now()) / 1000
            } else {
                legacy_expiry
            }
        });
    let uri = body
        .get("verification_uri_complete")
        .or_else(|| body.get("verification_uri"))
        .or_else(|| body.get("verification_url"))
        .and_then(Value::as_str)
        .ok_or("MiniMax OAuth 缺少授权地址")?;
    let url = Url::parse(uri).map_err(|_| "MiniMax OAuth 授权地址无效")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || !(1..=86400).contains(&seconds)
    {
        return Err("MiniMax OAuth 授权响应无效".into());
    }
    let interval = body
        .get("interval")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .map(|n| n.saturating_mul(if alternate { 1 } else { 1000 }))
        .unwrap_or(5000)
        .clamp(1000, 86400000);
    Ok(Device {
        id: uuid::Uuid::new_v4().to_string(),
        region,
        verifier,
        code: if alternate {
            user_code.clone()
        } else {
            string(body, "device_code")?
        },
        parameter: if alternate {
            "user_code"
        } else {
            "device_code"
        },
        user_code,
        verification_uri: url.to_string(),
        expires_at: now() + seconds * 1000,
        interval_ms: interval,
        next_poll: now() + interval,
    })
}

impl Session {
    fn origin(&self, region: Region) -> &str {
        #[cfg(test)]
        if let Some(origin) = &self.test_origin {
            return origin;
        }
        region.origin()
    }
    pub fn view(&self) -> AuthView {
        if let Some(device) = &self.device {
            return AuthView {
                phase: "pending",
                region: Some(device.region),
                session_id: Some(device.id.clone()),
                user_code: Some(device.user_code.clone()),
                verification_uri: Some(device.verification_uri.clone()),
                expires_at: Some(device.expires_at),
            };
        }
        AuthView {
            phase: match &self.grant {
                Some(g) if g.reauth_required => "reauth_required",
                Some(_) => "connected",
                None => "idle",
            },
            region: self.grant.as_ref().map(|g| g.region),
            session_id: None,
            user_code: None,
            verification_uri: None,
            expires_at: self.grant.as_ref().map(|g| g.expires_at),
        }
    }
    fn persist(&mut self) -> Result<(), String> {
        if let Some(grant) = &self.grant {
            self.vault.save(grant)?;
        }
        self.dirty = false;
        Ok(())
    }
    pub async fn token(&mut self, force: bool) -> Result<(String, Region), String> {
        if self.dirty {
            self.persist()?;
        }
        let grant = self.grant.as_ref().ok_or("请在设置中登录 MiniMax 账号")?;
        if grant.reauth_required {
            return Err("MiniMax 授权已失效，请在设置中退出后重新登录".into());
        }
        if force || grant.expires_at <= now() + 60_000 {
            let response = form(
                &self.client,
                self.origin(grant.region),
                "/oauth2/token",
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &grant.refresh_token),
                    ("client_id", CLIENT_ID),
                    ("scope", "agent.default"),
                    ("audience", "agent-backend"),
                ],
            )
            .await;
            match response {
                Ok(body) => {
                    let renewed = parse_grant(&body, Some(&grant.refresh_token), grant.region)?;
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
                        return Err("MiniMax 授权已失效，请在设置中退出后重新登录".into());
                    }
                    return Err(error);
                }
            }
        }
        let grant = self.grant.as_ref().unwrap();
        Ok((grant.access_token.clone(), grant.region))
    }
    pub async fn start(&mut self, region: Region) -> Result<AuthView, String> {
        if self.grant.is_some() {
            return Err("请先退出当前 MiniMax 账号".into());
        }
        self.device = None;
        let verifier = URL_SAFE_NO_PAD.encode(random::<32>()?);
        let challenge =
            URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
        let body = form(
            &self.client,
            self.origin(region),
            "/oauth2/device/code",
            &[
                ("client_id", CLIENT_ID),
                ("scope", "agent.default"),
                ("audience", "agent-backend"),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ],
        )
        .await?;
        self.device = Some(parse_device(&body, region, verifier)?);
        Ok(self.view())
    }
    pub async fn poll(&mut self, id: &str) -> Result<AuthView, String> {
        let region = self.device.as_ref().ok_or("授权会话已结束")?.region;
        let origin = self.origin(region).to_owned();
        let device = self
            .device
            .as_mut()
            .filter(|d| d.id == id)
            .ok_or("授权会话已结束")?;
        if now() >= device.expires_at {
            self.device = None;
            return Err("授权码已过期，请重新登录".into());
        }
        if now() < device.next_poll {
            return Ok(self.view());
        }
        device.next_poll = now() + device.interval_ms;
        let result = form(
            &self.client,
            &origin,
            "/oauth2/token",
            &[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                (device.parameter, &device.code),
                ("client_id", CLIENT_ID),
                ("code_verifier", &device.verifier),
            ],
        )
        .await;
        let result = result.and_then(|body| match body.get("status").and_then(Value::as_str) {
            Some("pending") => Err("authorization_pending".into()),
            Some("slow_down") => Err("slow_down".into()),
            Some("denied" | "access_denied") => Err("access_denied".into()),
            Some("expired" | "expired_token") => Err("expired_token".into()),
            _ => Ok(body),
        });
        match result {
            Ok(body) => {
                let grant = parse_grant(&body, None, device.region);
                self.device = None;
                self.grant = Some(grant?);
                self.dirty = true;
                self.persist()?;
            }
            Err(error) if error == "authorization_pending" => {}
            Err(error) if error == "slow_down" => {
                device.interval_ms += 5000;
                device.next_poll = now() + device.interval_ms;
            }
            Err(error) => {
                self.device = None;
                return Err(error);
            }
        }
        Ok(self.view())
    }
    pub fn cancel(&mut self) {
        self.device = None;
    }
    pub fn verification_uri(&self) -> Result<&str, String> {
        self.device
            .as_ref()
            .filter(|d| d.expires_at > now())
            .map(|d| d.verification_uri.as_str())
            .ok_or("没有待完成的授权".into())
    }
    pub async fn logout(&mut self) -> Result<Option<String>, String> {
        self.device = None;
        let warning = if let Some(grant) = &self.grant {
            form(
                &self.client,
                self.origin(grant.region),
                "/oauth2/revoke",
                &[
                    ("token", &grant.refresh_token),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", CLIENT_ID),
                ],
            )
            .await
            .err()
        } else {
            None
        };
        self.forget()?;
        Ok(warning.map(|_| "本地凭证已清除，但官方撤销请求失败".into()))
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
    use serde_json::json;

    async fn mock_server(
        responses: Vec<(u16, String)>,
    ) -> (String, tokio::task::JoinHandle<Vec<(String, Value)>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
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
                let request = String::from_utf8(bytes).unwrap();
                let request_line = request.lines().next().unwrap().to_string();
                let form_url =
                    Url::parse(&format!("http://localhost/?{}", &request[header_end..])).unwrap();
                let values: serde_json::Map<String, Value> = form_url
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
                    .collect();
                requests.push((request_line, Value::Object(values)));
                let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (origin, task)
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
        json!({"access_token":access, "refresh_token":refresh, "expires_in":3600, "token_type":"Bearer", "scope":"agent.default"})
    }

    fn test_vault(path: PathBuf) -> Result<Vault, String> {
        Vault::open(path, VAULT_LABEL, VAULT_AAD, VAULT_LOCK)
    }

    #[tokio::test]
    async fn legacy_grant_moves_to_one_card_and_accounts_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth");
        let legacy = test_vault(path.clone()).unwrap();
        legacy
            .save(
                &parse_grant(
                    &grant_body("legacy-access", "legacy-refresh"),
                    None,
                    Region::Cn,
                )
                .unwrap(),
            )
            .unwrap();
        drop(legacy);
        {
            let mut accounts = Accounts::open(path.clone(), Some("first-card")).unwrap();
            assert!(!path.join("account.enc").exists());
            let first = accounts.get("first-card").unwrap();
            assert_eq!(
                first.lock().await.token(false).await.unwrap().0,
                "legacy-access"
            );
            let second = accounts.get("second-card").unwrap();
            let mut second = second.lock().await;
            assert_eq!(second.view().phase, "idle");
            second.grant = Some(
                parse_grant(
                    &grant_body("second-access", "second-refresh"),
                    None,
                    Region::En,
                )
                .unwrap(),
            );
            second.persist().unwrap();
            assert_ne!(
                accounts.account_dir("first-card"),
                accounts.account_dir("second-card")
            );
            let first_bytes =
                fs::read(accounts.account_dir("first-card").join("account.enc")).unwrap();
            assert!(!String::from_utf8_lossy(&first_bytes).contains("legacy-refresh"));
        }
        // Reordering the cards on the next launch must not reassign the old grant.
        let mut accounts = Accounts::open(path, Some("second-card")).unwrap();
        assert_eq!(
            accounts
                .get("first-card")
                .unwrap()
                .lock()
                .await
                .token(false)
                .await
                .unwrap()
                .0,
            "legacy-access"
        );
        let second = accounts.get("second-card").unwrap();
        let mut second = second.lock().await;
        let (token, region) = second.token(false).await.unwrap();
        assert_eq!(token, "second-access");
        assert!(region.is_global());
    }

    #[tokio::test]
    async fn interrupted_migration_keeps_destination_and_no_card_legacy_is_claimed_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth");
        {
            let mut accounts = Accounts::open(path.clone(), None).unwrap();
            accounts
                .root
                .save(&parse_grant(&grant_body("old", "old-refresh"), None, Region::Cn).unwrap())
                .unwrap();
            let destination = test_vault(accounts.account_dir("first")).unwrap();
            destination
                .save(
                    &parse_grant(&grant_body("newer", "rotated-refresh"), None, Region::Cn)
                        .unwrap(),
                )
                .unwrap();
            accounts.sessions.clear();
        }
        {
            let mut accounts = Accounts::open(path.clone(), Some("first")).unwrap();
            assert_eq!(
                accounts
                    .get("first")
                    .unwrap()
                    .lock()
                    .await
                    .token(false)
                    .await
                    .unwrap()
                    .0,
                "newer"
            );
            assert_eq!(
                accounts.get("second").unwrap().lock().await.view().phase,
                "idle"
            );
            assert!(!path.join("account.enc").exists());
        }
        let path = dir.path().join("without-cards");
        {
            let legacy = test_vault(path.clone()).unwrap();
            legacy
                .save(&parse_grant(&grant_body("legacy", "refresh"), None, Region::Cn).unwrap())
                .unwrap();
        }
        let mut accounts = Accounts::open(path, None).unwrap();
        assert_eq!(
            accounts
                .get("new-first")
                .unwrap()
                .lock()
                .await
                .token(false)
                .await
                .unwrap()
                .0,
            "legacy"
        );
        assert_eq!(
            accounts
                .get("new-second")
                .unwrap()
                .lock()
                .await
                .view()
                .phase,
            "idle"
        );
    }

    #[tokio::test]
    async fn account_refresh_logout_and_locks_are_isolated() {
        let (origin, server) = mock_server(vec![
            (200, grant_body("a-new", "a-rotated").to_string()),
            (204, String::new()),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let mut accounts = Accounts::open(dir.path().join("oauth"), None).unwrap();
        let a = accounts.get("a").unwrap();
        let b = accounts.get("b").unwrap();
        {
            let mut a = a.lock().await;
            a.test_origin = Some(origin);
            a.grant =
                Some(parse_grant(&grant_body("a-old", "a-refresh"), None, Region::Cn).unwrap());
            a.persist().unwrap();
            let mut b = b
                .try_lock()
                .expect("another account must not share the lock");
            b.grant =
                Some(parse_grant(&grant_body("b-access", "b-refresh"), None, Region::En).unwrap());
            b.persist().unwrap();
            assert_eq!(a.token(true).await.unwrap().0, "a-new");
            assert_eq!(
                b.vault.load::<Grant>().unwrap().unwrap().refresh_token,
                "b-refresh"
            );
            assert!(a.logout().await.unwrap().is_none());
            assert!(a.vault.load::<Grant>().unwrap().is_none());
            assert_eq!(b.token(false).await.unwrap().0, "b-access");
            assert_eq!(
                b.vault.load::<Grant>().unwrap().unwrap().refresh_token,
                "b-refresh"
            );
        }
        let requests = server.await.unwrap();
        assert_eq!(requests[0].1["refresh_token"], "a-refresh");
        assert_eq!(requests[1].1["token"], "a-rotated");
        assert!(!serde_json::to_string(&requests)
            .unwrap()
            .contains("b-refresh"));
    }

    #[tokio::test]
    async fn pending_authorizations_and_forget_are_scoped_to_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let mut accounts = Accounts::open(dir.path().join("oauth"), None).unwrap();
        let a = accounts.get("../账号 A").unwrap();
        let b = accounts.get("../账号 B").unwrap();
        assert_eq!(
            accounts.account_dir("../账号 A").parent().unwrap(),
            accounts.root.dir.join("accounts")
        );
        let mut a = a.lock().await;
        let mut b = b.lock().await;
        let device = json!({"device_code":"device-secret", "user_code":"CODE", "verification_uri":"https://account.minimax.cn/authorize", "expires_in":600});
        a.device = Some(parse_device(&device, Region::Cn, "verifier-a".into()).unwrap());
        b.device = Some(parse_device(&device, Region::Cn, "verifier-b".into()).unwrap());
        let a_id = a.device.as_ref().unwrap().id.clone();
        let b_id = b.device.as_ref().unwrap().id.clone();
        assert!(b.poll(&a_id).await.is_err());
        assert_eq!(b.device.as_ref().unwrap().id, b_id);
        a.cancel();
        assert_eq!(a.view().phase, "idle");
        assert_eq!(b.view().phase, "pending");
        a.grant = Some(parse_grant(&grant_body("a", "a-refresh"), None, Region::Cn).unwrap());
        a.persist().unwrap();
        a.forget().unwrap();
        assert!(a.vault.load::<Grant>().unwrap().is_none());
        assert_eq!(b.view().phase, "pending");
        assert_eq!(b.device.as_ref().unwrap().verifier, "verifier-b");
    }

    #[tokio::test]
    async fn device_flow_pkce_polling_refresh_restart_and_revoke() {
        let (origin, server) = mock_server(vec![
            (200, json!({"device_code":"device-secret", "user_code":"ABCD", "verification_uri":"https://account.minimax.cn/authorize", "expires_in":600, "interval":5}).to_string()),
            (400, json!({"error":"authorization_pending"}).to_string()),
            (200, json!({"status":"slow_down"}).to_string()),
            (200, grant_body("access-1", "refresh-1").to_string()),
            (200, grant_body("access-2", "refresh-2").to_string()),
            (204, String::new()),
        ]).await;
        let dir = tempfile::tempdir().unwrap();
        let mut session = test_session(dir.path(), origin.clone());
        let view = session.start(Region::Cn).await.unwrap();
        let id = view.session_id.unwrap();
        // Polling too early must not hit the upstream endpoint.
        assert_eq!(session.poll(&id).await.unwrap().phase, "pending");
        let verifier = session.device.as_ref().unwrap().verifier.clone();
        for _ in 0..2 {
            session.device.as_mut().unwrap().next_poll = 0;
            assert_eq!(session.poll(&id).await.unwrap().phase, "pending");
        }
        assert_eq!(session.device.as_ref().unwrap().interval_ms, 10000);
        session.device.as_mut().unwrap().next_poll = 0;
        assert_eq!(session.poll(&id).await.unwrap().phase, "connected");
        assert_eq!(session.token(false).await.unwrap().0, "access-1");
        session.grant.as_mut().unwrap().expires_at = now();
        assert_eq!(session.token(false).await.unwrap().0, "access-2");
        assert_eq!(
            session
                .vault
                .load::<Grant>()
                .unwrap()
                .unwrap()
                .refresh_token,
            "refresh-2"
        );
        let view = serde_json::to_string(&session.view()).unwrap();
        assert!(!view.contains("access-2") && !view.contains("refresh-2"));
        drop(session);
        let mut restored = test_session(dir.path(), origin);
        restored.grant = restored.vault.load::<Grant>().unwrap();
        assert_eq!(restored.token(false).await.unwrap().0, "access-2");
        assert!(restored.logout().await.unwrap().is_none());
        assert_eq!(restored.view().phase, "idle");
        assert!(restored.vault.load::<Grant>().unwrap().is_none());
        let requests = server.await.unwrap();
        assert!(requests[0].0.starts_with("POST /oauth2/device/code "));
        assert_eq!(requests[0].1["code_challenge_method"], "S256");
        assert_eq!(
            requests[0].1["code_challenge"],
            URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()))
        );
        assert_eq!(requests[1].1["device_code"], "device-secret");
        assert_eq!(requests[1].1["code_verifier"], verifier);
        assert_eq!(requests[4].1["grant_type"], "refresh_token");
        assert_eq!(requests[4].1["refresh_token"], "refresh-1");
        assert!(requests[5].0.starts_with("POST /oauth2/revoke "));
        assert_eq!(requests[5].1["token"], "refresh-2");
    }

    #[tokio::test]
    async fn rejected_refresh_requires_reauthorization_and_failed_revoke_clears_local_grant() {
        let (origin, server) = mock_server(vec![
            (
                400,
                json!({"error":"invalid_grant", "error_description":"secret-refresh"}).to_string(),
            ),
            (503, json!({"error":"upstream_unavailable"}).to_string()),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let mut session = test_session(dir.path(), origin);
        session.grant =
            Some(parse_grant(&grant_body("access", "secret-refresh"), None, Region::Cn).unwrap());
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
        assert!(session.logout().await.unwrap().is_some());
        assert!(session.vault.load::<Grant>().unwrap().is_none());
        assert_eq!(server.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn expired_cancelled_and_stale_sessions_do_not_poll() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = test_session(dir.path(), "http://127.0.0.1:1".into());
        session.device = Some(parse_device(&json!({"user_code":"ABCD", "verification_url":"https://account.minimax.cn/authorize", "expired_in":600}), Region::Cn, "verifier".into()).unwrap());
        assert!(session.poll("stale-id").await.is_err());
        let id = session.device.as_ref().unwrap().id.clone();
        session.device.as_mut().unwrap().expires_at = 0;
        assert!(session.poll(&id).await.is_err());
        assert!(session.device.is_none());
        session.cancel();
        assert!(session.poll(&id).await.is_err());
    }

    #[test]
    fn handles_both_device_protocols_and_hides_secrets() {
        let standard = parse_device(&json!({"device_code":"secret-device", "user_code":"ABCD", "verification_uri":"https://account.minimax.cn/authorize", "expires_in":600, "interval":5}), Region::Cn, "secret-verifier".into()).unwrap();
        assert_eq!(standard.parameter, "device_code");
        assert_eq!(standard.interval_ms, 5000);
        let legacy = parse_device(&json!({"user_code":"ABCD", "verification_url":"https://account.minimax.cn/authorize", "expired_in":now()+600_000, "interval":2000}), Region::Cn, "secret-verifier".into()).unwrap();
        assert_eq!(legacy.parameter, "user_code");
        assert_eq!(legacy.interval_ms, 2000);
        let dir = tempfile::tempdir().unwrap();
        let session = Session {
            vault: test_vault(dir.path().join("vault")).unwrap(),
            client: Client::new(),
            grant: None,
            device: Some(standard),
            dirty: false,
            test_origin: None,
        };
        let view = serde_json::to_string(&session.view()).unwrap();
        assert!(!view.contains("secret-device"));
        assert!(!view.contains("secret-verifier"));
    }

    #[test]
    fn refresh_rotation_and_scope_validation() {
        let mut body = json!({"access_token":"access", "refresh_token":"rotated", "expires_in":3600, "token_type":"Bearer", "scope":"agent.default"});
        assert_eq!(
            parse_grant(&body, Some("old"), Region::Cn)
                .unwrap()
                .refresh_token,
            "rotated"
        );
        body.as_object_mut().unwrap().remove("refresh_token");
        assert_eq!(
            parse_grant(&body, Some("old"), Region::Cn)
                .unwrap()
                .refresh_token,
            "old"
        );
        assert!(parse_grant(&body, None, Region::Cn).is_err());
        body["scope"] = json!("other");
        assert!(parse_grant(&body, Some("old"), Region::Cn).is_err());
    }

    #[test]
    fn vault_roundtrip_rotation_lock_and_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault");
        let vault = test_vault(path.clone()).unwrap();
        assert!(test_vault(path.clone()).is_err());
        let mut grant = Grant {
            access_token: "secret-access".into(),
            refresh_token: "secret-refresh".into(),
            expires_at: now() + 3600000,
            region: Region::En,
            reauth_required: false,
        };
        vault.save(&grant).unwrap();
        let encrypted = fs::read(path.join("account.enc")).unwrap();
        assert!(!String::from_utf8_lossy(&encrypted).contains("secret"));
        grant.refresh_token = "rotated".into();
        vault.save(&grant).unwrap();
        drop(vault);
        let vault = test_vault(path.clone()).unwrap();
        assert_eq!(
            vault.load::<Grant>().unwrap().unwrap().refresh_token,
            "rotated"
        );
        fs::write(path.join("account.enc"), b"corrupt").unwrap();
        assert!(vault.load::<Grant>().is_err());
        vault.clear().unwrap();
        assert!(vault.load::<Grant>().unwrap().is_none());
    }
}
