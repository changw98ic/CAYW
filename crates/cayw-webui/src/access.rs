//! Local browser access has no login. Explicit LAN access uses a password
//! account, when configured, and ephemeral HttpOnly sessions. Runtime IPC authentication is separate.
use super::{authorized, error_response, one_header};
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use axum::{
    Json,
    body::to_bytes,
    extract::Request,
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

/// Validated single operator account. Only a salted Argon2id digest is stored.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanAccount {
    username: String,
    password_hash: String,
}
impl LanAccount {
    pub fn from_password(username: &str, password: &str) -> io::Result<Self> {
        if username.is_empty() || username.len() > 80 || username.chars().any(char::is_control) {
            return Err(io::Error::other(
                "Username must contain 1..80 bytes without control characters",
            ));
        }
        if !(8..=1024).contains(&password.len()) {
            return Err(io::Error::other("Password must contain 8..1024 bytes"));
        }
        let salt = SaltString::generate(&mut OsRng);
        let password_hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|_| io::Error::other("Password hashing failed"))?
            .to_string();
        Ok(Self {
            username: username.into(),
            password_hash,
        })
    }
    pub fn read(path: &Path) -> io::Result<Self> {
        let before = fs::symlink_metadata(path)?;
        if !before.is_file() || before.mode() & 0o777 != 0o600 || before.nlink() != 1 {
            return Err(io::Error::other(
                "Account must be a private regular file (0600), without links",
            ));
        }
        let file = fs::File::open(path)?;
        let after = file.metadata()?;
        if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
            return Err(io::Error::other("Account file changed while opening"));
        }
        let mut bytes = Vec::new();
        file.take(16385).read_to_end(&mut bytes)?;
        if bytes.len() > 16384 {
            return Err(io::Error::other("Account file exceeds size limit"));
        }
        let account: Self =
            serde_json::from_slice(&bytes).map_err(|_| io::Error::other("Invalid account file"))?;
        if account.username.is_empty()
            || account.username.len() > 80
            || account.username.chars().any(char::is_control)
            || !PasswordHash::new(&account.password_hash)
                .is_ok_and(|hash| hash.algorithm.as_str() == "argon2id")
        {
            return Err(io::Error::other("Invalid account file"));
        }
        Ok(account)
    }
    /// Account creation is explicit and atomic; existing credentials are kept.
    pub fn create(&self, path: &Path) -> io::Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&serde_json::to_vec_pretty(self)?)?;
        temporary.as_file().sync_all()?;
        temporary.persist_noclobber(path).map_err(|e| e.error)?;
        fs::File::open(parent)?.sync_all()
    }
    fn revision(&self) -> String {
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self).expect("account serialization"))
        )
    }
    fn optional(path: &Path) -> io::Result<Option<Self>> {
        match Self::read(path) {
            Ok(account) => Ok(Some(account)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
    // Keep the file comparison and atomic replacement under one host file lock.
    // A directory-sync failure is reported separately because replacement occurred.
    fn replace(
        &self,
        path: &Path,
        expected: Option<&str>,
        apply: impl FnOnce(),
    ) -> io::Result<Option<io::Error>> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path.with_extension("account-lock"))?;
        lock.lock()?;
        let current = Self::optional(path)?;
        if current.as_ref().map(Self::revision).as_deref() != expected {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "账号状态已改变，请刷新后再保存；未覆盖当前账号",
            ));
        }
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&serde_json::to_vec_pretty(self)?)?;
        temporary.as_file().sync_all()?;
        if current.is_some() {
            temporary.persist(path).map_err(|e| e.error)?;
        } else {
            temporary.persist_noclobber(path).map_err(|e| e.error)?;
        }
        apply();
        Ok(fs::File::open(parent)
            .and_then(|file| file.sync_all())
            .err())
    }
    fn verify(&self, username: &str, password: &str) -> bool {
        // Verify the hash for unknown users too; errors never echo submitted data.
        let valid = PasswordHash::new(&self.password_hash).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        });
        valid && self.username == username
    }
}

pub enum WebAccess {
    Local,
    Lan(Option<LanAccount>),
    /// Compatibility for existing explicit Rust gateway callers; never selected
    /// by the CLI and never permitted on a non-loopback listener.
    #[doc(hidden)]
    LegacyBearer(String),
}
struct Session {
    authority: String,
    expires: Instant,
}
struct State {
    account: Option<LanAccount>,
    sessions: HashMap<String, Session>,
    window: Instant,
    attempts: usize,
}
enum Mode {
    Local,
    Lan,
    LegacyBearer(String),
}
pub(super) struct BrowserAccess {
    mode: Mode,
    account_path: Option<PathBuf>,
    cookie: String,
    state: Arc<Mutex<State>>,
    saving: Arc<Semaphore>,
    hashing: Arc<Semaphore>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    username: String,
    password: String,
}
impl BrowserAccess {
    pub fn new(mode: WebAccess, port: u16, account_path: Option<PathBuf>) -> Self {
        let (mode, account) = match mode {
            WebAccess::Local => (Mode::Local, None),
            WebAccess::Lan(account) => (Mode::Lan, account),
            WebAccess::LegacyBearer(token) => (Mode::LegacyBearer(token), None),
        };
        Self {
            mode,
            account_path,
            cookie: format!("cayw_session_{port}"),
            state: Arc::new(Mutex::new(State {
                account,
                sessions: HashMap::new(),
                window: Instant::now(),
                attempts: 0,
            })),
            saving: Arc::new(Semaphore::new(1)),
            hashing: Arc::new(Semaphore::new(2)),
        }
    }
    fn cookie<'a>(&self, headers: &'a HeaderMap) -> Option<&'a str> {
        let mut found = None;
        for header in headers.get_all(header::COOKIE) {
            for part in header.to_str().ok()?.split(';') {
                let (name, value) = part.trim().split_once('=')?;
                if name == self.cookie {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(value);
                }
            }
        }
        found
    }
    pub fn authenticated(&self, headers: &HeaderMap, manager: bool) -> bool {
        if manager {
            return true;
        }
        match &self.mode {
            Mode::Local => true,
            Mode::LegacyBearer(token) => authorized(headers, token),
            Mode::Lan => {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.account.is_none()
                    || self.cookie(headers).is_some_and(|value| {
                        state.sessions.get(value).is_some_and(|grant| {
                            grant.expires > Instant::now()
                                && Some(grant.authority.as_str()) == one_header(headers, "host")
                        })
                    })
            }
        }
    }
    fn account_status(&self, manager: bool) -> io::Result<serde_json::Value> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let active = state.account.as_ref().map(LanAccount::revision);
        let lan_active = matches!(self.mode, Mode::Lan);
        if !manager {
            return Ok(
                json!({"can_manage":false,"configured":active.is_some(),"lan_active":lan_active,"login_required":lan_active&&active.is_some(),"message":"账号仅能在运行 CAYW 的电脑上设置。请在该电脑打开启动终端显示的本机管理地址。"}),
            );
        }
        let Some(path) = &self.account_path else {
            return Ok(
                json!({"can_manage":false,"configured":active.is_some(),"lan_active":lan_active,"message":"此网关未配置账号保存位置，请通过统一启动器打开网页。"}),
            );
        };
        let account = LanAccount::optional(path)?;
        let revision = account.as_ref().map(LanAccount::revision);
        let applied = lan_active && active == revision;
        let message = if lan_active && applied && account.is_some() {
            "已配置，局域网账号登录已启用；本机管理入口仍免登录。"
        } else if account.is_some() && lan_active {
            "账号文件已配置，当前实例尚未应用。通过此页重新保存立即生效，或重启网页服务。"
        } else if account.is_some() {
            "已配置，下次启动局域网模式时启用登录；本地访问仍免登录。"
        } else {
            "未配置，局域网访问直接放行；账号配置可选，本地访问始终免登录。"
        };
        Ok(
            json!({"can_manage":true,"configured":account.is_some(),"username":account.as_ref().map(|account|account.username.as_str()),"revision":revision,"lan_active":lan_active,"login_required":lan_active&&active.is_some(),"message":message}),
        )
    }
    async fn settings(&self, request: &mut Request, manager: bool) -> Response {
        if request.method() == Method::GET {
            return match self.account_status(manager) {
                Ok(status) => Json(status).into_response(),
                Err(error) => error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "account_read_failed",
                    &format!("无法读取账号状态：{error}"),
                    false,
                ),
            };
        }
        if !manager || self.account_path.is_none() {
            return error_response(
                StatusCode::FORBIDDEN,
                "forbidden",
                "只有本机管理入口可以保存账号",
                false,
            );
        }
        if request.method() != Method::POST
            || one_header(request.headers(), "content-type") != Some("application/json")
        {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "请提交账号设置 JSON",
                false,
            );
        }
        let body = std::mem::replace(request.body_mut(), axum::body::Body::empty());
        let Ok(Ok(bytes)) =
            tokio::time::timeout(Duration::from_secs(5), to_bytes(body, 8192)).await
        else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "账号设置请求无效或超时，未保存",
                false,
            );
        };
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Save {
            username: String,
            password: String,
            confirm_password: String,
            expected_revision: Option<String>,
        }
        let Ok(save) = serde_json::from_slice::<Save>(&bytes) else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "账号设置格式无效，未保存",
                false,
            );
        };
        if save.password != save.confirm_password {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_params",
                "两次密码不一致，未保存",
                false,
            );
        }
        if save
            .expected_revision
            .as_ref()
            .is_some_and(|r| r.len() != 64 || !r.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_params",
                "账号状态版本无效，请刷新",
                false,
            );
        }
        let Ok(permit) = self.hashing.clone().try_acquire_owned() else {
            return error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "account_busy",
                "账号保存处理中，请稍后重试",
                false,
            );
        };
        let Ok(saving) = self.saving.clone().try_acquire_owned() else {
            return error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "account_busy",
                "账号保存处理中，请稍后刷新状态",
                false,
            );
        };
        let path = self.account_path.clone().expect("account path");
        let state = self.state.clone();
        let lan = matches!(self.mode, Mode::Lan);
        let saved = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _saving = saving;
            let account = LanAccount::from_password(&save.username, &save.password)
                .map_err(|error| (StatusCode::BAD_REQUEST, "invalid_params", error))?;
            let sync_error = account
                .replace(&path, save.expected_revision.as_deref(), || {
                    if lan {
                        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                        state.account = Some(account.clone());
                        state.sessions.clear();
                    }
                })
                .map_err(|error| {
                    (
                        if error.kind() == io::ErrorKind::AlreadyExists {
                            StatusCode::CONFLICT
                        } else {
                            StatusCode::INTERNAL_SERVER_ERROR
                        },
                        if error.kind() == io::ErrorKind::AlreadyExists {
                            "account_conflict"
                        } else {
                            "account_save_failed"
                        },
                        error,
                    )
                })?;
            Ok::<_, (StatusCode, &str, io::Error)>((account, sync_error))
        })
        .await;
        match saved {
            Ok(Ok((_account, sync_error))) => {
                if let Some(error) = sync_error {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "account_saved_unconfirmed",
                        &format!(
                            "账号文件已替换，但持久化确认失败：{error}。请刷新状态核对，不会自动重试。"
                        ),
                        true,
                    );
                }
                match self.account_status(true) {
                    Ok(status) => Json(status).into_response(),
                    Err(error) => error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "account_saved_unconfirmed",
                        &format!("保存已执行，状态读取失败：{error}。请刷新核对。"),
                        true,
                    ),
                }
            }
            Ok(Err((status, code, error))) => {
                error_response(status, code, &format!("{error}"), false)
            }
            Err(_) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "account_save_unconfirmed",
                "账号保存回执未确认，请刷新状态核对，不会自动重试",
                true,
            ),
        }
    }
    pub async fn handle(&self, request: &mut Request, manager: bool) -> Option<Response> {
        let path = request.uri().path();
        if path == "/api/settings/account" {
            return Some(self.settings(request, manager).await);
        }
        if path == "/api/auth/status" && request.method() == Method::GET {
            let lan_active = matches!(self.mode, Mode::Lan);
            let lan_login_enabled = lan_active
                && self
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .account
                    .is_some();
            let required = lan_login_enabled && !manager;
            return Some(Json(json!({"mode":if matches!(self.mode,Mode::Lan)&&!manager{"lan"}else{"local"},"authenticated":self.authenticated(request.headers(),manager),"login_required":required,"lan_active":lan_active,"lan_login_enabled":lan_login_enabled})).into_response());
        }
        if path == "/api/auth/logout" && request.method() == Method::POST {
            if let Some(value) = self.cookie(request.headers()) {
                self.state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .sessions
                    .remove(value);
            }
            let mut response = StatusCode::NO_CONTENT.into_response();
            response.headers_mut().insert(
                header::SET_COOKIE,
                format!(
                    "{}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict",
                    self.cookie
                )
                .parse()
                .expect("cookie"),
            );
            return Some(response);
        }
        if path != "/api/auth/login" {
            return None;
        }
        let reject = || {
            error_response(
                StatusCode::UNAUTHORIZED,
                "forbidden",
                "账号或密码错误",
                false,
            )
        };
        let account = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .account
            .clone();
        let Some(account) = account.filter(|_| matches!(self.mode, Mode::Lan)) else {
            return Some(error_response(
                StatusCode::NOT_FOUND,
                "not_found",
                "No account is configured; login is not required",
                false,
            ));
        };
        if request.method() != Method::POST
            || one_header(request.headers(), "content-type") != Some("application/json")
        {
            return Some(reject());
        }
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.window.elapsed() >= Duration::from_secs(30) {
                state.window = Instant::now();
                state.attempts = 0;
            }
            if state.attempts >= 8 {
                return Some(error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    "login_limited",
                    "登录尝试过多，请稍后重试",
                    false,
                ));
            }
            state.attempts += 1;
        }
        let Ok(permit) = self.hashing.clone().try_acquire_owned() else {
            return Some(error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "login_busy",
                "登录处理中，请稍后重试",
                false,
            ));
        };
        let body = std::mem::replace(request.body_mut(), axum::body::Body::empty());
        let Ok(Ok(bytes)) =
            tokio::time::timeout(Duration::from_secs(5), to_bytes(body, 4096)).await
        else {
            return Some(reject());
        };
        let Ok(login) = serde_json::from_slice::<Login>(&bytes) else {
            return Some(reject());
        };
        if login.username.len() > 80 || login.password.len() > 1024 {
            return Some(reject());
        }
        let account = account.clone();
        let verified = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let verified = account.verify(&login.username, &login.password);
            (account, verified)
        })
        .await
        .ok();
        let Some((account, true)) = verified else {
            return Some(reject());
        };
        let session = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.account.as_ref() != Some(&account) {
            return Some(reject());
        }
        state.sessions.retain(|_, s| s.expires > Instant::now());
        if state.sessions.len() >= 64 {
            return Some(error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "session_limit",
                "请退出其他会话后重试",
                false,
            ));
        }
        state.sessions.insert(
            session.clone(),
            Session {
                authority: one_header(request.headers(), "host")
                    .unwrap_or_default()
                    .into(),
                expires: Instant::now() + Duration::from_secs(8 * 3600),
            },
        );
        let mut response = StatusCode::NO_CONTENT.into_response();
        response.headers_mut().insert(
            header::SET_COOKIE,
            format!(
                "{}={session}; Path=/; Max-Age=28800; HttpOnly; SameSite=Strict",
                self.cookie
            )
            .parse()
            .expect("cookie"),
        );
        Some(response)
    }
}
