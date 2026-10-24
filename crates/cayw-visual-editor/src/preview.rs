//! Read-only preview origin for a project page.
//!
//! The preview is deliberately separate from the editor origin so the editor
//! token is never visible to project HTML or project scripts. Static files are
//! opened component by component below one held directory handle without
//! following symbolic links. Nothing in this module writes to the project.

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    response::Response,
    routing::get,
};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::{ambient_authority, fs::Dir};
use html5gum::{DefaultEmitter, Token, Tokenizer};
use serde_json::json;
use std::{
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, RwLock},
};

/// URL path where the preview injects the parent/iframe bridge.
pub const BRIDGE_PATH: &str = "/__cayw_bridge.js";

const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ASSET_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMPONENTS: usize = 16;
const MAX_PATH_BYTES: usize = 512;

/// Files the preview is willing to serve. Anything else is refused rather than
/// guessed from its bytes.
const ALLOWED_TYPES: &[(&str, &str)] = &[
    ("html", "text/html; charset=utf-8"),
    ("htm", "text/html; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("js", "text/javascript; charset=utf-8"),
    ("mjs", "text/javascript; charset=utf-8"),
    ("json", "application/json"),
    ("map", "application/json"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("ico", "image/x-icon"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
    ("txt", "text/plain; charset=utf-8"),
    ("webmanifest", "application/manifest+json"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("wasm", "application/wasm"),
];

#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    #[error("preview path is not a portable relative resource path")]
    InvalidPath,
    #[error("preview resource was not found or is not a regular file")]
    Missing,
    #[error("preview resource exceeds the serving limit")]
    TooLarge,
    #[error("preview root could not be opened: {0}")]
    Root(std::io::Error),
    #[error("preview page is not configured as a relative HTML path")]
    Page,
}

impl PreviewError {
    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidPath | Self::Page => StatusCode::BAD_REQUEST,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Missing | Self::Root(_) => StatusCode::NOT_FOUND,
        }
    }
}

/// Everything the preview origin needs. `edit_html` is the current injected
/// draft supplied by the source engine; it is only served when the iframe asks
/// for edit mode.
pub struct PreviewState {
    root: Dir,
    page: String,
    editor_origin: String,
    authority: String,
    bridge_js: Vec<u8>,
    edit_html: RwLock<Option<String>>,
    edit_enabled: AtomicBool,
}

impl std::fmt::Debug for PreviewState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreviewState")
            .field("page", &self.page)
            .field("editor_origin", &self.editor_origin)
            .field("authority", &self.authority)
            .field(
                "edit_html",
                &self.edit_html.read().map(|value| value.is_some()),
            )
            .finish_non_exhaustive()
    }
}

impl PreviewState {
    /// Opens the project root once and keeps that directory handle for the
    /// lifetime of the preview. Renaming the original path cannot redirect it.
    pub fn open(
        root: &Path,
        page: &str,
        editor_origin: String,
        authority: String,
        bridge_js: Vec<u8>,
    ) -> Result<Self, PreviewError> {
        let page = normalize_page(page)?;
        let root = Dir::open_ambient_dir(root, ambient_authority()).map_err(PreviewError::Root)?;
        Ok(Self {
            root,
            page,
            editor_origin,
            authority,
            bridge_js,
            edit_html: RwLock::new(None),
            edit_enabled: AtomicBool::new(false),
        })
    }

    /// Replaces the draft the edit-mode iframe sees. Passing `None` disables
    /// edit mode until the source engine supplies a snapshot again.
    pub fn set_edit_html(&self, html: Option<String>) {
        let enabled = html.is_some();
        *self
            .edit_html
            .write()
            .unwrap_or_else(|error| error.into_inner()) = html;
        self.edit_enabled.store(enabled, Ordering::Release);
    }

    pub fn editor_origin(&self) -> &str {
        &self.editor_origin
    }

    pub fn page(&self) -> &str {
        &self.page
    }

    fn current_edit_html(&self) -> Option<String> {
        if !self.edit_enabled.load(Ordering::Acquire) {
            return None;
        }
        self.edit_html
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

pub fn router(state: Arc<PreviewState>) -> Router {
    Router::new().fallback(get(handle)).with_state(state)
}

async fn handle(State(state): State<Arc<PreviewState>>, uri: Uri, headers: HeaderMap) -> Response {
    if !host_matches(&headers, &state.authority) {
        return text(StatusCode::FORBIDDEN, "preview host is not allowed");
    }
    if uri.path() == BRIDGE_PATH {
        return bridge(&state);
    }
    let Some(decoded) = decode_path(uri.path()) else {
        return text(StatusCode::BAD_REQUEST, "preview path is invalid");
    };
    let page_path = format!("/{}", state.page);
    if decoded == "/" {
        // A nested page needs its directory as the base URL. Redirecting the
        // root alias keeps every relative resource next to the page.
        if state.page.contains('/') {
            return redirect(&state, uri.query());
        }
        return page(&state, uri.query());
    }
    if decoded == page_path {
        return page(&state, uri.query());
    }
    asset(&state, &decoded)
}

fn redirect(state: &PreviewState, query: Option<&str>) -> Response {
    let mut location = format!("/{}", encode_path(&state.page));
    if let Some(query) = query {
        location.push('?');
        location.push_str(query);
    }
    let Ok(location) = HeaderValue::from_str(&location) else {
        return text(
            StatusCode::BAD_REQUEST,
            "preview page path is not a valid URL",
        );
    };
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    response.headers_mut().insert(header::LOCATION, location);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("/")
}

fn page(state: &PreviewState, query: Option<&str>) -> Response {
    let edit = query_is_edit(query);
    let (bytes, injected) = if edit {
        let Some(html) = state.current_edit_html() else {
            return text(
                StatusCode::SERVICE_UNAVAILABLE,
                "no editable draft is available",
            );
        };
        (
            inject_bridge(&html, &state.editor_origin).into_bytes(),
            true,
        )
    } else {
        match read_relative(&state.root, &state.page, MAX_PAGE_BYTES) {
            Ok(bytes) => (bytes, false),
            Err(error) => return preview_error(error),
        }
    };
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("frame-ancestors {}", state.editor_origin)) {
        response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, value);
    }
    if injected {
        response
            .headers_mut()
            .insert("x-cayw-edit", HeaderValue::from_static("1"));
    }
    response
}

fn asset(state: &PreviewState, decoded: &str) -> Response {
    if decoded.starts_with("/__cayw") {
        return text(StatusCode::NOT_FOUND, "resource is reserved");
    }
    let Some(relative) = decoded.strip_prefix('/') else {
        return text(StatusCode::BAD_REQUEST, "preview path is invalid");
    };
    let Some(mime) = mime(relative) else {
        return text(StatusCode::NOT_FOUND, "resource type is not served");
    };
    let bytes = match read_relative(&state.root, relative, MAX_ASSET_BYTES) {
        Ok(bytes) => bytes,
        Err(error) => return preview_error(error),
    };
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("frame-ancestors {}", state.editor_origin)) {
        response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, value);
    }
    response
}

fn bridge(state: &PreviewState) -> Response {
    let mut response = Response::new(Body::from(state.bridge_js.clone()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/javascript; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Injects the bridge where it runs before every project script without
/// changing the original document structure. The preferred point is directly
/// after the original `<head ...>` opener: a script inserted before a real
/// head would make the browser create an implicit head and ignore the original
/// head attributes. If a script comes earlier, or there is no head, the bridge
/// goes immediately before the first script. With neither head nor script it
/// goes after a canonical doctype, or at the very front. A real doctype is
/// never moved, because a doctype that is not first would switch the page to
/// quirks mode. The original bytes on disk are never changed: callers pass a
/// draft rebuilt from the clean source for every snapshot.
pub fn inject_bridge(html: &str, editor_origin: &str) -> String {
    let insert_at = bridge_insert_offset(html);
    let mut output = String::with_capacity(html.len() + 192);
    output.push_str(&html[..insert_at]);
    output.push_str("<script src=\"");
    output.push_str(BRIDGE_PATH);
    output.push_str("?editor_origin=");
    output.push_str(&percent_encode(editor_origin));
    output.push_str("\"></script>\n");
    output.push_str(&html[insert_at..]);
    output
}

/// Finds the insertion offset from tokenizer spans only. Leading whitespace, a
/// BOM and comments do not make a doctype non-canonical; any element, end tag,
/// text or tokenizer error before it does.
fn bridge_insert_offset(html: &str) -> usize {
    let mut head_end: Option<usize> = None;
    let mut first_script: Option<usize> = None;
    let mut canonical_doctype: Option<usize> = None;
    let mut doctype_seen = false;
    let mut meaningful_before = false;
    let mut emitter = DefaultEmitter::<usize>::new_with_span();
    emitter.naively_switch_states(true);
    for token in Tokenizer::new_with_emitter(html, emitter) {
        match token {
            Ok(Token::Doctype(doctype)) => {
                if !meaningful_before && !doctype_seen {
                    doctype_seen = true;
                    let span = doctype.span;
                    if valid_offset(html, span.start) && valid_offset(html, span.end) {
                        canonical_doctype = Some(span.end);
                    }
                }
            }
            Ok(Token::Comment(_)) => {}
            Ok(Token::String(text)) => {
                let value = String::from_utf8_lossy(&text);
                if !value.trim_start_matches('\u{feff}').trim().is_empty() {
                    meaningful_before = true;
                }
            }
            Ok(Token::StartTag(tag)) => {
                meaningful_before = true;
                let name = String::from_utf8_lossy(&tag.name).to_ascii_lowercase();
                if name == "head"
                    && head_end.is_none()
                    && valid_offset(html, tag.span.start)
                    && valid_offset(html, tag.span.end)
                {
                    head_end = Some(tag.span.end);
                }
                if name == "script" && first_script.is_none() && valid_offset(html, tag.span.start)
                {
                    first_script = Some(tag.span.start);
                }
            }
            Ok(Token::EndTag(_)) | Ok(Token::Error(_)) | Err(_) => {
                meaningful_before = true;
            }
        }
    }
    match (head_end, first_script, canonical_doctype) {
        (Some(head), Some(script), _) => head.min(script),
        (Some(head), None, _) => head,
        (None, Some(script), _) => script,
        (None, None, Some(doctype)) => doctype,
        (None, None, None) => 0,
    }
}

fn valid_offset(html: &str, offset: usize) -> bool {
    offset <= html.len() && html.is_char_boundary(offset)
}

fn normalize_page(page: &str) -> Result<String, PreviewError> {
    let page = page.strip_prefix('/').unwrap_or(page);
    let page = page.strip_prefix("./").unwrap_or(page);
    let components = validate_components(page)?;
    let Some(last) = components.last() else {
        return Err(PreviewError::Page);
    };
    // Match the source engine: HTML page extensions are case-insensitive.
    let extension = extension(last)
        .map(str::to_ascii_lowercase)
        .ok_or(PreviewError::Page)?;
    if !matches!(extension.as_str(), "html" | "htm") {
        return Err(PreviewError::Page);
    }
    Ok(page.to_owned())
}

fn read_relative(root: &Dir, relative: &str, limit: usize) -> Result<Vec<u8>, PreviewError> {
    let components = validate_components(relative)?;
    let (name, parents) = components.split_last().ok_or(PreviewError::InvalidPath)?;
    let mut dir = root.try_clone().map_err(PreviewError::Root)?;
    for parent in parents {
        dir = dir
            .open_dir_nofollow(parent)
            .map_err(|_| PreviewError::Missing)?;
    }
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let mut file = dir
        .open_with(name, &options)
        .map_err(|_| PreviewError::Missing)?;
    let metadata = file.metadata().map_err(PreviewError::Root)?;
    if !metadata.is_file() {
        return Err(PreviewError::Missing);
    }
    if metadata.len() > limit as u64 {
        return Err(PreviewError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(metadata.len().min(limit as u64) as usize);
    file.by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(PreviewError::Root)?;
    if bytes.len() > limit {
        return Err(PreviewError::TooLarge);
    }
    Ok(bytes)
}

fn validate_components(path: &str) -> Result<Vec<&str>, PreviewError> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(PreviewError::InvalidPath);
    }
    let mut components = Vec::new();
    for part in path.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.len() > 255
            || part.starts_with('.')
            || part.contains(['\\', ':', '\0'])
            || part.chars().any(char::is_control)
        {
            return Err(PreviewError::InvalidPath);
        }
        components.push(part);
    }
    if components.len() > MAX_COMPONENTS {
        return Err(PreviewError::InvalidPath);
    }
    Ok(components)
}

fn extension(name: &str) -> Option<&str> {
    let (_, extension) = name.rsplit_once('.')?;
    Some(extension).filter(|value| !value.is_empty())
}

fn mime(path: &str) -> Option<&'static str> {
    let extension = extension(path)?.to_ascii_lowercase();
    ALLOWED_TYPES
        .iter()
        .find_map(|(candidate, mime)| (*candidate == extension).then_some(*mime))
}

fn decode_path(raw: &str) -> Option<String> {
    if !raw.starts_with('/') || raw.len() > MAX_PATH_BYTES * 3 {
        return None;
    }
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(bytes.get(index + 1)?)?;
            let low = hex(bytes.get(index + 2)?)?;
            let value = (high << 4) | low;
            if value == 0 || value == b'\\' || (value < 0x20 && value != b'\t') {
                return None;
            }
            decoded.push(value);
            index += 3;
        } else {
            if bytes[index] == 0 {
                return None;
            }
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded).ok()?;
    if decoded.contains(['\\', '\0']) {
        return None;
    }
    Some(decoded)
}

fn hex(byte: &u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn query_is_edit(query: Option<&str>) -> bool {
    query.is_some_and(|query| {
        query.split('&').any(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            key == "cayw_edit" && value == "1"
        })
    })
}

fn host_matches(headers: &HeaderMap, authority: &str) -> bool {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case(authority))
}

fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(value.len() * 3);
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            output.push(byte as char);
        } else {
            output.push('%');
            output.push(HEX[(byte >> 4) as usize] as char);
            output.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    output
}

fn text(status: StatusCode, message: &str) -> Response {
    let body = json!({"error": {"code": status_code(status), "message": message, "outcome_unknown": false}});
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn preview_error(error: PreviewError) -> Response {
    let status = error.status();
    text(status, &error.to_string())
}

fn status_code(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "bad_request",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::PAYLOAD_TOO_LARGE => "too_large",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::SERVICE_UNAVAILABLE => "unavailable",
        _ => "error",
    }
}
