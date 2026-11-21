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

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::CorsLayer;

use crate::{
    config::{AppConfig, VaultProfile},
    models::{TaskFilter, VaultSummary},
    storage::Storage,
    vault::{
        fs::{
            append_to_note, create_note, ensure_daily_note, file_mtime_and_size, safe_vault_path,
            toggle_task_at_line_verified,
        },
        merged_vaults, resolve_active_vault,
    },
};

#[derive(Clone)]
pub struct ServeState {
    pub cfg: AppConfig,
    pub default_vault: VaultProfile,
    pub storage: Storage,
    pub token: Option<String>,
}

pub fn build_router(state: Arc<ServeState>) -> Router {
    Router::new()
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
        .route("/api/v1/reindex", post(handle_reindex))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

pub async fn run_http_server(
    cfg: AppConfig,
    vault: VaultProfile,
    storage: Storage,
    bind: &str,
    port: u16,
    token: Option<String>,
) -> Result<()> {
    let _ = storage.sync_vault(&vault);
    let state = Arc::new(ServeState {
        cfg,
        default_vault: vault.clone(),
        storage,
        token,
    });
    let app = build_router(state);
    let addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .with_context(|| format!("Invalid bind address: {bind}:{port}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind HTTP server on {addr}"))?;
    eprintln!(
        "obsidian HTTP API listening on http://{addr} (vault: {})",
        vault.display_name()
    );
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
        Some(spec) => resolve_active_vault(&state.cfg, Some(spec))
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
            let _ = state.storage.sync_vault(&vault);
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
    let _ = state.storage.sync_vault(&vault);
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
            let _ = state.storage.sync_vault(&vault);
            let detail = state.storage.load_note_detail(&vault, &rel_path).ok();
            Json(json!({
                "ok": true,
                "rel_path": rel_path,
                "created": created,
                "note": detail,
            }))
            .into_response()
        }
        Err(e) => err_resp(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
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
    match state.storage.get_graph(&vault.id, center.as_deref(), depth) {
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
            let _ = state.storage.sync_vault(&vault);
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
