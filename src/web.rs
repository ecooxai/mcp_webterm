use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{Read, Write},
    net::IpAddr,
    net::Shutdown,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;

use crate::{
    VERSION,
    config::Config,
    db::{Database, Terminal, Workspace},
    metrics::MetricsSampler,
    runtime::{RuntimeEvent, Subscription},
    terminal::TerminalManager,
};

const SESSION_COOKIE: &str = "__Host-webterm_session";
const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const LOGIN_LIMIT_PER_CLIENT: usize = 5;
const LOGIN_LIMIT_GLOBAL: usize = 50;
const WS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(25);
const WS_IO_TIMEOUT: Duration = Duration::from_secs(10);
const WS_QUEUE_CAPACITY: usize = 8;

#[derive(Clone)]
pub struct AppState {
    config: Arc<Config>,
    browser_auth: Arc<BrowserAuth>,
    metrics: Arc<MetricsSampler>,
    processes: Arc<crate::processes::ProcessMonitor>,
}

struct BrowserAuth {
    password_hash: Option<String>,
    sessions: Mutex<HashMap<String, BrowserSession>>,
    limiter: Mutex<LoginLimiter>,
    ttl: Duration,
}

struct BrowserSession {
    csrf_token: String,
    expires_at: Instant,
}

#[derive(Default)]
struct LoginLimiter {
    clients: HashMap<String, VecDeque<Instant>>,
    global: VecDeque<Instant>,
}

pub fn router(config: Config) -> Result<Router> {
    let password_hash = if let Some(password) = config.web_password.as_deref() {
        if password.len() > 1024 {
            bail!("web password must be at most 1024 bytes")
        }
        Some(hash_password(password)?)
    } else {
        config
            .web_password_hash_file
            .as_deref()
            .map(|path| {
                fs::read_to_string(path)
                    .with_context(|| format!("read web password hash {}", path.display()))
            })
            .transpose()?
            .map(|hash| hash.trim().to_owned())
    };
    if let Some(hash) = &password_hash {
        PasswordHash::new(hash)
            .map_err(|error| anyhow::anyhow!("parse web password hash: {error}"))?;
    }
    let ttl = Duration::from_secs(config.web_session_ttl_seconds);
    Ok(router_with_auth(config, password_hash, ttl))
}

fn router_with_auth(config: Config, password_hash: Option<String>, ttl: Duration) -> Router {
    let mcp_router = crate::mcp::router(config.clone());
    let state = AppState {
        config: Arc::new(config),
        browser_auth: Arc::new(BrowserAuth {
            password_hash,
            sessions: Mutex::new(HashMap::new()),
            limiter: Mutex::new(LoginLimiter::default()),
            ttl,
        }),
        metrics: Arc::new(MetricsSampler::new()),
        processes: Arc::new(crate::processes::ProcessMonitor::default()),
    };
    Router::new()
        .route("/", get(index))
        .route("/log", get(index))
        .route("/assets/explorer.js", get(explorer_js))
        .route("/assets/explorer.css", get(explorer_css))
        .route("/assets/tool-log.js", get(logs_js))
        .route("/assets/terminal-links.js", get(terminal_links_js))
        .route("/assets/workspace-tools.js", get(workspace_tools_js))
        .route("/assets/model-viewer.min.js", get(model_viewer_js))
        .route("/api/v1/files/list", get(explorer_list))
        .route("/api/v1/files/preview", get(explorer_preview))
        .route("/api/v1/files/resolve", get(explorer_resolve))
        .route("/api/v1/workspace-activity", get(workspace_activity))
        .route("/api/v1/git/status", get(workspace_git_status))
        .route("/api/v1/git/diff", get(workspace_git_diff))
        .route("/api/v1/files/raw/{workspace}/{*path}", get(explorer_raw))
        .route("/api/v1/tool-logs", get(browser_tool_logs))
        .route("/api/v1/tool-logs/{id}", get(browser_tool_log_detail))
        .route("/assets/app.js", get(app_js))
        .route("/assets/app.css", get(app_css))
        .route("/assets/process-monitor.js", get(monitor_js))
        .route("/assets/process-monitor.css", get(monitor_css))
        .route("/assets/vendor/{file}", get(vendor_asset))
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/api/v1/session", get(auth_session))
        .route("/api/v1/login", post(login))
        .route("/api/v1/logout", post(logout))
        .route("/api/v1/metrics", get(browser_metrics))
        .route("/api/v1/processes", get(browser_processes))
        .route("/api/v1/processes/{pid}", get(browser_process_detail))
        .route(
            "/api/v1/processes/{pid}/action",
            post(browser_process_action),
        )
        .route("/proxy/{port}", axum::routing::any(browser_port_proxy))
        .route("/proxy/{port}/", axum::routing::any(browser_port_proxy))
        .route(
            "/proxy/{port}/{*tail}",
            axum::routing::any(browser_port_proxy),
        )
        .route(
            "/api/v1/workspaces",
            get(browser_workspaces).post(create_workspace),
        )
        .route("/api/v1/folders", get(browser_folders))
        .route(
            "/api/v1/workspaces/{id}",
            axum::routing::patch(update_workspace).delete(delete_workspace),
        )
        .route("/api/v1/workspaces/{id}/terminals", post(create_terminal))
        .route(
            "/api/v1/workspaces/{id}/ensure-terminal",
            post(ensure_workspace_terminal),
        )
        .route(
            "/api/v1/terminals/{id}",
            axum::routing::patch(update_terminal).delete(delete_terminal),
        )
        .route("/api/v1/terminals/{id}/ws", get(terminal_websocket))
        .route("/api/v1/status", get(bearer_status))
        .route("/api/v1/preview-ports", get(preview_ports))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(state.clone())
        .merge(mcp_router)
        .layer(axum::middleware::from_fn_with_state(
            state,
            query_port_dispatch,
        ))
}

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_CSS: &str = include_str!("../web/app.css");
const APP_JS: &str = include_str!("../web/app.js");
const XTERM_CSS: &str = include_str!("../web/vendor/xterm-5.5.0.css");
const XTERM_JS: &str = include_str!("../web/vendor/xterm-5.5.0.js");
const FIT_JS: &str = include_str!("../web/vendor/addon-fit-0.10.0.js");

#[derive(Default, Deserialize)]
struct IndexQuery {
    passwd: Option<String>,
}

async fn index(
    State(state): State<AppState>,
    Query(query): Query<IndexQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(password) = query.passwd else {
        return html_response(INDEX_HTML, "text/html; charset=utf-8");
    };

    if let Some(response) = verify_browser_password(&state, &headers, password).await {
        return response;
    }

    let (session_id, _) = create_browser_session(&state);
    let mut response = StatusCode::SEE_OTHER.into_response();
    response
        .headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/"));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    set_browser_session_cookie(&state, &session_id, &mut response);
    response
}

async fn app_js() -> Response {
    html_response(APP_JS, "text/javascript; charset=utf-8")
}

async fn app_css() -> Response {
    html_response(APP_CSS, "text/css; charset=utf-8")
}

async fn vendor_asset(Path(file): Path<String>) -> Response {
    match file.as_str() {
        "xterm-5.5.0.css" => html_response(XTERM_CSS, "text/css; charset=utf-8"),
        "xterm-5.5.0.js" => html_response(XTERM_JS, "text/javascript; charset=utf-8"),
        "addon-fit-0.10.0.js" => html_response(FIT_JS, "text/javascript; charset=utf-8"),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn html_response(body: &'static str, content_type: &'static str) -> Response {
    let mut response = body.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert("content-security-policy",HeaderValue::from_static("default-src 'self'; connect-src 'self' wss: blob:; img-src 'self' data: blob:; media-src 'self' blob:; frame-src 'self' blob: http: https:; worker-src 'self' blob:; style-src 'self' 'unsafe-inline'; script-src 'self' 'wasm-unsafe-eval'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

#[derive(Serialize)]
struct Health<'a> {
    status: &'a str,
}

async fn health() -> Json<Health<'static>> {
    Json(Health { status: "ok" })
}

#[derive(Serialize)]
struct Version<'a> {
    name: &'a str,
    version: &'a str,
    api_revision: &'a str,
}

async fn version() -> Json<Version<'static>> {
    Json(Version {
        name: "webterm",
        version: VERSION,
        api_revision: "subdomain-image-v5",
    })
}

async fn browser_metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if current_session(&state, &headers).is_none() {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let sampler = state.metrics.clone();
    match tokio::task::spawn_blocking(move || sampler.sample()).await {
        Ok(metrics) => Json(metrics).into_response(),
        Err(error) => {
            tracing::error!(error = %error, "system metrics sampling task failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to sample system metrics",
            )
                .into_response()
        }
    }
}

async fn bearer_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(expected) = state.config.auth_token.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "bearer authentication is not configured",
        )
            .into_response();
    };
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let authorized = supplied
        .map(|value| value.as_bytes().ct_eq(expected.as_bytes()).into())
        .unwrap_or(false);
    if !authorized {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    Json(Health { status: "ok" }).into_response()
}

#[derive(Serialize)]
struct SessionResponse {
    authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    csrf_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_in_seconds: Option<u64>,
    capabilities: Capabilities,
}

#[derive(Serialize)]
struct Capabilities {
    workspace_crud: bool,
    terminal_crud: bool,
    terminal_websocket: bool,
}

fn capabilities() -> Capabilities {
    Capabilities {
        workspace_crud: true,
        terminal_crud: true,
        terminal_websocket: true,
    }
}

async fn auth_session(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match current_session(&state, &headers) {
        Some((_, csrf_token, expires_in)) => Json(SessionResponse {
            authenticated: true,
            csrf_token: Some(csrf_token),
            expires_in_seconds: Some(expires_in),
            capabilities: capabilities(),
        })
        .into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            Json(SessionResponse {
                authenticated: false,
                csrf_token: None,
                expires_in_seconds: None,
                capabilities: capabilities(),
            }),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct LoginRequest {
    password: String,
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Response {
    if !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "same-origin request required").into_response();
    }
    if let Some(response) = verify_browser_password(&state, &headers, request.password).await {
        return response;
    }

    let (session_id, csrf_token) = create_browser_session(&state);
    let mut response = Json(SessionResponse {
        authenticated: true,
        csrf_token: Some(csrf_token),
        expires_in_seconds: Some(state.browser_auth.ttl.as_secs()),
        capabilities: capabilities(),
    })
    .into_response();
    set_browser_session_cookie(&state, &session_id, &mut response);
    response
}

async fn verify_browser_password(
    state: &AppState,
    headers: &HeaderMap,
    password: String,
) -> Option<Response> {
    if password.is_empty() || password.len() > 1024 {
        release_password_memory();
        return Some((StatusCode::UNAUTHORIZED, "invalid credentials").into_response());
    }

    let client = client_key(headers);
    if !lock(&state.browser_auth.limiter).allow(&client) {
        release_password_memory();
        let mut response =
            (StatusCode::TOO_MANY_REQUESTS, "too many login attempts").into_response();
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
        return Some(response);
    }

    let Some(hash) = state.browser_auth.password_hash.clone() else {
        release_password_memory();
        return Some(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "browser authentication is not configured",
            )
                .into_response(),
        );
    };

    let verified = tokio::task::spawn_blocking(move || {
        let verified = PasswordHash::new(&hash).ok().is_some_and(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        });
        release_password_memory();
        verified
    })
    .await
    .unwrap_or(false);
    if !verified {
        return Some((StatusCode::UNAUTHORIZED, "invalid credentials").into_response());
    }

    lock(&state.browser_auth.limiter).clear_client(&client);
    None
}

fn create_browser_session(state: &AppState) -> (String, String) {
    let session_id = uuid::Uuid::new_v4().simple().to_string();
    let csrf_token = uuid::Uuid::new_v4().simple().to_string();
    lock(&state.browser_auth.sessions).insert(
        session_id.clone(),
        BrowserSession {
            csrf_token: csrf_token.clone(),
            expires_at: Instant::now() + state.browser_auth.ttl,
        },
    );
    (session_id, csrf_token)
}

fn set_browser_session_cookie(state: &AppState, session_id: &str, response: &mut Response) {
    let cookie = format!(
        "{SESSION_COOKIE}={session_id}; Path=/; Max-Age={}; Secure; HttpOnly; SameSite=Strict",
        state.browser_auth.ttl.as_secs()
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("session cookie contains safe characters"),
    );
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "same-origin request required").into_response();
    }
    let Some((session_id, csrf_token, _)) = current_session(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    };
    if !valid_csrf(&headers, &csrf_token) {
        return (StatusCode::FORBIDDEN, "invalid CSRF token").into_response();
    }
    lock(&state.browser_auth.sessions).remove(&session_id);
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "__Host-webterm_session=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Strict",
        ),
    );
    response
}

#[derive(Serialize)]
struct WorkspaceTree {
    id: i64,
    name: String,
    path: std::path::PathBuf,
    terminals: Vec<BrowserTerminal>,
}

#[derive(Serialize)]
struct BrowserTerminal {
    id: i64,
    workspace_id: i64,
    name: String,
    session_id: String,
    status: String,
    backend: &'static str,
}

#[derive(Serialize)]
struct WorkspaceList {
    workspaces: Vec<WorkspaceTree>,
}

const MAX_FOLDER_ENTRIES: usize = 500;

#[derive(Default, Deserialize)]
struct FolderQuery {
    path: Option<PathBuf>,
}

#[derive(Serialize)]
struct FolderEntry {
    name: String,
    path: String,
}

#[derive(Serialize)]
struct FolderListing {
    current: String,
    selected_name: String,
    parent: Option<String>,
    entries: Vec<FolderEntry>,
    truncated: bool,
}

async fn browser_folders(
    State(state): State<AppState>,
    Query(query): Query<FolderQuery>,
    headers: HeaderMap,
) -> Response {
    if current_session(&state, &headers).is_none() {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let config = (*state.config).clone();
    api_task(
        move || folder_listing(&config, query.path.as_deref()),
        StatusCode::OK,
    )
    .await
}

fn folder_listing(config: &Config, requested: Option<&FsPath>) -> Result<FolderListing> {
    crate::db::ensure_default_workspace_folder(config)?;
    let roots = crate::db::canonical_workspace_roots(config)?;
    let preferred = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("project"));
    let requested = requested
        .map(PathBuf::from)
        .or_else(|| {
            preferred.filter(|path| {
                path.canonicalize()
                    .ok()
                    .is_some_and(|path| roots.iter().any(|root| path.starts_with(root)))
            })
        })
        .unwrap_or_else(|| roots[0].clone());
    let current = crate::db::canonical_workspace_path(config, &requested)?;
    let current_text = utf8_path(&current)?.to_owned();
    let selected_name = current
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace")
        .to_owned();
    let parent = current
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .filter(|parent| roots.iter().any(|root| parent.starts_with(root)))
        .map(|parent| utf8_path(&parent).map(str::to_owned))
        .transpose()?;

    let iterator = fs::read_dir(&current).with_context(|| {
        format!(
            "read folder {} (check directory permissions)",
            current.display()
        )
    })?;
    let mut entries = Vec::new();
    for item in iterator {
        let Ok(item) = item else { continue };
        let Ok(canonical) = item.path().canonicalize() else {
            continue;
        };
        if !canonical.is_dir() || !roots.iter().any(|root| canonical.starts_with(root)) {
            continue;
        }
        let Some(name) = item.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(path) = canonical.to_str().map(str::to_owned) else {
            continue;
        };
        entries.push(FolderEntry { name, path });
    }
    entries.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.path.cmp(&right.path))
    });
    let truncated = entries.len() > MAX_FOLDER_ENTRIES;
    entries.truncate(MAX_FOLDER_ENTRIES);
    Ok(FolderListing {
        current: current_text,
        selected_name,
        parent,
        entries,
        truncated,
    })
}

fn utf8_path(path: &FsPath) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("folder path is not valid UTF-8: {}", path.display()))
}

async fn browser_workspaces(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if current_session(&state, &headers).is_none() {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let config = (*state.config).clone();
    match tokio::task::spawn_blocking(move || load_workspace_tree(&config)).await {
        Ok(Ok(workspaces)) => Json(WorkspaceList { workspaces }).into_response(),
        Ok(Err(error)) => {
            tracing::error!(error = %error, "load browser workspace list");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to load workspaces",
            )
                .into_response()
        }
        Err(error) => {
            tracing::error!(error = %error, "workspace list task failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to load workspaces",
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
struct CreateWorkspaceRequest {
    name: String,
    path: PathBuf,
}

#[derive(Deserialize)]
struct UpdateWorkspaceRequest {
    name: Option<String>,
    path: Option<PathBuf>,
}

#[derive(Serialize)]
struct WorkspaceResponse {
    workspace: WorkspaceTree,
}

async fn create_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateWorkspaceRequest>,
) -> Response {
    if let Some(response) = mutation_rejection(&state, &headers) {
        return response;
    }
    let config = (*state.config).clone();
    api_task(
        move || {
            let path = crate::db::canonical_workspace_path(&config, &request.path)?;
            let workspace =
                Database::open_config(&config)?.create_workspace(&request.name, &path)?;
            Ok(WorkspaceResponse {
                workspace: workspace_tree(workspace, Vec::new()),
            })
        },
        StatusCode::CREATED,
    )
    .await
}

async fn update_workspace(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(request): Json<UpdateWorkspaceRequest>,
) -> Response {
    if let Some(response) = mutation_rejection(&state, &headers) {
        return response;
    }
    let config = (*state.config).clone();
    api_task(
        move || {
            let path = request
                .path
                .as_deref()
                .map(|path| crate::db::canonical_workspace_path(&config, path))
                .transpose()?;
            let database = Database::open_config(&config)?;
            let workspace = database.update_workspace(
                &id.to_string(),
                request.name.as_deref(),
                path.as_deref(),
            )?;
            let terminals = database.list_terminals(Some(id))?;
            Ok(WorkspaceResponse {
                workspace: workspace_tree(workspace, terminals),
            })
        },
        StatusCode::OK,
    )
    .await
}

#[derive(Default, Deserialize)]
struct DeleteWorkspaceQuery {
    #[serde(default)]
    force: bool,
}

async fn delete_workspace(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<DeleteWorkspaceQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = mutation_rejection(&state, &headers) {
        return response;
    }
    let config = (*state.config).clone();
    empty_api_task(move || {
        let database = Database::open_config(&config)?;
        let records = database.list_terminals(Some(id))?;
        if !records.is_empty() && !query.force {
            bail!("workspace has terminal records; pass force=true to remove them")
        }
        if query.force {
            let manager = TerminalManager::new(&config)?;
            for terminal in records {
                manager.stop(&terminal.tmux_session)?;
                database.delete_terminal(terminal.id)?;
            }
        }
        database.remove_workspace(&id.to_string())?;
        Ok(())
    })
    .await
}

#[derive(Default, Deserialize)]
struct CreateTerminalRequest {
    name: Option<String>,
    #[serde(default = "default_columns")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}

fn default_columns() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

#[derive(Deserialize)]
struct UpdateTerminalRequest {
    name: String,
}

#[derive(Serialize)]
struct TerminalResponse {
    terminal: BrowserTerminal,
}

#[derive(Serialize)]
struct EnsureTerminalResponse {
    terminal: BrowserTerminal,
    created: bool,
    restarted: bool,
}

const STARTING_GRACE_SECONDS: i64 = 10;

async fn ensure_workspace_terminal(
    State(state): State<AppState>,
    Path(workspace_id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = mutation_rejection(&state, &headers) {
        return response;
    }
    let config = (*state.config).clone();
    api_task(
        move || ensure_terminal_for_workspace(&config, workspace_id),
        StatusCode::OK,
    )
    .await
}

fn ensure_terminal_for_workspace(
    config: &Config,
    workspace_id: i64,
) -> Result<EnsureTerminalResponse> {
    let mut database = Database::open_config(config)?;
    let terminals = TerminalManager::new(config)?;
    let now = unix_timestamp()?;
    for terminal in database.list_terminals(Some(workspace_id))? {
        if terminal.status == "running" {
            if terminals.has_session(&terminal.tmux_session)? {
                return Ok(EnsureTerminalResponse {
                    terminal: browser_terminal(&terminal),
                    created: false,
                    restarted: false,
                });
            }
            database.set_terminal_status(terminal.id, "stopped")?;
        } else if terminal.status == "starting" {
            if terminals.has_session(&terminal.tmux_session)? {
                let terminal = database.set_terminal_status(terminal.id, "running")?;
                return Ok(EnsureTerminalResponse {
                    terminal: browser_terminal(&terminal),
                    created: false,
                    restarted: false,
                });
            }
            if now.saturating_sub(terminal.updated_at) < STARTING_GRACE_SECONDS {
                return wait_for_starting_terminal(&database, &terminals, terminal);
            }
            database.set_terminal_status(terminal.id, "stopped")?;
        }
    }

    let reservation = database.reserve_fallback_terminal(workspace_id)?;
    if !reservation.should_start {
        if reservation.terminal.status == "running" {
            return Ok(EnsureTerminalResponse {
                terminal: browser_terminal(&reservation.terminal),
                created: false,
                restarted: false,
            });
        }
        return wait_for_starting_terminal(&database, &terminals, reservation.terminal);
    }

    if let Err(error) = terminals.create(
        &reservation.terminal.tmux_session,
        &database.workspace_by_id(workspace_id)?.path,
    ) && !terminals.has_session(&reservation.terminal.tmux_session)?
    {
        database.set_terminal_status(reservation.terminal.id, "stopped")?;
        return Err(error).context("start fallback terminal term1");
    }
    if let Err(error) = terminals.resize(&reservation.terminal.tmux_session, 80, 24) {
        terminals.stop(&reservation.terminal.tmux_session)?;
        database.set_terminal_status(reservation.terminal.id, "stopped")?;
        return Err(error).context("resize fallback terminal term1");
    }
    let terminal = database.set_terminal_status(reservation.terminal.id, "running")?;
    Ok(EnsureTerminalResponse {
        terminal: browser_terminal(&terminal),
        created: !reservation.restarted,
        restarted: reservation.restarted,
    })
}

fn wait_for_starting_terminal(
    database: &Database,
    terminals: &TerminalManager,
    terminal: Terminal,
) -> Result<EnsureTerminalResponse> {
    for _ in 0..40 {
        if terminals.has_session(&terminal.tmux_session)? {
            let terminal = database.set_terminal_status(terminal.id, "running")?;
            return Ok(EnsureTerminalResponse {
                terminal: browser_terminal(&terminal),
                created: false,
                restarted: false,
            });
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    bail!("terminal term1 is still starting; retry in a moment")
}

fn unix_timestamp() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs() as i64)
}

async fn create_terminal(
    State(state): State<AppState>,
    Path(workspace_id): Path<i64>,
    headers: HeaderMap,
    Json(request): Json<CreateTerminalRequest>,
) -> Response {
    if let Some(response) = mutation_rejection(&state, &headers) {
        return response;
    }
    if !(2..=crate::runtime::MAX_COLS).contains(&request.cols)
        || !(2..=crate::runtime::MAX_ROWS).contains(&request.rows)
    {
        return (
            StatusCode::BAD_REQUEST,
            "native dimensions must be 2..512 columns and 2..256 rows",
        )
            .into_response();
    }
    let config = (*state.config).clone();
    api_task(
        move || {
            let mut database = Database::open_config(&config)?;
            let workspace = database.workspace_by_id(workspace_id)?;
            let terminal = match request.name.as_deref() {
                Some(name) => database.reserve_terminal(workspace.id, name)?,
                None => database.reserve_default_terminal(workspace.id)?,
            };
            let terminals = TerminalManager::new(&config)?;
            if let Err(error) = terminals.create(&terminal.tmux_session, &workspace.path) {
                database.delete_terminal(terminal.id)?;
                return Err(error);
            }
            if let Err(error) = terminals.resize(&terminal.tmux_session, request.cols, request.rows)
            {
                terminals.stop(&terminal.tmux_session)?;
                database.delete_terminal(terminal.id)?;
                return Err(error);
            }
            let terminal = database.set_terminal_status(terminal.id, "running")?;
            Ok(TerminalResponse {
                terminal: browser_terminal(&terminal),
            })
        },
        StatusCode::CREATED,
    )
    .await
}

async fn update_terminal(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(request): Json<UpdateTerminalRequest>,
) -> Response {
    if let Some(response) = mutation_rejection(&state, &headers) {
        return response;
    }
    let config = (*state.config).clone();
    api_task(
        move || {
            let terminal = Database::open_config(&config)?.rename_terminal(id, &request.name)?;
            Ok(TerminalResponse {
                terminal: browser_terminal(&terminal),
            })
        },
        StatusCode::OK,
    )
    .await
}

async fn delete_terminal(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = mutation_rejection(&state, &headers) {
        return response;
    }
    let config = (*state.config).clone();
    empty_api_task(move || {
        let database = Database::open_config(&config)?;
        let terminal = database.terminal_by_id(id)?;
        TerminalManager::new(&config)?.stop(&terminal.tmux_session)?;
        database.delete_terminal(id)
    })
    .await
}

async fn terminal_websocket(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "same-origin request required").into_response();
    }
    let Some((_, _, expires_in)) = current_session(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    };
    let config = (*state.config).clone();
    let prepared = tokio::task::spawn_blocking({
        let config = config.clone();
        move || -> Result<PreparedTerminalSocket> {
            let terminal = Database::open_config(&config)?.terminal_by_id(id)?;
            let manager = TerminalManager::new(&config)?;
            if !manager.has_session(&terminal.tmux_session)? {
                bail!("terminal is not running")
            }
            if terminal_backend(&terminal.tmux_session) == "native-pty" {
                let subscription = manager.native().subscribe(&terminal.tmux_session)?;
                Ok(PreparedTerminalSocket::Native {
                    terminal,
                    subscription,
                })
            } else {
                manager.follow_client_size(&terminal.tmux_session)?;
                let snapshot = manager.capture(&terminal.tmux_session, 100)?;
                Ok(PreparedTerminalSocket::Legacy { terminal, snapshot })
            }
        }
    })
    .await;
    let prepared = match prepared {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => return api_error(error),
        Err(error) => {
            tracing::error!(error = %error, "prepare terminal websocket task failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "terminal unavailable").into_response();
        }
    };
    ws.max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |socket| async move {
            match prepared {
                PreparedTerminalSocket::Native {
                    terminal,
                    subscription,
                } => {
                    run_native_terminal_socket(socket, config, terminal, subscription, expires_in)
                        .await
                }
                PreparedTerminalSocket::Legacy { terminal, snapshot } => {
                    run_legacy_terminal_socket(socket, config, terminal, snapshot, expires_in).await
                }
            }
        })
}

enum PreparedTerminalSocket {
    Native {
        terminal: Terminal,
        subscription: Subscription,
    },
    Legacy {
        terminal: Terminal,
        snapshot: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientTerminalMessage {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

enum NativeTerminalCommand {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

async fn run_native_terminal_socket(
    mut socket: WebSocket,
    config: Config,
    terminal: Terminal,
    subscription: Subscription,
    expires_in: u64,
) {
    if let Err(error) =
        native_terminal_socket_inner(&mut socket, &config, &terminal, subscription, expires_in)
            .await
    {
        tracing::warn!(terminal_id = terminal.id, error = %error, "native terminal websocket closed with error");
        let _ = send_ws_event(
            &mut socket,
            "error",
            serde_json::json!({ "message": error.to_string() }),
        )
        .await;
    }
}

async fn native_terminal_socket_inner(
    socket: &mut WebSocket,
    config: &Config,
    terminal: &Terminal,
    mut subscription: Subscription,
    expires_in: u64,
) -> Result<()> {
    let manager = TerminalManager::new(config)?;
    let initial_cols = subscription.cols;
    let initial_rows = subscription.rows;
    let initial_snapshot = std::mem::take(&mut subscription.snapshot);
    let shutdown_stream = subscription
        .stream()
        .try_clone()
        .context("clone runtime subscription transport")?;

    let (event_tx, mut event_rx) = mpsc::channel::<Result<RuntimeEvent, String>>(WS_QUEUE_CAPACITY);
    let mut reader_task = tokio::task::spawn_blocking(move || -> Result<()> {
        loop {
            let (event, finished) = match subscription.read_event() {
                Ok(Some(event)) => (Ok(event), false),
                Ok(None) => break,
                Err(error) => (Err(error.to_string()), true),
            };
            match event_tx.try_send(event) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => break,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    bail!("runtime output queue is full; closing slow websocket subscriber")
                }
            }
            if finished {
                break;
            }
        }
        Ok(())
    });

    let runtime_id = terminal.tmux_session.clone();
    let (command_tx, mut command_rx) = mpsc::channel::<NativeTerminalCommand>(WS_QUEUE_CAPACITY);
    let (command_error_tx, mut command_error_rx) = mpsc::channel::<String>(1);
    let mut writer_task = tokio::task::spawn_blocking(move || {
        while let Some(command) = command_rx.blocking_recv() {
            let result = match command {
                NativeTerminalCommand::Input(data) => manager.write_bytes(&runtime_id, &data),
                NativeTerminalCommand::Resize { cols, rows } => {
                    manager.resize(&runtime_id, cols, rows)
                }
            };
            if let Err(error) = result {
                let _ = command_error_tx.try_send(error.to_string());
                break;
            }
        }
    });

    let connection = async {
        send_ws_event(
            socket,
            "status",
            serde_json::json!({ "status": "connected", "backend": "native-pty" }),
        )
        .await?;
        send_ws_event(
            socket,
            "resize",
            serde_json::json!({ "cols": initial_cols, "rows": initial_rows }),
        )
        .await?;
        send_terminal_snapshot(socket, initial_snapshot).await?;

        let session_expiry = tokio::time::sleep(Duration::from_secs(expires_in));
        tokio::pin!(session_expiry);
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + WS_HEARTBEAT_INTERVAL,
            WS_HEARTBEAT_INTERVAL,
        );
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut awaiting_pong = false;

        loop {
            tokio::select! {
                _ = &mut session_expiry => {
                    send_ws_message(socket, Message::Close(Some(CloseFrame {
                        code: 4401,
                        reason: "session expired".into(),
                    })), "close expired terminal websocket").await?;
                    break;
                }
                _ = heartbeat.tick() => {
                    if awaiting_pong {
                        bail!("terminal websocket heartbeat timed out")
                    }
                    send_ws_message(
                        socket,
                        Message::Ping(Vec::from("webterm-heartbeat").into()),
                        "send terminal websocket heartbeat",
                    ).await?;
                    awaiting_pong = true;
                }
                runtime_event = event_rx.recv() => {
                    match runtime_event {
                        Some(Ok(RuntimeEvent::Output { data })) => {
                            send_ws_message(socket, Message::Binary(data.into()), "send native terminal output").await?;
                        }
                        Some(Ok(RuntimeEvent::Resize { cols, rows, snapshot })) => {
                            send_ws_event(socket, "resize", serde_json::json!({ "cols": cols, "rows": rows })).await?;
                            send_terminal_snapshot(socket, snapshot).await?;
                        }
                        Some(Ok(RuntimeEvent::Closed)) => {
                            send_ws_event(socket, "closed", serde_json::json!({})).await?;
                            break;
                        }
                        Some(Err(error)) => bail!("runtime subscription failed: {error}"),
                        None => bail!("runtime subscription closed"),
                    }
                }
                writer_error = command_error_rx.recv() => {
                    if let Some(error) = writer_error {
                        bail!("runtime input failed: {error}")
                    } else {
                        bail!("runtime input worker closed")
                    }
                }
                incoming = socket.recv() => {
                    match incoming {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<ClientTerminalMessage>(text.as_str()) {
                                Ok(ClientTerminalMessage::Input { data }) => {
                                    queue_native_command(&command_tx, NativeTerminalCommand::Input(data.into_bytes())).await?;
                                }
                                Ok(ClientTerminalMessage::Resize { cols, rows }) if cols >= 2 && rows >= 2 => {
                                    queue_native_command(&command_tx, NativeTerminalCommand::Resize { cols, rows }).await?;
                                }
                                Ok(ClientTerminalMessage::Resize { .. }) => {
                                    send_ws_event(socket, "error", serde_json::json!({ "message": "terminal dimensions must each be at least 2" })).await?;
                                }
                                Err(_) => {
                                    send_ws_event(socket, "error", serde_json::json!({ "message": "invalid terminal message" })).await?;
                                }
                            }
                        }
                        Some(Ok(Message::Binary(data))) => {
                            queue_native_command(&command_tx, NativeTerminalCommand::Input(data.to_vec())).await?;
                        }
                        Some(Ok(Message::Ping(data))) => {
                            send_ws_message(socket, Message::Pong(data), "send terminal websocket pong").await?;
                        }
                        Some(Ok(Message::Pong(_))) => awaiting_pong = false,
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(error)) => return Err(error.into()),
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    // Closing this cloned Unix stream wakes only this subscriber's blocking
    // read. It never sends a stop request to the independently-owned runtime.
    drop(command_tx);
    let _ = shutdown_stream.shutdown(Shutdown::Both);
    if tokio::time::timeout(WS_IO_TIMEOUT, &mut reader_task)
        .await
        .is_err()
    {
        reader_task.abort();
        tracing::warn!(
            terminal_id = terminal.id,
            "runtime subscription reader did not stop promptly"
        );
    }
    if tokio::time::timeout(WS_IO_TIMEOUT, &mut writer_task)
        .await
        .is_err()
    {
        writer_task.abort();
        tracing::warn!(
            terminal_id = terminal.id,
            "runtime input writer did not stop promptly"
        );
    }
    let _ = send_ws_event(
        socket,
        "status",
        serde_json::json!({ "status": "detached", "backend": "native-pty" }),
    )
    .await;
    connection
}

async fn queue_native_command(
    command_tx: &mpsc::Sender<NativeTerminalCommand>,
    command: NativeTerminalCommand,
) -> Result<()> {
    tokio::time::timeout(WS_IO_TIMEOUT, command_tx.send(command))
        .await
        .context("queue runtime command timed out")?
        .context("runtime command writer closed")
}

async fn send_terminal_snapshot(socket: &mut WebSocket, snapshot: Vec<u8>) -> Result<()> {
    send_ws_event(
        socket,
        "snapshot",
        serde_json::json!({ "data": String::from_utf8_lossy(&snapshot) }),
    )
    .await
}

struct AttachClient {
    child: Box<dyn Child + Send + Sync>,
    terminated: bool,
}

impl AttachClient {
    fn new(child: Box<dyn Child + Send + Sync>) -> Self {
        Self {
            child,
            terminated: false,
        }
    }

    fn terminate(&mut self) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            // Reap even if the child exited between try_wait and kill.
            let _ = self.child.wait();
        }
    }
}

impl Drop for AttachClient {
    fn drop(&mut self) {
        // This PID is the tmux attach client, not the tmux server or pane. A
        // failure during setup must never leave an orphan attach client behind.
        self.terminate();
    }
}

async fn run_legacy_terminal_socket(
    mut socket: WebSocket,
    config: Config,
    terminal: Terminal,
    snapshot: String,
    expires_in: u64,
) {
    if let Err(error) =
        legacy_terminal_socket_inner(&mut socket, &config, &terminal, snapshot, expires_in).await
    {
        tracing::warn!(terminal_id = terminal.id, error = %error, "terminal websocket closed with error");
        let _ = send_ws_event(
            &mut socket,
            "error",
            serde_json::json!({ "message": error.to_string() }),
        )
        .await;
    }
}

async fn legacy_terminal_socket_inner(
    socket: &mut WebSocket,
    config: &Config,
    terminal: &Terminal,
    snapshot: String,
    expires_in: u64,
) -> Result<()> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("open terminal PTY")?;
    let mut command = CommandBuilder::new("tmux");
    command.arg("-S");
    command.arg(&config.tmux_socket);
    command.arg("-N");
    command.arg("attach-session");
    command.arg("-t");
    command.arg(format!("={}", terminal.tmux_session));
    command.env("TERM", "xterm-256color");
    let mut attach_client = AttachClient::new(
        pair.slave
            .spawn_command(command)
            .context("attach tmux through PTY")?,
    );
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
    let mut writer = pair.master.take_writer().context("take PTY writer")?;
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));

    let (output_tx, mut output_rx) = mpsc::channel::<Vec<u8>>(WS_QUEUE_CAPACITY);
    std::thread::Builder::new()
        .name(format!("webterm-pty-read-{}", terminal.id))
        .spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if output_tx.blocking_send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .context("spawn PTY reader")?;

    let (input_tx, mut input_rx) = mpsc::channel::<Vec<u8>>(WS_QUEUE_CAPACITY);
    let mut writer_task = tokio::task::spawn_blocking(move || {
        while let Some(data) = input_rx.blocking_recv() {
            writer.write_all(&data)?;
            writer.flush()?;
        }
        Ok::<(), std::io::Error>(())
    });

    let connection = async {
        send_ws_event(
            socket,
            "status",
            serde_json::json!({ "status": "connected" }),
        )
        .await?;
        send_ws_event(socket, "snapshot", serde_json::json!({ "data": snapshot })).await?;
        let session_expiry = tokio::time::sleep(Duration::from_secs(expires_in));
        tokio::pin!(session_expiry);
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + WS_HEARTBEAT_INTERVAL,
            WS_HEARTBEAT_INTERVAL,
        );
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut awaiting_pong = false;

        loop {
            tokio::select! {
                _ = &mut session_expiry => {
                    send_ws_message(socket, Message::Close(Some(CloseFrame {
                        code: 4401,
                        reason: "session expired".into(),
                    })), "close expired terminal websocket").await?;
                    break;
                }
                _ = heartbeat.tick() => {
                    if awaiting_pong {
                        bail!("terminal websocket heartbeat timed out")
                    }
                    send_ws_message(
                        socket,
                        Message::Ping(Vec::from("webterm-heartbeat").into()),
                        "send terminal websocket heartbeat",
                    )
                    .await?;
                    awaiting_pong = true;
                }
                output = output_rx.recv() => {
                    let Some(output) = output else { break };
                    // Preserve arbitrary terminal byte streams, including UTF-8 code points
                    // split across PTY reads. The browser incrementally decodes binary frames.
                    send_ws_message(
                        socket,
                        Message::Binary(output.into()),
                        "send terminal websocket output",
                    )
                    .await?;
                }
                incoming = socket.recv() => {
                    match incoming {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<ClientTerminalMessage>(text.as_str()) {
                                Ok(ClientTerminalMessage::Input { data }) => {
                                    queue_pty_input(&input_tx, data.into_bytes()).await?;
                                }
                                Ok(ClientTerminalMessage::Resize { cols, rows }) if cols >= 2 && rows >= 2 => {
                                    lock(&master).resize(PtySize {
                                        rows,
                                        cols,
                                        pixel_width: 0,
                                        pixel_height: 0,
                                    }).context("resize terminal PTY")?;
                                }
                                Ok(ClientTerminalMessage::Resize { .. }) => {
                                    send_ws_event(socket, "error", serde_json::json!({ "message": "terminal dimensions must each be at least 2" })).await?;
                                }
                                Err(_) => {
                                    send_ws_event(socket, "error", serde_json::json!({ "message": "invalid terminal message" })).await?;
                                }
                            }
                        }
                        Some(Ok(Message::Binary(data))) => {
                            queue_pty_input(&input_tx, data.to_vec()).await?;
                        }
                        Some(Ok(Message::Ping(data))) => {
                            send_ws_message(socket, Message::Pong(data), "send terminal websocket pong").await?;
                        }
                        Some(Ok(Message::Pong(_))) => awaiting_pong = false,
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(error)) => return Err(error.into()),
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    // Always tear down the tmux attach client, even when socket I/O fails. This
    // does not stop the tmux session or its shell, so reconnection is durable.
    drop(input_tx);
    attach_client.terminate();
    if tokio::time::timeout(WS_IO_TIMEOUT, &mut writer_task)
        .await
        .is_err()
    {
        writer_task.abort();
        tracing::warn!(
            terminal_id = terminal.id,
            "PTY writer did not stop promptly"
        );
    }
    let _ = send_ws_event(
        socket,
        "status",
        serde_json::json!({ "status": "detached" }),
    )
    .await;
    connection
}

async fn queue_pty_input(input_tx: &mpsc::Sender<Vec<u8>>, data: Vec<u8>) -> Result<()> {
    tokio::time::timeout(WS_IO_TIMEOUT, input_tx.send(data))
        .await
        .context("queue terminal input timed out")?
        .context("PTY input closed")
}

async fn send_ws_message(
    socket: &mut WebSocket,
    message: Message,
    action: &'static str,
) -> Result<()> {
    tokio::time::timeout(WS_IO_TIMEOUT, socket.send(message))
        .await
        .with_context(|| format!("{action} timed out"))?
        .context(action)
}

async fn send_ws_event(
    socket: &mut WebSocket,
    kind: &str,
    fields: serde_json::Value,
) -> Result<()> {
    let mut object = fields.as_object().cloned().unwrap_or_default();
    object.insert("type".into(), serde_json::Value::String(kind.into()));
    send_ws_message(
        socket,
        Message::Text(serde_json::to_string(&object)?.into()),
        "send terminal websocket event",
    )
    .await
}

fn mutation_rejection(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if !same_origin(headers) {
        return Some((StatusCode::FORBIDDEN, "same-origin request required").into_response());
    }
    let Some((_, csrf_token, _)) = current_session(state, headers) else {
        return Some((StatusCode::UNAUTHORIZED, "unauthorized").into_response());
    };
    if !valid_csrf(headers, &csrf_token) {
        return Some((StatusCode::FORBIDDEN, "invalid CSRF token").into_response());
    }
    None
}

async fn api_task<T, F>(task: F, success: StatusCode) -> Response
where
    T: Serialize + Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(task).await {
        Ok(Ok(value)) => (success, Json(value)).into_response(),
        Ok(Err(error)) => api_error(error),
        Err(error) => {
            tracing::error!(error = %error, "browser API task failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "request failed").into_response()
        }
    }
}

async fn empty_api_task<F>(task: F) -> Response
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    match tokio::task::spawn_blocking(task).await {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => api_error(error),
        Err(error) => {
            tracing::error!(error = %error, "browser API task failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "request failed").into_response()
        }
    }
}

fn api_error(error: anyhow::Error) -> Response {
    tracing::warn!(error = %error, "browser API request rejected");
    (StatusCode::BAD_REQUEST, error.to_string()).into_response()
}

fn load_workspace_tree(config: &Config) -> Result<Vec<WorkspaceTree>> {
    let database = Database::open_config(config)?;
    let manager = TerminalManager::new(config)?;
    let mut terminals = database.list_terminals(None)?;
    for terminal in &mut terminals {
        let status = if manager.has_session(&terminal.tmux_session)? {
            "running"
        } else if terminal.status == "starting"
            && unix_timestamp()?.saturating_sub(terminal.updated_at) < STARTING_GRACE_SECONDS
        {
            "starting"
        } else {
            "stopped"
        };
        if terminal.status != status {
            *terminal = database.set_terminal_status(terminal.id, status)?;
        }
    }
    Ok(database
        .list_workspaces()?
        .into_iter()
        .map(|workspace: Workspace| {
            let children = terminals
                .iter()
                .filter(|terminal| terminal.workspace_id == workspace.id)
                .cloned()
                .collect();
            workspace_tree(workspace, children)
        })
        .collect())
}

fn workspace_tree(workspace: Workspace, terminals: Vec<Terminal>) -> WorkspaceTree {
    WorkspaceTree {
        id: workspace.id,
        name: workspace.name,
        path: workspace.path,
        terminals: terminals.iter().map(browser_terminal).collect(),
    }
}

fn browser_terminal(terminal: &Terminal) -> BrowserTerminal {
    BrowserTerminal {
        id: terminal.id,
        workspace_id: terminal.workspace_id,
        name: terminal.name.clone(),
        session_id: terminal.tmux_session.clone(),
        status: terminal.status.clone(),
        backend: terminal_backend(&terminal.tmux_session),
    }
}

fn terminal_backend(runtime_id: &str) -> &'static str {
    if runtime_id.starts_with("pty-") {
        "native-pty"
    } else {
        "legacy-tmux"
    }
}

fn current_session(state: &AppState, headers: &HeaderMap) -> Option<(String, String, u64)> {
    let session_id = cookie_value(headers, SESSION_COOKIE)?;
    let now = Instant::now();
    let mut sessions = lock(&state.browser_auth.sessions);
    sessions.retain(|_, session| session.expires_at > now);
    let session = sessions.get(&session_id)?;
    Some((
        session_id,
        session.csrf_token.clone(),
        session.expires_at.saturating_duration_since(now).as_secs(),
    ))
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then(|| value.to_owned()))
}

fn valid_csrf(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.as_bytes().ct_eq(expected.as_bytes()).into())
        .unwrap_or(false)
}

fn same_origin(headers: &HeaderMap) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    origin
        .parse::<Uri>()
        .ok()
        .and_then(|uri| {
            uri.authority()
                .map(|authority| authority.as_str().to_owned())
        })
        .is_some_and(|authority| authority.eq_ignore_ascii_case(host))
}

fn client_key(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| value.len() <= 64 && value.parse::<IpAddr>().is_ok())
        .unwrap_or("loopback")
        .to_owned()
}

impl LoginLimiter {
    fn allow(&mut self, client: &str) -> bool {
        let now = Instant::now();
        prune(&mut self.global, now);
        self.clients.retain(|_, attempts| {
            prune(attempts, now);
            !attempts.is_empty()
        });
        if self.global.len() >= LOGIN_LIMIT_GLOBAL {
            return false;
        }
        let attempts = self.clients.entry(client.to_owned()).or_default();
        if attempts.len() >= LOGIN_LIMIT_PER_CLIENT {
            return false;
        }
        attempts.push_back(now);
        self.global.push_back(now);
        true
    }

    fn clear_client(&mut self, client: &str) {
        self.clients.remove(client);
    }
}

fn prune(attempts: &mut VecDeque<Instant>, now: Instant) {
    while attempts
        .front()
        .is_some_and(|attempt| now.saturating_duration_since(*attempt) >= LOGIN_WINDOW)
    {
        attempts.pop_front();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn hash_password(password: &str) -> Result<String> {
    if password.is_empty() {
        bail!("password must not be empty")
    }
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| anyhow::anyhow!("hash password: {error}"))?
        .to_string();
    release_password_memory();
    Ok(hash)
}

// Argon2 deliberately allocates a large working set. glibc otherwise keeps
// that arena mapped after hashing, making an idle service retain tens of MiB
// that it will not actively use. Keep the secure parameters and return those
// idle pages to Linux after each password operation.
#[cfg(target_env = "gnu")]
fn release_password_memory() {
    // SAFETY: malloc_trim takes no pointer and glibc documents it as MT-Safe.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(target_env = "gnu"))]
fn release_password_memory() {}

#[cfg(test)]
mod tests {
    use std::fs;

    use axum::{body::Body, http::Request};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    fn test_router() -> Router {
        router_with_auth(
            Config {
                auth_token: Some("a-long-enough-test-token-value".into()),
                ..Config::default()
            },
            Some(hash_password("test-password").unwrap()),
            Duration::from_secs(600),
        )
    }

    #[tokio::test]
    async fn health_is_public() {
        let response = test_router()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_requires_valid_bearer_token() {
        let response = test_router()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .header("authorization", "Bearer a-long-enough-test-token-value")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn root_passwd_bootstraps_session_and_redirects_without_echoing_password() {
        let app = test_router();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/?passwd=test-password")
                    .header("host", "webterm.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get(header::LOCATION).unwrap(), "/");
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store, no-cache, must-revalidate"
        );
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(cookie.contains("Secure; HttpOnly; SameSite=Strict"));
        assert!(!cookie.contains("test-password"));
        let cookie_pair = cookie.split(';').next().unwrap();

        let session = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/session")
                    .header("cookie", cookie_pair)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(session.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn root_passwd_rejects_invalid_password_without_session_cookie() {
        let response = test_router()
            .oneshot(
                Request::builder()
                    .uri("/?passwd=wrong-password")
                    .header("host", "webterm.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
    }

    #[tokio::test]
    async fn browser_login_cookie_csrf_and_logout_flow() {
        let app = test_router();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/login")
                    .header("host", "webterm.example")
                    .header("origin", "https://webterm.example")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"test-password"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(cookie.contains("Secure; HttpOnly; SameSite=Strict"));
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let csrf = json["csrf_token"].as_str().unwrap();
        let cookie_pair = cookie.split(';').next().unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/logout")
                    .header("host", "webterm.example")
                    .header("origin", "https://webterm.example")
                    .header("cookie", cookie_pair)
                    .header("x-csrf-token", csrf)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn login_requires_same_origin() {
        let response = test_router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/login")
                    .header("host", "webterm.example")
                    .header("origin", "https://evil.example")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"test-password"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn folder_picker_requires_browser_authentication() {
        let response = test_router()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/folders")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn metrics_require_browser_authentication() {
        let response = test_router()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authenticated_metrics_have_the_bounded_system_shape() {
        let app = test_router();
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/login")
                    .header("host", "webterm.example")
                    .header("origin", "https://webterm.example")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"test-password"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let cookie = login
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/metrics")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let object = json.as_object().unwrap();
        assert_eq!(object.len(), 4);
        for field in [
            "cpu_percent",
            "memory_used_bytes",
            "memory_total_bytes",
            "memory_percent",
        ] {
            assert!(object[field].is_number() || object[field].is_null());
        }
    }

    #[tokio::test]
    async fn fallback_terminal_endpoint_requires_browser_authentication() {
        let response = test_router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/workspaces/1/ensure-terminal")
                    .header("host", "webterm.example")
                    .header("origin", "https://webterm.example")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authenticated_folder_picker_returns_server_listing() {
        let temp = tempfile::TempDir::new().unwrap();
        let allowed = temp.path().join("allowed");
        fs::create_dir(&allowed).unwrap();
        fs::create_dir(allowed.join("child")).unwrap();
        fs::write(allowed.join("hidden.txt"), "not a directory").unwrap();
        let app = router_with_auth(
            Config {
                database_path: temp.path().join("state.db"),
                tmux_socket: temp.path().join("tmux.sock"),
                workspace_roots: vec![allowed.clone()],
                ..Config::default()
            },
            Some(hash_password("test-password").unwrap()),
            Duration::from_secs(600),
        );
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/login")
                    .header("host", "webterm.example")
                    .header("origin", "https://webterm.example")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"test-password"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let cookie = login
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/folders")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 16_384)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["current"], allowed.to_str().unwrap());
        assert_eq!(json["entries"].as_array().unwrap().len(), 1);
        assert_eq!(json["entries"][0]["name"], "child");
    }

    #[test]
    fn folder_listing_is_sorted_directory_only_and_confined() {
        let temp = tempfile::TempDir::new().unwrap();
        let allowed = temp.path().join("allowed");
        let outside = temp.path().join("outside");
        fs::create_dir(&allowed).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(allowed.join("zebra")).unwrap();
        fs::create_dir(allowed.join("Alpha")).unwrap();
        fs::write(allowed.join("private.txt"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, allowed.join("escape")).unwrap();
        let config = Config {
            workspace_roots: vec![allowed.clone()],
            ..Config::default()
        };

        let listing = folder_listing(&config, Some(&allowed)).unwrap();
        assert_eq!(
            listing.current,
            allowed.canonicalize().unwrap().to_str().unwrap()
        );
        assert!(listing.parent.is_none());
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Alpha", "zebra"]
        );
        assert!(!listing.truncated);
        assert!(folder_listing(&config, Some(&outside)).is_err());
    }

    #[test]
    fn runtime_id_prefix_exposes_native_and_legacy_backends() {
        assert_eq!(terminal_backend("pty-012345"), "native-pty");
        assert_eq!(terminal_backend("wt-legacy"), "legacy-tmux");
        assert_eq!(terminal_backend("historical-id"), "legacy-tmux");
    }
}

include!("web_extensions.rs");

include!("web_explorer.rs");
