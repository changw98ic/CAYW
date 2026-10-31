//! Ephemeral local browser handoff. Control credentials never enter a URL.
//! A one-use, 60-second launch ticket becomes an HttpOnly session cookie. Both
//! are bound to the receiving authority and die when this gateway exits.
use super::{authorized, error_response, one_header};
use axum::{
    Json,
    extract::Request,
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

const TICKET_TTL: Duration = Duration::from_secs(60);
const SESSION_TTL: Duration = Duration::from_secs(8 * 60 * 60);
const MAX_TICKETS: usize = 16;
const MAX_SESSIONS: usize = 64;
struct Grant {
    authority: String,
    expires: Instant,
}
#[derive(Default)]
struct Grants {
    tickets: HashMap<String, Grant>,
    sessions: HashMap<String, Grant>,
}
pub(super) struct BrowserSessions {
    cookie_name: String,
    grants: Mutex<Grants>,
}
fn random() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
impl BrowserSessions {
    pub fn new(port: u16) -> Self {
        Self {
            cookie_name: format!("cayw_session_{port}"),
            grants: Mutex::new(Grants::default()),
        }
    }
    fn cookie<'a>(&self, headers: &'a HeaderMap) -> Option<&'a str> {
        let mut found = None;
        for header in headers.get_all(header::COOKIE) {
            for part in header.to_str().ok()?.split(';') {
                let (name, value) = part.trim().split_once('=')?;
                if name == self.cookie_name {
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
        let Some(value) = self.cookie(headers) else {
            return false;
        };
        self.grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .get(value)
            .is_some_and(|grant| {
                grant.expires > Instant::now()
                    && Some(grant.authority.as_str()) == one_header(headers, "host")
            })
    }
    pub fn handle(&self, request: &Request, control: &str) -> Option<Response> {
        let path = request.uri().path();
        let mint = path == "/api/browser/launch";
        let logout = path == "/api/browser/logout";
        let exchange = path == "/" && request.uri().query().is_some();
        if !mint && !logout && !exchange {
            return None;
        }
        let reject = || {
            error_response(
                StatusCode::FORBIDDEN,
                "forbidden",
                "Launch link is invalid or expired; run cayw start again",
                false,
            )
        };
        // The outer guard has already validated exact loopback Host and Origin.
        let authority = one_header(request.headers(), "host").unwrap_or_default();
        let mut grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        grants.tickets.retain(|_, grant| grant.expires > now);
        grants.sessions.retain(|_, grant| grant.expires > now);
        if mint {
            if request.method() != Method::POST
                || request.uri().query().is_some()
                || !authorized(request.headers(), control)
            {
                return Some(reject());
            }
            if grants.tickets.len() >= MAX_TICKETS {
                return Some(error_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    "launch_busy",
                    "Too many unconsumed launch links; retry after one minute",
                    false,
                ));
            }
            let ticket = random();
            grants.tickets.insert(
                ticket.clone(),
                Grant {
                    authority: authority.into(),
                    expires: now + TICKET_TTL,
                },
            );
            return Some(Json(json!({"launch_url":format!("http://{authority}/?launch={ticket}"),"expires_in":60})).into_response());
        }
        if logout {
            if request.method() != Method::POST || request.uri().query().is_some() {
                return Some(reject());
            }
            if let Some(cookie) = self.cookie(request.headers()) {
                grants.sessions.remove(cookie);
            }
            let mut response = StatusCode::NO_CONTENT.into_response();
            response.headers_mut().insert(
                header::SET_COOKIE,
                format!(
                    "{}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict",
                    self.cookie_name
                )
                .parse()
                .expect("cookie"),
            );
            return Some(response);
        }
        if request.method() != Method::GET {
            return Some(reject());
        }
        let Some(ticket) = request
            .uri()
            .query()
            .and_then(|query| query.strip_prefix("launch="))
        else {
            return Some(reject());
        };
        if ticket.len() != 64 || !ticket.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Some(reject());
        }
        if grants
            .tickets
            .get(ticket)
            .is_none_or(|grant| grant.authority != authority)
        {
            return Some(reject());
        }
        if grants.sessions.len() >= MAX_SESSIONS {
            return Some(error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "session_limit",
                "Browser session limit reached; disconnect browser sessions or restart the gateway",
                false,
            ));
        }
        grants.tickets.remove(ticket);
        let session = random();
        grants.sessions.insert(
            session.clone(),
            Grant {
                authority: authority.into(),
                expires: now + SESSION_TTL,
            },
        );
        let mut response = StatusCode::SEE_OTHER.into_response();
        response
            .headers_mut()
            .insert(header::LOCATION, "/".parse().expect("location"));
        response.headers_mut().insert(
            header::SET_COOKIE,
            format!(
                "{}={session}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict",
                self.cookie_name,
                SESSION_TTL.as_secs()
            )
            .parse()
            .expect("cookie"),
        );
        Some(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const CONTROL: &str = "0123456789abcdef0123456789abcdef";
    fn request(method: Method, uri: &str, host: &str) -> Request {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("host", host)
            .header("authorization", format!("Bearer {CONTROL}"))
            .body(axum::body::Body::empty())
            .unwrap()
    }
    #[test]
    fn expired_or_other_authority_tickets_and_previous_gateway_cookies_are_rejected() {
        let browser = BrowserSessions::new(1234);
        let ticket = "a".repeat(64);
        browser.grants.lock().unwrap().tickets.insert(
            ticket.clone(),
            Grant {
                authority: "127.0.0.1:1234".into(),
                expires: Instant::now() - Duration::from_secs(1),
            },
        );
        let uri = format!("/?launch={ticket}");
        assert_eq!(
            browser
                .handle(&request(Method::GET, &uri, "127.0.0.1:1234"), CONTROL)
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        browser.grants.lock().unwrap().tickets.insert(
            ticket,
            Grant {
                authority: "127.0.0.1:1234".into(),
                expires: Instant::now() + TICKET_TTL,
            },
        );
        assert_eq!(
            browser
                .handle(&request(Method::GET, &uri, "localhost:1234"), CONTROL)
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let response = browser
            .handle(&request(Method::GET, &uri, "127.0.0.1:1234"), CONTROL)
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("host", "127.0.0.1:1234".parse().unwrap());
        headers.insert(
            "cookie",
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .parse()
                .unwrap(),
        );
        assert!(browser.authenticated(&headers));
        assert!(!BrowserSessions::new(1234).authenticated(&headers));
        let session = browser.cookie(&headers).unwrap().to_owned();
        browser
            .grants
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session)
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        assert!(!browser.authenticated(&headers));
    }
}
