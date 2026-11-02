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
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

/// Validated single operator account. Only a salted Argon2id digest is stored.
#[derive(Clone, Serialize, Deserialize)]
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
    sessions: HashMap<String, Session>,
    window: Instant,
    attempts: usize,
}
pub(super) struct BrowserAccess {
    mode: WebAccess,
    cookie: String,
    state: Mutex<State>,
    hashing: Arc<Semaphore>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    username: String,
    password: String,
}
impl BrowserAccess {
    pub fn new(mode: WebAccess, port: u16) -> Self {
        Self {
            mode,
            cookie: format!("cayw_session_{port}"),
            state: Mutex::new(State {
                sessions: HashMap::new(),
                window: Instant::now(),
                attempts: 0,
            }),
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
    pub fn authenticated(&self, headers: &HeaderMap) -> bool {
        match &self.mode {
            WebAccess::Local | WebAccess::Lan(None) => true,
            WebAccess::LegacyBearer(token) => authorized(headers, token),
            WebAccess::Lan(Some(_)) => self.cookie(headers).is_some_and(|value| {
                self.state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .sessions
                    .get(value)
                    .is_some_and(|grant| {
                        grant.expires > Instant::now()
                            && Some(grant.authority.as_str()) == one_header(headers, "host")
                    })
            }),
        }
    }
    pub async fn handle(&self, request: &mut Request) -> Option<Response> {
        let path = request.uri().path();
        if path == "/api/auth/status" && request.method() == Method::GET {
            return Some(Json(json!({"mode":if matches!(self.mode,WebAccess::Lan(_)){"lan"}else{"local"},"authenticated":self.authenticated(request.headers()),"login_required":matches!(self.mode,WebAccess::Lan(Some(_)))})).into_response());
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
        let WebAccess::Lan(Some(account)) = &self.mode else {
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
            account.verify(&login.username, &login.password)
        })
        .await
        .unwrap_or(false);
        if !verified {
            return Some(reject());
        }
        let session = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
