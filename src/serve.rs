//! Local HTTP JSON API server (`bitefoundry-obsidian serve`, default `:8792`).
//!
//! Endpoints:
//! - `GET  /health`
//! - `GET  /api/v1/vaults`
//! - `GET  /api/v1/search?q=...&limit=...`
//! - `GET  /api/v1/notes?folder=...`
//! - `GET  /api/v1/notes/{*path}`
//! - `POST /api/v1/notes`
//! - `POST /api/v1/notes/append`
//! - `POST /api/v1/daily`
//! - `GET  /api/v1/backlinks?note=...`
//! - `GET  /api/v1/graph?note=...&depth=2`
//! - `GET  /api/v1/tags?tag=...`
//! - `GET  /api/v1/tasks?status=open|done|all&note=...`
//! - `POST /api/v1/tasks/toggle`
//! - `POST /api/v1/reindex`

use std::{
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::Arc,
};

use anyhow::{bail, Context, Result};
use axum::{
    extract::{Path as AxumPath, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::{
    config::{AppConfig, VaultProfile},
    models::{CodedError, TaskFilter, VaultSummary},
    storage::Storage,
    vault::{
        fs::{
            append_to_note, create_note, ensure_daily_note, file_mtime_and_size, safe_vault_path,
            toggle_task_at_line_verified,
        },
        merged_vaults, resolve_request_vault,
    },
};

#[derive(Clone)]
pub struct ServeState {
    pub cfg: AppConfig,
    pub default_vault: VaultProfile,
    pub storage: Storage,
    pub token: Option<String>,
}

/// Request-origin policy of the HTTP API. The default accepts loopback hosts and origins only.
#[derive(Debug, Clone, Default)]
pub struct ServePolicy {
    /// Extra `Host` header names accepted besides `127.0.0.1`, `localhost` and `[::1]`.
    pub extra_hosts: Vec<String>,
    /// Exact browser origins (`scheme://host[:port]`) allowed to call the API cross-origin.
    pub allowed_origins: Vec<String>,
}

/// Launch options for [`run_http_server_with_options`].
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// Bearer token required on `/api/v1/*`. Blank counts as unset.
    pub token: Option<String>,
    /// Serve a loopback bind without any token instead of generating one.
    pub allow_keyless: bool,
    /// Exact browser origins allowed cross-origin (`--allow-origin` / `--cors-origin`).
    pub allow_origins: Vec<String>,
    /// Extra `Host` header names to accept (`--allow-host`).
    pub allow_hosts: Vec<String>,
}

const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

struct GuardPolicy {
    hosts: Vec<String>,
    origins: Vec<String>,
}

impl GuardPolicy {
    fn new(policy: &ServePolicy) -> Self {
        let mut hosts: Vec<String> = LOOPBACK_HOSTS.iter().map(|h| h.to_string()).collect();
        for extra in &policy.extra_hosts {
            if let Some(host) = normalize_host(extra) {
                if !hosts.contains(&host) {
                    hosts.push(host);
                }
            }
        }
        let origins = policy
            .allowed_origins
            .iter()
            .filter_map(|o| normalize_origin(o))
            .collect();
        Self { hosts, origins }
    }
}

/// Lower-cases a `Host` value and strips its optional port and trailing dot.
/// IPv6 literals come back bracketed (`[::1]`). Returns `None` for anything malformed.
fn normalize_host(raw: &str) -> Option<String> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let valid_port = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if let Some(rest) = s.strip_prefix('[') {
        let (addr, tail) = rest.split_once(']')?;
        if !(tail.is_empty() || tail.strip_prefix(':').is_some_and(valid_port)) {
            return None;
        }
        let parsed: Ipv6Addr = addr.parse().ok()?;
        return Some(format!("[{parsed}]"));
    }
    if let Ok(parsed) = s.parse::<Ipv6Addr>() {
        return Some(format!("[{parsed}]"));
    }
    let host = match s.rsplit_once(':') {
        Some((host, port)) if valid_port(port) => host,
        Some(_) => return None,
        None => s.as_str(),
    };
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.contains(['/', '@', ' ']) {
        return None;
    }
    Some(host.to_string())
}

/// Normalizes an exact origin (`scheme://host[:port]`, no path) to lower case.
fn normalize_origin(raw: &str) -> Option<String> {
    let s = raw.trim().trim_end_matches('/').to_ascii_lowercase();
    let rest = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))?;
    if rest.is_empty() || rest.contains(['/', '*', ' ', '?', '#']) {
        return None;
    }
    HeaderValue::from_str(&s).ok()?;
    Some(s)
}

fn is_loopback_origin(origin: &str) -> bool {
    let s = origin.trim().to_ascii_lowercase();
    let Some(rest) = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
    else {
        return false;
    };
    normalize_host(rest).is_some_and(|h| LOOPBACK_HOSTS.contains(&h.as_str()))
}

fn host_allowed(policy: &GuardPolicy, req: &Request) -> bool {
    let mut seen = 0usize;
    let mut check = |raw: &str| {
        seen += 1;
        normalize_host(raw).is_some_and(|h| policy.hosts.contains(&h))
    };
    for value in req.headers().get_all(header::HOST) {
        let Ok(raw) = value.to_str() else {
            return false;
        };
        if !check(raw) {
            return false;
        }
    }
    // HTTP/2 and absolute-form requests carry the authority in the URI.
    if let Some(authority) = req.uri().authority() {
        if !check(authority.as_str()) {
            return false;
        }
    }
    seen > 0
}

fn origin_allowed(policy: &GuardPolicy, origin: &str) -> bool {
    let candidate = origin.trim().to_ascii_lowercase();
    policy.origins.contains(&candidate) || is_loopback_origin(&candidate)
}

/// Rejects requests whose `Host` is not allow-listed (DNS rebinding) or whose `Origin` is a
/// foreign website, before any handler, body extractor or vault sync runs.
async fn guard(State(policy): State<Arc<GuardPolicy>>, req: Request, next: Next) -> Response {
    if !host_allowed(&policy, &req) {
        return err_resp(StatusCode::FORBIDDEN, "Host not allowed");
    }
    for origin in req.headers().get_all(header::ORIGIN) {
        let allowed = origin
            .to_str()
            .ok()
            .is_some_and(|o| origin_allowed(&policy, o));
        if !allowed {
            return err_resp(StatusCode::FORBIDDEN, "Origin not allowed");
        }
    }
    next.run(req).await
}

fn cors_layer(origins: &[String]) -> Option<CorsLayer> {
    let values: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    if values.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(values))
            .allow_methods([Method::GET, Method::POST])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
    )
}

/// Builds the API router with the default loopback-only [`ServePolicy`].
pub fn build_router(state: Arc<ServeState>) -> Router {
    build_router_with_policy(state, ServePolicy::default())
}

/// Builds the API router. No CORS headers are sent unless `policy.allowed_origins` lists an
/// exact origin; every route, including `/health`, sits behind the Host/Origin guard.
pub fn build_router_with_policy(state: Arc<ServeState>, policy: ServePolicy) -> Router {
    let guard_policy = Arc::new(GuardPolicy::new(&policy));
    let mut router = Router::new()
        .route("/health", get(handle_health))
        .route("/api/v1/vaults", get(handle_list_vaults))
        .route("/api/v1/search", get(handle_search))
        .route(
            "/api/v1/notes",
            get(handle_list_notes).post(handle_create_note),
        )
        .route(
            "/api/v1/notes/append",
            get(handle_read_append_note).post(handle_append_note),
        )
        .route("/api/v1/notes/{*path}", get(handle_read_note))
        .route("/api/v1/daily", post(handle_daily_note))
        .route("/api/v1/backlinks", get(handle_backlinks))
        .route("/api/v1/graph", get(handle_graph))
        .route("/api/v1/tags", get(handle_tags))
        .route("/api/v1/tasks", get(handle_tasks))
        .route("/api/v1/tasks/toggle", post(handle_toggle_task))
        .route("/api/v1/reindex", post(handle_reindex));
    if let Some(cors) = cors_layer(&guard_policy.origins) {
        router = router.layer(cors);
    }
    router
        .layer(middleware::from_fn_with_state(guard_policy, guard))
        .with_state(state)
}

/// Generates a 32-byte random Bearer token (64 hex characters) from the OS CSPRNG.
pub fn generate_serve_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Decides the Bearer token for a bind address. Returns `(token, generated)`.
///
/// - an explicit non-blank token is used as given;
/// - a non-loopback bind without a token is refused (`--allow-keyless` does not lift this);
/// - a loopback bind without a token gets a freshly generated one, unless `allow_keyless`.
pub fn resolve_serve_token(
    addr: &SocketAddr,
    token: Option<String>,
    allow_keyless: bool,
) -> Result<(Option<String>, bool)> {
    if let Some(token) = token.filter(|t| !t.trim().is_empty()) {
        return Ok((Some(token), false));
    }
    if !addr.ip().is_loopback() {
        bail!("Refusing to serve on {addr} without --token. Bind to 127.0.0.1 or pass --token.");
    }
    if allow_keyless {
        return Ok((None, false));
    }
    Ok((Some(generate_serve_token()), true))
}

fn parse_bind_addr(bind: &str, port: u16) -> Result<SocketAddr> {
    let trimmed = bind.trim();
    let bare = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    format!("{bind}:{port}")
        .parse()
        .with_context(|| format!("Invalid bind address: {bind}:{port}"))
}

fn serve_policy(addr: &SocketAddr, opts: &ServeOptions) -> Result<ServePolicy> {
    let mut extra_hosts = vec![addr.ip().to_string()];
    for host in &opts.allow_hosts {
        if normalize_host(host).is_none() {
            bail!("Invalid --allow-host value '{host}'. Pass a host name or IP address such as 192.168.1.20.");
        }
        extra_hosts.push(host.clone());
    }
    let mut allowed_origins = Vec::new();
    for origin in &opts.allow_origins {
        let Some(normalized) = normalize_origin(origin) else {
            bail!("Invalid --allow-origin value '{origin}'. Pass one exact origin such as https://app.example.com; wildcards and paths are not supported.");
        };
        allowed_origins.push(normalized);
    }
    Ok(ServePolicy {
        extra_hosts,
        allowed_origins,
    })
}

pub async fn run_http_server(
    cfg: AppConfig,
    vault: VaultProfile,
    storage: Storage,
    bind: &str,
    port: u16,
    token: Option<String>,
) -> Result<()> {
    let opts = ServeOptions {
        token,
        ..ServeOptions::default()
    };
    run_http_server_with_options(cfg, vault, storage, bind, port, opts).await
}

pub async fn run_http_server_with_options(
    cfg: AppConfig,
    vault: VaultProfile,
    storage: Storage,
    bind: &str,
    port: u16,
    opts: ServeOptions,
) -> Result<()> {
    // Every refusal happens before the vault is scanned or the socket is bound.
    let addr = parse_bind_addr(bind, port)?;
    let policy = serve_policy(&addr, &opts)?;
    let (token, generated) = resolve_serve_token(&addr, opts.token.clone(), opts.allow_keyless)?;

    let _ = storage.sync_vault(&vault);
    let state = Arc::new(ServeState {
        cfg,
        default_vault: vault.clone(),
        storage,
        token: token.clone(),
    });
    let app = build_router_with_policy(state, policy);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind HTTP server on {addr}"))?;
    let local_addr = listener.local_addr().unwrap_or(addr);
    eprintln!(
        "obsidian HTTP API listening on http://{local_addr} (vault: {})",
        vault.display_name()
    );
    match (&token, generated) {
        (Some(token), true) => {
            eprintln!("obsidian HTTP API token (generated for this run): {token}");
            eprintln!(
                "  Send it as `Authorization: Bearer <token>` on /api/v1/* requests. Pass --token to choose your own, or --allow-keyless to serve loopback without a token."
            );
        }
        (None, _) => eprintln!(
            "warning: serving without a token (--allow-keyless): any local process can read and write this vault."
        ),
        _ => {}
    }
    axum::serve(listener, app).await?;
    Ok(())
}

fn check_auth(state: &ServeState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    let Some(expected) = state.token.as_deref().filter(|t| !t.trim().is_empty()) else {
        return Ok(());
    };
    let got = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    if got == Some(expected) {
        Ok(())
    } else {
        Err(Box::new(err_resp(
            StatusCode::UNAUTHORIZED,
            "Missing or invalid Bearer token",
        )))
    }
}

fn resolve_req_vault_unsynced(
    state: &ServeState,
    vault_param: Option<&str>,
) -> Result<VaultProfile, Box<Response>> {
    match vault_param.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(state.default_vault.clone()),
        Some(spec) => resolve_request_vault(&state.cfg, &state.default_vault, spec)
            .map_err(|e| Box::new(err_resp(StatusCode::NOT_FOUND, &e.to_string()))),
    }
}

fn resolve_req_vault(
    state: &ServeState,
    vault_param: Option<&str>,
) -> Result<VaultProfile, Box<Response>> {
    let vault = resolve_req_vault_unsynced(state, vault_param)?;
    let _ = state.storage.sync_vault(&vault);
    Ok(vault)
}

fn err_resp(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "ok": false, "error": msg }))).into_response()
}

async fn handle_health(State(state): State<Arc<ServeState>>) -> impl IntoResponse {
    let _ = state.storage.sync_vault(&state.default_vault);
    let notes = state
        .storage
        .note_count(&state.default_vault.id)
        .unwrap_or(0);
    Json(json!({
        "ok": true,
        "service": "obsidian-cli",
        "version": env!("CARGO_PKG_VERSION"),
        "vault": state.default_vault.display_name(),
        "vault_id": state.default_vault.id,
        "notes": notes,
    }))
}

async fn handle_list_vaults(State(state): State<Arc<ServeState>>, headers: HeaderMap) -> Response {
    if let Err(r) = check_auth(&state, &headers) {
        return *r;
    }
    let vaults: Vec<VaultSummary> = merged_vaults(&state.cfg)
        .into_iter()
        .map(|v| {
            let exists = v.path.is_dir();
            if exists {
                let _ = state.storage.sync_vault(&v);
            }
            let active = v.id == state.default_vault.id;
            let count = state.storage.note_count(&v.id).unwrap_or(0);
            VaultSummary {
                id: v.id,
                name: v.name,
                exists,
                path: v.path,
                active,
                discovered: v.discovered,
                open_in_obsidian: v.open_in_obsidian,
                note_count: count,
            }
        })
        .collect();
    Json(json!({ "ok": true, "vaults": vaults })).into_response()
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    limit: Option<usize>,
    vault: Option<String>,
}

async fn handle_search(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> Response {
    if let Err(r) = check_auth(&state, &headers) {
        return *r;
    }
    let vault = match resolve_req_vault(&state, q.vault.as_deref()) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let query = q.q.unwrap_or_default();
    let limit = q.limit.unwrap_or(25).clamp(1, 200);
    match state.storage.search_notes(&vault.id, &query, limit) {
        Ok(hits) => {
            Json(json!({ "ok": true, "query": query, "count": hits.len(), "results": hits }))
                .into_response()
        }
        Err(e) => err_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct ListNotesQuery {
    folder: Option<String>,
    vault: Option<String>,
}

async fn handle_list_notes(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Query(q): Query<ListNotesQuery>,
) -> Response {
    if let Err(r) = check_auth(&state, &headers) {
        return *r;
    }
    let vault = match resolve_req_vault(&state, q.vault.as_deref()) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match state.storage.list_notes(&vault.id, q.folder.as_deref()) {
        Ok(notes) => {
            Json(json!({ "ok": true, "count": notes.len(), "notes": notes })).into_response()
        }
        Err(e) => err_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct VaultOnlyQuery {
    vault: Option<String>,
}

async fn handle_read_append_note(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Query(q): Query<VaultOnlyQuery>,
) -> Response {
    read_note_response(&state, &headers, "append", q.vault.as_deref())
}

async fn handle_read_note(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    AxumPath(path): AxumPath<String>,
    Query(q): Query<VaultOnlyQuery>,
) -> Response {
    read_note_response(&state, &headers, &path, q.vault.as_deref())
}

fn read_note_response(
    state: &ServeState,
    headers: &HeaderMap,
    note_ref: &str,
    vault_param: Option<&str>,
) -> Response {
    if let Err(r) = check_auth(state, headers) {
        return *r;
    }
    let vault = match resolve_req_vault(state, vault_param) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match state.storage.load_note_detail(&vault, note_ref) {
        Ok(detail) => Json(json!({ "ok": true, "note": detail })).into_response(),
        Err(e) => err_resp(StatusCode::NOT_FOUND, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct CreateNoteReq {
    title: String,
    folder: Option<String>,
    content: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    vault: Option<String>,
}

async fn handle_create_note(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(req): Json<CreateNoteReq>,
) -> Response {
    if let Err(r) = check_auth(&state, &headers) {
        return *r;
    }
    let vault = match resolve_req_vault(&state, req.vault.as_deref()) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match create_note(
        &vault.path,
        &req.title,
        req.folder.as_deref(),
        req.content.as_deref(),
        &req.tags,
    ) {
        Ok((rel_path, _)) => {
            let _ = state
                .storage
                .sync_paths(&vault, std::slice::from_ref(&rel_path), &[]);
            let detail = state.storage.load_note_detail(&vault, &rel_path).ok();
            (
                StatusCode::CREATED,
                Json(json!({ "ok": true, "rel_path": rel_path, "note": detail })),
            )
                .into_response()
        }
        Err(e) => err_resp(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct AppendNoteReq {
    note: String,
    text: String,
    vault: Option<String>,
}

async fn handle_append_note(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(req): Json<AppendNoteReq>,
) -> Response {
    if let Err(r) = check_auth(&state, &headers) {
        return *r;
    }
    let vault = match resolve_req_vault(&state, req.vault.as_deref()) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let rel_path = match state.storage.resolve_note_path(&vault.id, &req.note) {
        Ok(p) => p,
        Err(e) => return err_resp(StatusCode::NOT_FOUND, &e.to_string()),
    };
    let abs = match safe_vault_path(&vault.path, &rel_path) {
        Ok(p) => p,
        Err(e) => return err_resp(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    if let Err(e) = append_to_note(&abs, &req.text) {
        return err_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    let _ = state
        .storage
        .sync_paths(&vault, std::slice::from_ref(&rel_path), &[]);
    Json(json!({ "ok": true, "rel_path": rel_path })).into_response()
}

#[derive(Deserialize)]
struct DailyReq {
    date: Option<String>,
    append: Option<String>,
    vault: Option<String>,
}

async fn handle_daily_note(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    body: Option<Json<DailyReq>>,
) -> Response {
    if let Err(r) = check_auth(&state, &headers) {
        return *r;
    }
    let req = body.map(|b| b.0).unwrap_or(DailyReq {
        date: None,
        append: None,
        vault: None,
    });
    let vault = match resolve_req_vault(&state, req.vault.as_deref()) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match ensure_daily_note(&vault, req.date.as_deref()) {
        Ok((rel_path, abs_path, created)) => {
            if let Some(text) = req.append.as_deref().filter(|s| !s.trim().is_empty()) {
                if let Err(e) = append_to_note(&abs_path, text) {
                    return err_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
                }
            }
            let _ = state
                .storage
                .sync_paths(&vault, std::slice::from_ref(&rel_path), &[]);
            let detail = state.storage.load_note_detail(&vault, &rel_path).ok();
            Json(json!({
                "ok": true,
                "rel_path": rel_path,
                "created": created,
                "note": detail,
            }))
            .into_response()
        }
        Err(e) => {
            let invalid = e
                .downcast_ref::<CodedError>()
                .is_some_and(|c| c.code == "invalid_argument");
            let status = if invalid {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            err_resp(status, &e.to_string())
        }
    }
}

#[derive(Deserialize)]
struct BacklinksQuery {
    note: String,
    vault: Option<String>,
}

async fn handle_backlinks(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Query(q): Query<BacklinksQuery>,
) -> Response {
    if let Err(r) = check_auth(&state, &headers) {
        return *r;
    }
    let vault = match resolve_req_vault(&state, q.vault.as_deref()) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let rel_path = match state.storage.resolve_note_path(&vault.id, &q.note) {
        Ok(p) => p,
        Err(e) => return err_resp(StatusCode::NOT_FOUND, &e.to_string()),
    };
    match state.storage.get_backlinks(&vault.id, &rel_path) {
        Ok(backlinks) => Json(json!({
            "ok": true,
            "note": rel_path,
            "count": backlinks.len(),
            "backlinks": backlinks,
        }))
        .into_response(),
        Err(e) => err_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct GraphQuery {
    note: Option<String>,
    depth: Option<usize>,
    vault: Option<String>,
}

async fn handle_graph(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Query(q): Query<GraphQuery>,
) -> Response {
    if let Err(r) = check_auth(&state, &headers) {
        return *r;
    }
    let vault = match resolve_req_vault(&state, q.vault.as_deref()) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let center = match q.note.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(n) => match state.storage.resolve_note_path(&vault.id, n) {
            Ok(p) => Some(p),
            Err(e) => return err_resp(StatusCode::NOT_FOUND, &e.to_string()),
        },
        None => None,
    };
    let depth = q.depth.unwrap_or(2).clamp(1, 5);
    match state.storage.get_graph(&vault, center.as_deref(), depth) {
        Ok(graph) => Json(json!({ "ok": true, "graph": graph })).into_response(),
        Err(e) => err_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct TagsQuery {
    tag: Option<String>,
    vault: Option<String>,
}

async fn handle_tags(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Query(q): Query<TagsQuery>,
) -> Response {
    if let Err(r) = check_auth(&state, &headers) {
        return *r;
    }
    let vault = match resolve_req_vault(&state, q.vault.as_deref()) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Some(tag) = q.tag.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        match state.storage.find_by_tag(&vault.id, tag) {
            Ok(notes) => Json(json!({
                "ok": true,
                "tag": tag.trim_start_matches('#'),
                "count": notes.len(),
                "notes": notes,
            }))
            .into_response(),
            Err(e) => err_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    } else {
        match state.storage.list_tags(&vault.id) {
            Ok(tags) => {
                Json(json!({ "ok": true, "count": tags.len(), "tags": tags })).into_response()
            }
            Err(e) => err_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    }
}

#[derive(Deserialize)]
struct TasksQuery {
    status: Option<String>,
    note: Option<String>,
    vault: Option<String>,
}

async fn handle_tasks(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Query(q): Query<TasksQuery>,
) -> Response {
    if let Err(r) = check_auth(&state, &headers) {
        return *r;
    }
    let vault = match resolve_req_vault(&state, q.vault.as_deref()) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let filter = match q
        .status
        .as_deref()
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("open") => TaskFilter::Open,
        Some("done") => TaskFilter::Done,
        _ => TaskFilter::All,
    };
    let note_path = match q.note.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(n) => match state.storage.resolve_note_path(&vault.id, n) {
            Ok(p) => Some(p),
            Err(e) => return err_resp(StatusCode::NOT_FOUND, &e.to_string()),
        },
        None => None,
    };
    match state
        .storage
        .list_tasks(&vault.id, filter, note_path.as_deref())
    {
        Ok(tasks) => {
            Json(json!({ "ok": true, "count": tasks.len(), "tasks": tasks })).into_response()
        }
        Err(e) => err_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct ToggleTaskReq {
    note: String,
    line: usize,
    expected_text: Option<String>,
    expected_checked: Option<bool>,
    expected_mtime_ms: Option<i64>,
    vault: Option<String>,
}

async fn handle_toggle_task(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    Json(req): Json<ToggleTaskReq>,
) -> Response {
    if let Err(r) = check_auth(&state, &headers) {
        return *r;
    }
    let vault = match resolve_req_vault(&state, req.vault.as_deref()) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let rel_path = match state.storage.resolve_note_path(&vault.id, &req.note) {
        Ok(p) => p,
        Err(e) => return err_resp(StatusCode::NOT_FOUND, &e.to_string()),
    };
    let abs = match safe_vault_path(&vault.path, &rel_path) {
        Ok(p) => p,
        Err(e) => return err_resp(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    if let Some(expected_mtime) = req.expected_mtime_ms {
        let (actual_mtime, _) = file_mtime_and_size(&abs);
        if actual_mtime != expected_mtime {
            return err_resp(
                StatusCode::CONFLICT,
                &format!(
                    "Task conflict: note '{rel_path}' modified externally (expected mtime_ms {expected_mtime}, found {actual_mtime})"
                ),
            );
        }
    }
    match toggle_task_at_line_verified(
        &abs,
        req.line,
        req.expected_text.as_deref(),
        req.expected_checked,
    ) {
        Ok(checked) => {
            let _ = state
                .storage
                .sync_paths(&vault, std::slice::from_ref(&rel_path), &[]);
            Json(json!({
                "ok": true,
                "rel_path": rel_path,
                "line": req.line,
                "checked": checked,
            }))
            .into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.starts_with("Task conflict") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            err_resp(status, &msg)
        }
    }
}

async fn handle_reindex(
    State(state): State<Arc<ServeState>>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    if let Err(r) = check_auth(&state, &headers) {
        return *r;
    }
    let vault_spec = body
        .as_ref()
        .and_then(|v| v.0.get("vault"))
        .and_then(|v| v.as_str());
    let vault = match resolve_req_vault_unsynced(&state, vault_spec) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match state.storage.reindex_vault(&vault) {
        Ok(stats) => Json(json!({ "ok": true, "stats": stats })).into_response(),
        Err(e) => err_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn obs01_token_is_generated_on_loopback_unless_keyless() {
        let (first, generated) = resolve_serve_token(&addr("127.0.0.1:8792"), None, false).unwrap();
        let first = first.unwrap();
        assert!(generated);
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        let (second, _) = resolve_serve_token(&addr("[::1]:8792"), None, false).unwrap();
        assert_ne!(Some(first), second);

        assert_eq!(
            resolve_serve_token(&addr("127.0.0.1:8792"), None, true).unwrap(),
            (None, false)
        );
        assert_eq!(
            resolve_serve_token(&addr("127.0.0.1:8792"), Some("mine".into()), false).unwrap(),
            (Some("mine".to_string()), false)
        );
        // A blank token counts as unset.
        let (blank, generated) =
            resolve_serve_token(&addr("127.0.0.1:8792"), Some("  ".into()), false).unwrap();
        assert!(generated && blank.is_some());
    }

    #[test]
    fn obs01_non_loopback_bind_requires_token() {
        for bind in ["0.0.0.0:8792", "192.168.1.20:8792", "[::]:8792"] {
            assert!(
                resolve_serve_token(&addr(bind), None, false).is_err(),
                "{bind}"
            );
            assert!(
                resolve_serve_token(&addr(bind), None, true).is_err(),
                "{bind}"
            );
            assert!(
                resolve_serve_token(&addr(bind), Some(" ".into()), true).is_err(),
                "{bind}"
            );
            assert_eq!(
                resolve_serve_token(&addr(bind), Some("t".into()), false).unwrap(),
                (Some("t".to_string()), false),
                "{bind}"
            );
        }
    }

    #[test]
    fn obs01_host_normalization() {
        assert_eq!(normalize_host("localhost").as_deref(), Some("localhost"));
        assert_eq!(
            normalize_host("LOCALHOST:8792").as_deref(),
            Some("localhost")
        );
        assert_eq!(normalize_host("localhost.").as_deref(), Some("localhost"));
        assert_eq!(
            normalize_host("localhost.:8792").as_deref(),
            Some("localhost")
        );
        assert_eq!(
            normalize_host("127.0.0.1:8792").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(normalize_host("[::1]:8792").as_deref(), Some("[::1]"));
        assert_eq!(
            normalize_host("[0:0:0:0:0:0:0:1]").as_deref(),
            Some("[::1]")
        );
        assert_eq!(normalize_host("::1").as_deref(), Some("[::1]"));
        for bad in [
            "",
            "  ",
            "localhost:",
            "localhost:abc",
            "[::1",
            "[::1]x",
            "a/b",
            "u@localhost",
        ] {
            assert_eq!(normalize_host(bad), None, "{bad}");
        }
    }

    #[test]
    fn obs01_origin_policy() {
        let policy = GuardPolicy::new(&ServePolicy {
            extra_hosts: vec!["192.168.1.20".into(), "bad host/".into()],
            allowed_origins: vec!["https://App.Example.com/".into(), "*".into()],
        });
        assert!(policy.hosts.contains(&"192.168.1.20".to_string()));
        assert_eq!(policy.hosts.len(), 4);
        assert_eq!(policy.origins, vec!["https://app.example.com".to_string()]);

        assert!(origin_allowed(&policy, "https://app.example.com"));
        assert!(origin_allowed(&policy, "http://localhost:3000"));
        assert!(origin_allowed(&policy, "http://127.0.0.1"));
        assert!(origin_allowed(&policy, "http://[::1]:5173"));
        for bad in [
            "null",
            "https://evil.example",
            "http://app.example.com",
            "https://app.example.com:8443",
            "http://localhost.evil.example",
            "file://localhost",
            "http://192.168.1.20",
        ] {
            assert!(!origin_allowed(&policy, bad), "{bad}");
        }
    }

    #[test]
    fn obs01_bind_address_parsing() {
        assert_eq!(
            parse_bind_addr("127.0.0.1", 8792).unwrap(),
            addr("127.0.0.1:8792")
        );
        assert_eq!(parse_bind_addr("::1", 8792).unwrap(), addr("[::1]:8792"));
        assert_eq!(parse_bind_addr("[::1]", 8792).unwrap(), addr("[::1]:8792"));
        assert!(parse_bind_addr("not-an-ip", 8792).is_err());
    }
}
