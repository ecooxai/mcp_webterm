use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Map, Value, json};
use subtle::ConstantTimeEq;

use crate::{
    VERSION,
    config::Config,
    db::{Database, Terminal, Workspace, canonical_workspace_path},
    terminal::TerminalManager,
};

const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
const MAX_REQUEST_BYTES: usize = 128 * 1024;
const MAX_WRITE_BYTES: usize = 64 * 1024;
const MAX_CAPTURE_BYTES: usize = 256 * 1024;
const MAX_CAPTURE_LINES: u64 = 1_000;
const MAX_TERMINAL_COLUMNS: u64 = crate::runtime::MAX_COLS as u64;
const MAX_TERMINAL_ROWS: u64 = crate::runtime::MAX_ROWS as u64;
const MAX_WORKSPACES: usize = 500;
const MAX_TERMINALS: usize = 2_000;

#[derive(Clone)]
struct McpState {
    config: Arc<Config>,
}

/// Build the authenticated, stateless MCP Streamable HTTP routes.
///
/// The returned router has no missing state and can be merged into the main
/// application router. Both spellings are intentional because some MCP client
/// URL normalizers retain a trailing slash.
pub fn router(config: Config) -> Router {
    crate::audit::initialize(&config);
    Router::new()
        .route("/mcp", post(mcp_post))
        .route("/mcp/", post(mcp_post))
        .layer(middleware::from_fn(response_headers))
        .with_state(McpState {
            config: Arc::new(config),
        })
}

async fn response_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn mcp_post(State(state): State<McpState>, request: Request<Body>) -> Response {
    let headers = request.headers();

    if !trusted_origin(headers) {
        return plain_error(StatusCode::FORBIDDEN, "untrusted Origin");
    }

    let bearer_configured = state.config.auth_token.is_some();
    let password_configured = state.config.web_password.is_some();
    if !bearer_configured && !password_configured {
        return plain_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "MCP authentication is not configured",
        );
    }

    let bearer_ok = state
        .config
        .auth_token
        .as_deref()
        .is_some_and(|expected| authorized(headers, expected));
    let password_ok = state
        .config
        .web_password
        .as_deref()
        .is_some_and(|expected| query_password_authorized(request.uri().query(), expected));
    if !bearer_ok && !password_ok {
        let mut response = plain_error(StatusCode::UNAUTHORIZED, "unauthorized");
        if bearer_configured {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"webterm-mcp\""),
            );
        }
        return response;
    }

    if !is_json_content_type(&headers) {
        return plain_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json",
        );
    }
    if !accepts_json(&headers) {
        return plain_error(
            StatusCode::NOT_ACCEPTABLE,
            "Accept must permit application/json",
        );
    }
    if let Some(version) = protocol_version_header(&headers) {
        match version {
            Ok(version) if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) => {}
            Ok(_) | Err(()) => {
                return rpc_response(
                    StatusCode::BAD_REQUEST,
                    rpc_error(Value::Null, -32602, "Unsupported MCP-Protocol-Version"),
                );
            }
        }
    }

    let body = match to_bytes(request.into_body(), MAX_REQUEST_BYTES).await {
        Ok(body) => body,
        Err(_) => return plain_error(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };

    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            return rpc_response(
                StatusCode::BAD_REQUEST,
                rpc_error(Value::Null, -32700, "Parse error"),
            );
        }
    };

    let object = match valid_request(&request) {
        Ok(object) => object,
        Err(()) => {
            return rpc_response(
                StatusCode::BAD_REQUEST,
                rpc_error(Value::Null, -32600, "Invalid Request"),
            );
        }
    };
    let id = object.get("id").cloned();
    let method = object["method"].as_str().expect("validated method");

    // JSON-RPC notifications never have a protocol response. Streamable HTTP
    // acknowledges accepted notifications with an empty 202 response.
    if id.is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    let id = id.unwrap_or(Value::Null);

    let params = match object.get("params") {
        None | Some(Value::Null) => None,
        Some(Value::Object(params)) => Some(params),
        Some(_) => {
            return rpc_response(
                StatusCode::OK,
                rpc_error(id, -32602, "Invalid params: expected an object"),
            );
        }
    };

    let audit_start = Instant::now();
    let audit_config = state.config.clone();
    let audit_id = if method == "tools/call" {
        let config = audit_config.clone(); let owned = params.cloned();
        tokio::task::spawn_blocking(move || crate::audit::begin(&config, owned.as_ref())).await.ok().and_then(Result::ok)
    } else { None };
    let result = match method {
        "initialize" => initialize(params),
        "ping" => Ok(json!({})),
        "tools/list" => list_tools(params),
        "tools/call" => call_tool(state.config, params).await,
        _ => Err(RpcFailure::new(-32601, "Method not found")),
    };

    if let Some(audit_id) = audit_id {
        let (value, failed) = match &result { Ok(value) => (value.clone(), value.get("isError").and_then(Value::as_bool).unwrap_or(false)), Err(error) => (json!({"error":error.message}),true) };
        let duration = audit_start.elapsed().as_millis();
        let _ = tokio::task::spawn_blocking(move || crate::audit::finish(&audit_config,audit_id,&value,failed,duration)).await;
    }
    let message = match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(error) => rpc_error(id, error.code, &error.message),
    };
    rpc_response(StatusCode::OK, message)
}

fn valid_request(value: &Value) -> std::result::Result<&Map<String, Value>, ()> {
    let object = value.as_object().ok_or(())?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || !object.get("method").is_some_and(Value::is_string)
    {
        return Err(());
    }
    if let Some(id) = object.get("id")
        && !id.is_null()
        && !id.is_string()
        && !id.is_number()
    {
        return Err(());
    }
    Ok(object)
}

fn initialize(params: Option<&Map<String, Value>>) -> RpcResult {
    let params = params.ok_or_else(|| RpcFailure::invalid_params("missing initialize params"))?;
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcFailure::invalid_params("protocolVersion must be a string"))?;
    if params
        .get("capabilities")
        .is_some_and(|value| !value.is_object())
    {
        return Err(RpcFailure::invalid_params("capabilities must be an object"));
    }
    if params
        .get("clientInfo")
        .is_some_and(|value| !value.is_object())
    {
        return Err(RpcFailure::invalid_params("clientInfo must be an object"));
    }
    let negotiated = SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .copied()
        .find(|version| *version == requested)
        .unwrap_or(LATEST_PROTOCOL_VERSION);
    Ok(json!({
        "protocolVersion": negotiated,
        "capabilities": {"tools":{"listChanged":false}},
        "serverInfo": {"name":"webterm","version":VERSION},
        "instructions":"Use get_image to return workspace PNG/JPEG/GIF/WebP files as visible native image content, never print base64. Colab private previews use https://PORT-proxy-colabdev.alima.freeddns.org/ with distinct app origins. Use bash and python to build, test and debug in persistent virtual terminals. Bash, Python and terminal write/read/capture require task and summary. Reuse a simple task name; summary is n/100 progress plus fewer than 20 words describing this action. workspace_id accepts only absolute folder paths, never numeric IDs. Authenticated dev previews use /app/index.html?proxyport=PORT with unchanged app paths; /?proxyport=0 returns to WebTerm. Root-relative resources and WebSockets inherit the selected browser port; explicit proxyport always wins. workspace_id is the absolute folder path; one canonical folder has one workspace, visible in the browser. Commands wait up to 20 seconds by default, then return a running terminal handle; never re-run merely because the command is still running. Use terminal_read or terminal_capture to follow it. filter_cmd runs a bounded Bash program on the full retained snapshot via stdin (for example grep -i error or tail -c 500), before the preview; it never types into the original PTY. Prefer full_output=false to avoid polluting model context: outputs over 2000 characters show the first 500 and last 1500. full_output=true returns retained output within safety limits. For requested dev previews prefer cloudflared tunnel --url http://127.0.0.1:PORT in another terminal; this makes the service public, so never expose secrets or admin endpoints."
    }))
}

fn list_tools(params: Option<&Map<String, Value>>) -> RpcResult {
    if params
        .and_then(|params| params.get("cursor"))
        .is_some_and(|cursor| !cursor.is_null())
    {
        return Err(RpcFailure::invalid_params(
            "pagination cursors are not supported",
        ));
    }
    Ok(json!({"tools":tool_definitions()}))
}

async fn call_tool(config: Arc<Config>, params: Option<&Map<String, Value>>) -> RpcResult {
    let params = params.ok_or_else(|| RpcFailure::invalid_params("missing tools/call params"))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| RpcFailure::invalid_params("tool name must be a non-empty string"))?;
    if !TOOL_NAMES.contains(&name) {
        return Err(RpcFailure::invalid_params(format!("unknown tool {name:?}")));
    }
    let arguments = match params.get("arguments") {
        None => Map::new(),
        Some(Value::Object(arguments)) => arguments.clone(),
        Some(_) => {
            return Ok(tool_error("tool arguments must be an object"));
        }
    };
    static IMAGE_LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let _image_slot=if name=="get_image" {Some(IMAGE_LIMIT.acquire().await.expect("image semaphore remains open"))} else {None};
    let name = name.to_owned();
    let task = tokio::task::spawn_blocking(move || {
        if name == "get_image" {
            reject_unknown(&arguments, &["workspace_id","path","task","summary"])?;
            validate_tracking(arguments.get("task").context("task is required")?, arguments.get("summary").context("summary is required")?)?;
            crate::image_tool::read(&config, required_string(&arguments,"workspace_id")?,required_string(&arguments,"path")?)
        } else { execute_tool(&config, &name, &arguments).map(tool_success) }
    }).await;
    let result = match task {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => tool_error(&format!("{error:#}")),
        Err(error) => {
            tracing::error!(error = %error, "MCP tool task failed");
            tool_error("tool execution failed")
        }
    };
    Ok(result)
}

fn execute_tool(config: &Config, name: &str, arguments: &Map<String, Value>) -> Result<Value> {
    let mut operational = arguments.clone();
    let tracked = matches!(
        name,
        "bash" | "python" | "terminal_write" | "terminal_read" | "terminal_capture"
    );
    if tracked || operational.contains_key("task") || operational.contains_key("summary") {
        let task = operational.remove("task").context("task is required")?;
        let summary = operational
            .remove("summary")
            .context("summary is required")?;
        validate_tracking(&task, &summary)?;
    }
    let arguments = &operational;
    if name.starts_with("terminal_") || matches!(name, "bash" | "python" | "workspace_ensure") {
        let path = arguments
            .get("workspace_id")
            .and_then(Value::as_str)
            .context(
                "workspace_id must be an absolute folder path; numeric IDs are not accepted",
            )?;
        if !path.starts_with('/') || path.contains('\0') || path.len() > 4096 {
            bail!("workspace_id must be an absolute folder path");
        }
    }
    let mut result = match name {
        "workspace_ensure" => tool_workspace_ensure(config, arguments),
        "bash" | "python" => tool_command(config, name, arguments),
        "terminal_read" => tool_terminal_capture(config, arguments),
        "status" => tool_status(config, arguments),
        "workspace_list" => tool_workspace_list(config, arguments),
        "terminal_list" => tool_terminal_list(config, arguments),
        "terminal_create" => tool_terminal_create(config, arguments),
        "terminal_capture" => tool_terminal_capture(config, arguments),
        "terminal_write" => tool_terminal_write(config, arguments),
        "terminal_resize" => tool_terminal_resize(config, arguments),
        "terminal_stop" => tool_terminal_stop(config, arguments),
        _ => bail!("unknown tool"),
    }?;
    public_paths(config, &mut result)?;
    Ok(result)
}

fn tool_status(config: &Config, arguments: &Map<String, Value>) -> Result<Value> {
    reject_unknown(arguments, &[])?;
    let manager = TerminalManager::new(config)?;
    let db = Database::open_config(config)?;
    let runtime_ready = std::os::unix::net::UnixStream::connect(&config.runtime_socket).is_ok();
    let running = if runtime_ready {
        db.list_terminals(None)?
            .iter()
            .filter(|t| manager.has_session(t.session_id()).unwrap_or(false))
            .count()
    } else {
        0
    };
    Ok(
        json!({"service":"webterm","running":true,"runtime_ready":runtime_ready,
              "workspaces":db.list_workspaces()?.len(),"running_terminals":running}),
    )
}

fn tool_workspace_list(config: &Config, arguments: &Map<String, Value>) -> Result<Value> {
    reject_unknown(arguments, &[])?;
    let manager = TerminalManager::new(config)?;
    let database = Database::open_config(config)?;
    let mut workspaces = database.list_workspaces()?;
    let mut terminals = database.list_terminals(None)?;
    let truncated = workspaces.len() > MAX_WORKSPACES || terminals.len() > MAX_TERMINALS;
    workspaces.truncate(MAX_WORKSPACES);
    terminals.truncate(MAX_TERMINALS);

    let mut by_workspace: HashMap<i64, Vec<Value>> = HashMap::new();
    for terminal in terminals {
        by_workspace
            .entry(terminal.workspace_id)
            .or_default()
            .push(terminal_json(&terminal, Some(&manager)));
    }
    let workspaces = workspaces
        .into_iter()
        .map(|workspace| {
            let terminals = by_workspace.remove(&workspace.id).unwrap_or_default();
            json!({
                "id":workspace.id,
                "name":workspace.name,
                "path":workspace.path,
                "created_at":workspace.created_at,
                "updated_at":workspace.updated_at,
                "terminals":terminals
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({"workspaces":workspaces,"truncated":truncated}))
}

fn tool_terminal_list(config: &Config, arguments: &Map<String, Value>) -> Result<Value> {
    reject_unknown(arguments, &["workspace_id"])?;
    let workspace_id = workspace_arg(config, arguments, false)?.map(|w| w.id);
    let database = Database::open_config(config)?;
    let manager = TerminalManager::new(config)?;
    if let Some(workspace_id) = workspace_id {
        database.workspace_by_id(workspace_id)?;
    }
    let mut terminals = database.list_terminals(workspace_id)?;
    let truncated = terminals.len() > MAX_TERMINALS;
    terminals.truncate(MAX_TERMINALS);
    Ok(json!({
        "workspace_id":workspace_id,
        "terminals":terminals.iter().map(|terminal| terminal_json(terminal, Some(&manager))).collect::<Vec<_>>(),
        "truncated":truncated
    }))
}

fn tool_terminal_create(config: &Config, arguments: &Map<String, Value>) -> Result<Value> {
    reject_unknown(arguments, &["workspace_id", "name", "cols", "rows"])?;
    let name = optional_string(arguments, "name")?;
    if let Some(name) = name {
        validate_terminal_name(name)?;
    }
    let columns = bounded_u16(arguments, "cols", 80, 2, MAX_TERMINAL_COLUMNS)?;
    let rows = bounded_u16(arguments, "rows", 24, 2, MAX_TERMINAL_ROWS)?;

    // All arguments are validated before the database reservation or native
    // process creation. The reservation is deleted on every startup failure.
    let mut database = Database::open_config(config)?;
    let workspace = workspace_arg(config, arguments, true)?.context("workspace_id is required")?;
    let workspace_id = workspace.id;
    let manager = TerminalManager::new(config).context("prepare terminal manager")?;
    let terminal = match name {
        Some(name) => database.reserve_terminal(workspace_id, name)?,
        None => database.reserve_default_terminal(workspace_id)?,
    };
    if let Err(error) = manager.create(terminal.session_id(), &workspace.path) {
        let _ = database.delete_terminal(terminal.id);
        return Err(error).context("start terminal");
    }
    if let Err(error) = manager.resize(terminal.session_id(), columns, rows) {
        let _ = manager.stop(terminal.session_id());
        let _ = database.delete_terminal(terminal.id);
        return Err(error).context("resize new terminal");
    }
    let terminal = match database.set_terminal_status(terminal.id, "running") {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = manager.stop(terminal.session_id());
            let _ = database.delete_terminal(terminal.id);
            return Err(error).context("persist running terminal status");
        }
    };
    Ok(json!({"terminal":terminal_json(&terminal, Some(&manager))}))
}

fn tool_terminal_capture(config: &Config, arguments: &Map<String, Value>) -> Result<Value> {
    reject_unknown(
        arguments,
        &[
            "workspace_id",
            "terminal_id",
            "lines",
            "full_output",
            "filter_cmd",
        ],
    )?;
    let terminal_id = required_positive_i64(arguments, "terminal_id")?;
    let filter = optional_string(arguments, "filter_cmd")?;
    if let Some(cmd) = filter {
        if cmd.trim().is_empty() || cmd.len() > 4096 || cmd.contains('\0') {
            bail!("filter_cmd must be nonempty Bash text of at most 4096 UTF-8 bytes");
        }
    }
    let lines = bounded_u16(
        arguments,
        "lines",
        if filter.is_some() { 1000 } else { 100 },
        1,
        MAX_CAPTURE_LINES,
    )?;
    let full = optional_bool(arguments, "full_output", false)?;
    let database = Database::open_config(config)?;
    let terminal = scoped_terminal(config, arguments, &database, terminal_id)?;
    let manager = TerminalManager::new(config)?;
    let mut result = if let Some(value) = command_result(config, &terminal, true)? {
        value
    } else {
        let output = manager.capture(terminal.session_id(), lines)?;
        let (output, capped) = truncate_utf8(output, MAX_CAPTURE_BYTES);
        json!({"terminal_id":terminal_id,"workspace_id":terminal.workspace_id,"running":manager.has_session(terminal.session_id())?,
               "exit_code":null,"output":output,"capture_limited":capped,"capture_lines":lines})
    };
    if let Some(cmd) = filter {
        let text = result["output"].as_str().unwrap_or("");
        let input_limited = result.get("retention_limited") == Some(&json!(true))
            || result.get("capture_limited") == Some(&json!(true));
        let cwd = database.workspace_by_id(terminal.workspace_id)?.path;
        let filtered = crate::terminal_filter::apply(cmd, text, &cwd)?;
        result["retention_limited"] = json!(false);
        for (key, value) in filtered.as_object().context("filter object")? {
            result[key] = value.clone();
        }
        result["filter_input_limited"] = json!(input_limited);
    }
    limit_output(&mut result, full);
    // Execution timestamps/PIDs and repeated warnings belong in diagnostics, not every poll.
    let keep = [
        "terminal_id",
        "workspace_id",
        "running",
        "exit_code",
        "output",
        "output_chars",
        "output_truncated",
        "omitted_chars",
        "full_output",
        "interrupted",
        "retention_limited",
        "capture_limited",
        "capture_lines",
        "filter_exit_code",
        "filter_stderr",
        "filter_input_chars",
        "filter_input_limited",
        "filter_stderr_truncated",
        "filter_timed_out",
        "filter_output_limit_hit",
    ];
    result
        .as_object_mut()
        .context("terminal result")?
        .retain(|key, _| keep.contains(&key.as_str()));
    Ok(result)
}

fn tool_terminal_write(config: &Config, arguments: &Map<String, Value>) -> Result<Value> {
    reject_unknown(arguments, &["workspace_id", "terminal_id", "data", "enter"])?;
    let terminal_id = required_positive_i64(arguments, "terminal_id")?;
    let data = required_string(arguments, "data")?;
    if data.len() > MAX_WRITE_BYTES {
        bail!("data must be at most {MAX_WRITE_BYTES} UTF-8 bytes")
    }
    let enter = optional_bool(arguments, "enter", false)?;
    let database = Database::open_config(config)?;
    let terminal = scoped_terminal(config, arguments, &database, terminal_id)?;
    // Input to a running command preserves its collector; a new shell command
    // after completion switches captures back to the visible terminal screen.
    let record = command_folder(config, &terminal).join("result.json");
    if record.is_file() {
        if let Ok(value) = serde_json::from_slice::<Value>(&fs::read(&record)?) {
            if value["running"] == false {
                fs::rename(&record, record.with_file_name("previous.json"))?;
            }
        }
    }
    TerminalManager::new(config)?.write(terminal.session_id(), data, enter)?;
    Ok(json!({
        "terminal_id":terminal_id,
        "workspace_id":terminal.workspace_id,
        "bytes_written":data.len(),
        "enter":enter
    }))
}

fn tool_terminal_resize(config: &Config, arguments: &Map<String, Value>) -> Result<Value> {
    reject_unknown(arguments, &["workspace_id", "terminal_id", "cols", "rows"])?;
    let terminal_id = required_positive_i64(arguments, "terminal_id")?;
    let columns = bounded_u16(arguments, "cols", 0, 2, MAX_TERMINAL_COLUMNS)?;
    let rows = bounded_u16(arguments, "rows", 0, 2, MAX_TERMINAL_ROWS)?;
    let database = Database::open_config(config)?;
    let terminal = scoped_terminal(config, arguments, &database, terminal_id)?;
    TerminalManager::new(config)?.resize(terminal.session_id(), columns, rows)?;
    Ok(
        json!({"terminal_id":terminal_id,"workspace_id":terminal.workspace_id,"cols":columns,"rows":rows}),
    )
}

fn tool_terminal_stop(config: &Config, arguments: &Map<String, Value>) -> Result<Value> {
    reject_unknown(arguments, &["workspace_id", "terminal_id"])?;
    let terminal_id = required_positive_i64(arguments, "terminal_id")?;
    let database = Database::open_config(config)?;
    let terminal = scoped_terminal(config, arguments, &database, terminal_id)?;
    let manager = TerminalManager::new(config)?;
    manager.stop(terminal.session_id())?;
    let terminal = database.set_terminal_status(terminal_id, "stopped")?;
    Ok(json!({"terminal":terminal_json(&terminal, Some(&manager))}))
}

fn workspace_arg(
    config: &Config,
    arguments: &Map<String, Value>,
    create: bool,
) -> Result<Option<Workspace>> {
    let value = arguments
        .get("workspace_id")
        .context("workspace_id is required and must be an absolute folder path")?;
    let database = Database::open_config(config)?;
    let raw = value
        .as_str()
        .context("workspace_id must be an absolute folder path")?;
    if raw.is_empty() || raw.len() > 4096 || raw.contains('\0') {
        bail!("invalid workspace path")
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        bail!("workspace_id must be an absolute folder path; numeric IDs are not accepted")
    }
    if !path.exists() && create {
        // Check the nearest existing ancestor before making any folders. The
        // second canonical check rejects symlinks escaping the configured roots.
        let parent = path
            .ancestors()
            .find(|p| p.exists())
            .context("no existing workspace ancestor")?;
        canonical_workspace_path(config, parent)?;
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!("do not use .. when creating a workspace folder")
        }
        fs::create_dir_all(path).context("create workspace folder")?;
    }
    let canonical = canonical_workspace_path(config, path)?;
    if create {
        return Ok(Some(database.ensure_workspace(&canonical)?));
    }
    let workspace = database
        .list_workspaces()?
        .into_iter()
        .find(|w| w.path == canonical)
        .context(
            "workspace is not registered; run bash, python or terminal_create in this folder first",
        )?;
    Ok(Some(workspace))
}

fn scoped_terminal(
    config: &Config,
    args: &Map<String, Value>,
    database: &Database,
    id: i64,
) -> Result<Terminal> {
    let terminal = database.terminal_by_id(id)?;
    if let Some(workspace) = workspace_arg(config, args, false)? {
        if terminal.workspace_id != workspace.id {
            bail!("terminal does not belong to workspace_id")
        }
    }
    Ok(terminal)
}

fn tool_workspace_ensure(config: &Config, args: &Map<String, Value>) -> Result<Value> {
    reject_unknown(args, &["workspace_id"])?;
    let workspace = workspace_arg(config, args, true)?.context("workspace_id is required")?;
    Ok(json!({"workspace":workspace}))
}

fn public_paths(config: &Config, value: &mut Value) -> Result<()> {
    let paths: HashMap<i64, String> = Database::open_config(config)?
        .list_workspaces()?
        .into_iter()
        .map(|w| (w.id, w.path.to_string_lossy().to_string()))
        .collect();
    fn visit(value: &mut Value, paths: &HashMap<i64, String>) {
        match value {
            Value::Object(object) => {
                if object.get("path").is_some_and(Value::is_string)
                    && object.get("id").is_some_and(Value::is_i64)
                {
                    object.insert("id".into(), object["path"].clone());
                    object.insert("workspace_id".into(), object["path"].clone());
                }
                if let Some(id) = object.get("workspace_id").and_then(Value::as_i64) {
                    if let Some(path) = paths.get(&id) {
                        object.insert("workspace_id".into(), json!(path));
                    }
                }
                for child in object.values_mut() {
                    visit(child, paths);
                }
            }
            Value::Array(items) => {
                for child in items {
                    visit(child, paths);
                }
            }
            _ => {}
        }
    }
    visit(value, &paths);
    Ok(())
}

fn limit_output(value: &mut Value, full: bool) {
    let output = value["output"].as_str().unwrap_or("").to_owned();
    let chars: Vec<char> = output.chars().collect();
    let total = value
        .get("output_chars")
        .and_then(Value::as_u64)
        .unwrap_or(chars.len() as u64);
    let preview = !full && chars.len() > 2000;
    let shown = if preview {
        chars[..500]
            .iter()
            .chain(chars[chars.len() - 1500..].iter())
            .collect::<String>()
    } else {
        output
    };
    let shown_chars = shown.chars().count() as u64;
    value["output"] = json!(shown);
    value["output_chars"] = json!(total);
    value["full_output"] = json!(full);
    value["output_truncated"] = json!(shown_chars < total);
    value["truncated"] =
        json!(shown_chars < total || value.get("capture_limited") == Some(&json!(true)));
    value["omitted_chars"] = json!(total.saturating_sub(shown_chars));
    if preview {
        value["truncation_note"] = json!(
            "Output contains exactly the first 500 and last 1500 characters; the middle is omitted. Prefer full_output=false to avoid context pollution. Use full_output=true only when needed."
        );
    } else if shown_chars < total {
        value["truncation_note"] = json!(
            "Full retained output returned; older middle content exceeded the safety retention limit. Save unlimited logs explicitly in your workspace when needed."
        );
    }
}

fn command_folder(config: &Config, terminal: &Terminal) -> PathBuf {
    std::env::var_os("WEBTERM_COMMAND_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            config
                .database_path
                .parent()
                .unwrap_or(Path::new("."))
                .join("mcp-commands")
        })
        .join(terminal.session_id())
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

fn command_result(config: &Config, terminal: &Terminal, full: bool) -> Result<Option<Value>> {
    let file = command_folder(config, terminal).join("result.json");
    if !file.is_file() {
        return Ok(None);
    }
    if fs::metadata(&file)?.len() > 2 * 1024 * 1024 {
        bail!("command output record exceeds safety limit")
    }
    let mut value: Value =
        serde_json::from_slice(&fs::read(file)?).context("read command output")?;
    value["terminal_id"] = json!(terminal.id);
    value["workspace_id"] = json!(terminal.workspace_id);
    value["retained_output"] = json!(true);
    if terminal.status == "stopped" && value["running"] == true {
        value["running"] = json!(false);
        value["exit_code"] = Value::Null;
        value["interrupted"] = json!(true);
    }
    limit_output(&mut value, full);
    Ok(Some(value))
}

fn quote_shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn tool_command(config: &Config, name: &str, args: &Map<String, Value>) -> Result<Value> {
    use std::os::unix::fs::PermissionsExt;
    let key = if name == "bash" { "command" } else { "code" };
    reject_unknown(args, &["workspace_id", key, "wait_s", "full_output"])?;
    let source = required_string(args, key)?;
    if source.trim().is_empty() || source.len() > 32768 {
        bail!("source must be nonempty and at most 32768 UTF-8 bytes")
    }
    let full = optional_bool(args, "full_output", false)?;
    let wait = args
        .get("wait_s")
        .map(|v| v.as_f64().context("wait_s must be a number"))
        .transpose()?
        .unwrap_or(20.0);
    if !wait.is_finite() || !(0.0..=20.0).contains(&wait) {
        bail!("wait_s must be between 0 and 20 seconds")
    }
    if !args.contains_key("workspace_id") {
        bail!("workspace_id is required")
    }
    let created = tool_terminal_create(
        config,
        json!({"workspace_id":args["workspace_id"],"cols":160,"rows":45})
            .as_object()
            .unwrap(),
    )?;
    let terminal_id = created["terminal"]["id"]
        .as_i64()
        .context("new terminal ID")?;
    let database = Database::open_config(config)?;
    let terminal = database.terminal_by_id(terminal_id)?;
    let folder = command_folder(config, &terminal);
    fs::create_dir_all(&folder)?;
    fs::set_permissions(folder.parent().unwrap(), fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
    private_write(
        &folder.join("runner.py"),
        include_bytes!("command_runner.py"),
    )?;
    private_write(
        &folder.join(if name == "bash" {
            "source.sh"
        } else {
            "source.py"
        }),
        source.as_bytes(),
    )?;
    private_write(
        &folder.join("spec.json"),
        serde_json::to_string(&json!({"language":name}))?.as_bytes(),
    )?;
    private_write(&folder.join("result.json"),serde_json::to_string(&json!({"language":name,"running":true,"exit_code":null,"output":"","output_chars":0,"starting":true}))?.as_bytes())?;
    let launch = format!(
        "python3 {} {}",
        quote_shell(&folder.join("runner.py").to_string_lossy()),
        quote_shell(&folder.to_string_lossy())
    );
    let began = Instant::now();
    TerminalManager::new(config)?.write(terminal.session_id(), &launch, true)?;
    loop {
        let value =
            command_result(config, &terminal, full)?.context("command output record missing")?;
        if value["running"] == false || began.elapsed().as_secs_f64() >= wait {
            return Ok(value);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

const TOOL_NAMES: [&str; 13] = [
    "get_image",
    "workspace_ensure",
    "bash",
    "python",
    "terminal_read",
    "status",
    "workspace_list",
    "terminal_list",
    "terminal_create",
    "terminal_capture",
    "terminal_write",
    "terminal_resize",
    "terminal_stop",
];

fn tool_definitions() -> Vec<Value> {
    let read_only = json!({
        "readOnlyHint":true,
        "destructiveHint":false,
        "idempotentHint":true,
        "openWorldHint":false
    });
    let mut definitions = vec![
        tool_definition(
            "status",
            "WebTerm status",
            "Return bounded service, workspace, and terminal status counts.",
            object_schema(json!({}), &[]),
            json!({"type":"object"}),
            read_only.clone(),
        ),
        tool_definition(
            "workspace_list",
            "List workspaces",
            "List configured workspaces with their existing terminal records.",
            object_schema(json!({}), &[]),
            json!({"type":"object"}),
            read_only.clone(),
        ),
        tool_definition(
            "terminal_list",
            "List terminals",
            "List terminal records, optionally within one workspace.",
            object_schema(json!({"workspace_id":{"type":"integer","minimum":1}}), &[]),
            json!({"type":"object"}),
            read_only.clone(),
        ),
        tool_definition(
            "terminal_create",
            "Create terminal",
            "Create a persistent terminal in an existing workspace. Omit name to atomically choose the first available positive integer.",
            object_schema(
                json!({
                    "workspace_id":{"type":"integer","minimum":1},
                    "name":{"type":"string","minLength":1,"maxLength":64},
                    "cols":{"type":"integer","minimum":2,"maximum":MAX_TERMINAL_COLUMNS,"default":80},
                    "rows":{"type":"integer","minimum":2,"maximum":MAX_TERMINAL_ROWS,"default":24}
                }),
                &["workspace_id"],
            ),
            json!({"type":"object"}),
            json!({
                "readOnlyHint":false,
                "destructiveHint":false,
                "idempotentHint":false,
                "openWorldHint":false
            }),
        ),
        tool_definition(
            "terminal_capture",
            "Capture terminal",
            "Capture recent visible terminal output. Results are capped at 256 KiB.",
            object_schema(
                json!({
                    "terminal_id":{"type":"integer","minimum":1},
                    "lines":{"type":"integer","minimum":1,"maximum":MAX_CAPTURE_LINES,"default":100}
                }),
                &["terminal_id"],
            ),
            json!({"type":"object"}),
            read_only,
        ),
        tool_definition(
            "terminal_write",
            "Write to terminal",
            "Send up to 64 KiB of literal UTF-8 data to a running terminal and optionally press Enter. The terminal may execute the supplied input.",
            object_schema(
                json!({
                    "terminal_id":{"type":"integer","minimum":1},
                    "data":{"type":"string","maxLength":MAX_WRITE_BYTES},
                    "enter":{"type":"boolean","default":false}
                }),
                &["terminal_id", "data"],
            ),
            json!({"type":"object"}),
            json!({
                "readOnlyHint":false,
                "destructiveHint":true,
                "idempotentHint":false,
                "openWorldHint":true
            }),
        ),
        tool_definition(
            "terminal_resize",
            "Resize terminal",
            "Set bounded dimensions for a running terminal window.",
            object_schema(
                json!({
                    "terminal_id":{"type":"integer","minimum":1},
                    "cols":{"type":"integer","minimum":2,"maximum":MAX_TERMINAL_COLUMNS},
                    "rows":{"type":"integer","minimum":2,"maximum":MAX_TERMINAL_ROWS}
                }),
                &["terminal_id", "cols", "rows"],
            ),
            json!({"type":"object"}),
            json!({
                "readOnlyHint":false,
                "destructiveHint":false,
                "idempotentHint":true,
                "openWorldHint":false
            }),
        ),
        tool_definition(
            "terminal_stop",
            "Stop terminal",
            "Stop a native PTY or live legacy tmux session and retain its database record with stopped status.",
            object_schema(
                json!({"terminal_id":{"type":"integer","minimum":1}}),
                &["terminal_id"],
            ),
            json!({"type":"object"}),
            json!({
                "readOnlyHint":false,
                "destructiveHint":true,
                "idempotentHint":true,
                "openWorldHint":false
            }),
        ),
    ];
    let workspace = json!({"type":"string","minLength":1,"maxLength":4096,"description":"Absolute folder path, e.g. /home/dev/project/app. Canonical aliases share one workspace."});
    let full = json!({"type":"boolean","default":false,"description":"Prefer false to avoid context pollution. True bypasses the 2000-character preview, not retained-output safety limits."});
    for tool in &mut definitions {
        if tool["name"]
            .as_str()
            .is_some_and(|n| n.starts_with("terminal_"))
        {
            tool["inputSchema"]["properties"]["workspace_id"] = workspace.clone();
            let required = tool["inputSchema"]["required"].as_array_mut().unwrap();
            if !required.contains(&json!("workspace_id")) {
                required.push(json!("workspace_id"));
            }
        }
        if tool["name"] == "terminal_capture" {
            tool["inputSchema"]["properties"]["full_output"] = full.clone();
            tool["description"] = json!(
                "Read retained command output, or recent visible screen output for ordinary terminals. Default preview is first 500 plus last 1500 characters. Prefer full_output=false."
            );
        }
        if tool["name"] == "terminal_create" {
            tool["description"] = json!(
                "Create a persistent terminal in the workspace folder, registering it once and showing it in WebTerm."
            );
        }
    }
    let mut read = definitions
        .iter()
        .find(|d| d["name"] == "terminal_capture")
        .unwrap()
        .clone();
    read["name"] = json!("terminal_read");
    read["title"] = json!("Read terminal");
    definitions.push(read);
    definitions.push(tool_definition("workspace_ensure","Open workspace","Register an absolute folder once, creating it within allowed roots when missing. Appears in WebTerm.",object_schema(json!({"workspace_id":workspace.clone()}), &["workspace_id"]),json!({"type":"object"}),json!({"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false})));
    for (name, key) in [("bash", "command"), ("python", "code")] {
        let mut properties = json!({"workspace_id":workspace.clone(),"wait_s":{"type":"number","minimum":0,"maximum":20,"default":20},"full_output":full.clone()});
        properties[key] = json!({"type":"string","minLength":1,"maxLength":32768});
        definitions.push(tool_definition(name,name,"Build, test and debug in a persistent virtual terminal in workspace_id. Wait up to 20 seconds by default; return output and exit_code when complete, otherwise a running terminal_id. Prefer full_output=false.",object_schema(properties,&["workspace_id",key]),json!({"type":"object"}),json!({"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true})));
    }
    definitions.push(tool_definition("get_image","Get image","Return one PNG/JPEG/GIF/WebP file as native MCP image content so the client can see it. Path is confined to workspace_id; max 16 MiB and 32 megapixels. No shell or file upload.",object_schema(json!({"workspace_id":workspace.clone(),"path":{"type":"string","minLength":1,"maxLength":4096},"task":{"type":"string","minLength":1,"maxLength":80},"summary":{"type":"string","minLength":7,"maxLength":240}}), &["workspace_id","path","task","summary"]),json!({"type":"object"}),json!({"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false})));
    for tool in &mut definitions {
        if matches!(
            tool["name"].as_str(),
            Some("bash" | "python" | "terminal_write" | "terminal_read" | "terminal_capture")
        ) {
            tool["inputSchema"]["properties"]["task"] = json!({"type":"string","minLength":1,"maxLength":80,"description":"Simple task name. Reuse it across calls for this task in this workspace."});
            tool["inputSchema"]["properties"]["summary"] = json!({"type":"string","minLength":7,"maxLength":240,"description":"Current progress n/100, then a short action description containing fewer than 20 words. Example: 35/100 Testing query proxy resources."});
            let required = tool["inputSchema"]["required"].as_array_mut().unwrap();
            required.push(json!("task"));
            required.push(json!("summary"));
        }
    }
    for tool in &mut definitions {
        let name = tool["name"].as_str().unwrap().to_owned();
        if matches!(name.as_str(), "terminal_read" | "terminal_capture") {
            tool["inputSchema"]["properties"]["filter_cmd"] = json!({"type":"string","minLength":1,"maxLength":4096,
                "description":"Optional Bash filter on the full retained terminal text via stdin, before preview. Examples: grep -i error; tail -c 500; python3 -c 'import sys; print(sys.stdin.read()[-500:],end=\"\")'. Runs in workspace, separate from live PTY, at most 5 seconds. Has shell permissions, not a sandbox."});
            tool["annotations"]["readOnlyHint"] = json!(false);
            tool["annotations"]["destructiveHint"] = json!(true);
            tool["annotations"]["openWorldHint"] = json!(true);
            tool["annotations"]["idempotentHint"] = json!(false);
        }
        tool["outputSchema"] = crate::output_contracts::schema(&name);
    }
    definitions
}

fn validate_tracking(task: &Value, summary: &Value) -> Result<()> {
    let task = task.as_str().context("task must be a simple task name")?;
    if task.trim().is_empty() || task.chars().count() > 80 || task.chars().any(char::is_control) {
        bail!("task must be a single-line name of 1..80 characters");
    }
    let summary = summary
        .as_str()
        .context("summary must be n/100 followed by a description")?;
    if summary.chars().count() > 240 || summary.chars().any(char::is_control) {
        bail!("summary must be one line of at most 240 characters");
    }
    let fields = summary.split_whitespace().collect::<Vec<_>>();
    if !(2..=20).contains(&fields.len()) {
        bail!("summary description must contain fewer than 20 words");
    }
    let number = fields[0]
        .strip_suffix("/100")
        .context("summary must start with n/100")?;
    if number.is_empty()
        || number.len() > 3
        || !number.bytes().all(|b| b.is_ascii_digit())
        || number.parse::<u16>()? > 100
    {
        bail!("summary progress must be 0..100/100");
    }
    Ok(())
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type":"object",
        "properties":properties,
        "required":required,
        "additionalProperties":false
    })
}

fn tool_definition(
    name: &str,
    title: &str,
    description: &str,
    input_schema: Value,
    output_schema: Value,
    mut annotations: Value,
) -> Value {
    annotations["title"] = json!(title);
    json!({
        "name":name,
        "title":title,
        "description":description,
        "inputSchema":input_schema,
        "outputSchema":output_schema,
        "annotations":annotations
    })
}

fn terminal_json(terminal: &Terminal, manager: Option<&TerminalManager>) -> Value {
    let runtime_pid = if terminal.backend() == "native-pty"
        && matches!(terminal.status.as_str(), "running" | "starting")
    {
        manager
            .and_then(|manager| manager.native().info(terminal.session_id()).ok())
            .map(|info| info.pid)
    } else {
        None
    };
    json!({
        "id":terminal.id,
        "workspace_id":terminal.workspace_id,
        "name":terminal.name,
        "session_id":terminal.session_id(),
        "backend":terminal.backend(),
        "runtime_pid":runtime_pid,
        "status":terminal.status,
        "created_at":terminal.created_at,
        "updated_at":terminal.updated_at
    })
}

fn tool_success(result: Value) -> Value {
    let text = serde_json::to_string_pretty(&result)
        .unwrap_or_else(|_| "Tool completed successfully.".to_owned());
    json!({
        "content":[{"type":"text","text":text}],
        "structuredContent":result,
        "isError":false
    })
}

fn tool_error(message: &str) -> Value {
    let (message, truncated) = truncate_utf8(message.to_owned(), 8 * 1024);
    let suffix = if truncated { " [truncated]" } else { "" };
    json!({
        "content":[{"type":"text","text":format!("Error: {message}{suffix}")}],
        "isError":true
    })
}

fn reject_unknown(arguments: &Map<String, Value>, allowed: &[&str]) -> Result<()> {
    if let Some(name) = arguments
        .keys()
        .find(|name| !allowed.contains(&name.as_str()))
    {
        bail!("unknown argument {name:?}")
    }
    Ok(())
}

fn required_positive_i64(arguments: &Map<String, Value>, name: &str) -> Result<i64> {
    arguments
        .get(name)
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .with_context(|| format!("{name} must be a positive integer"))
}

#[allow(dead_code)]
fn optional_positive_i64(arguments: &Map<String, Value>, name: &str) -> Result<Option<i64>> {
    match arguments.get(name) {
        None => Ok(None),
        Some(value) => value
            .as_i64()
            .filter(|value| *value > 0)
            .map(Some)
            .with_context(|| format!("{name} must be a positive integer")),
    }
}

fn required_string<'a>(arguments: &'a Map<String, Value>, name: &str) -> Result<&'a str> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("{name} must be a string"))
}

fn optional_string<'a>(arguments: &'a Map<String, Value>, name: &str) -> Result<Option<&'a str>> {
    match arguments.get(name) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .with_context(|| format!("{name} must be a string")),
    }
}

fn optional_bool(arguments: &Map<String, Value>, name: &str, default: bool) -> Result<bool> {
    match arguments.get(name) {
        None => Ok(default),
        Some(value) => value
            .as_bool()
            .with_context(|| format!("{name} must be a boolean")),
    }
}

fn bounded_u16(
    arguments: &Map<String, Value>,
    name: &str,
    default: u16,
    minimum: u64,
    maximum: u64,
) -> Result<u16> {
    let value = match arguments.get(name) {
        Some(value) => value
            .as_u64()
            .with_context(|| format!("{name} must be an integer"))?,
        None if default >= minimum as u16 => return Ok(default),
        None => bail!("{name} is required"),
    };
    if !(minimum..=maximum).contains(&value) {
        bail!("{name} must be between {minimum} and {maximum}")
    }
    Ok(value as u16)
}

fn validate_terminal_name(name: &str) -> Result<()> {
    let length = name.chars().count();
    if !(1..=64).contains(&length) || name.trim() != name {
        bail!("name must contain 1-64 characters with no leading or trailing whitespace")
    }
    if name.chars().any(char::is_control) {
        bail!("name must not contain control characters")
    }
    Ok(())
}

fn truncate_utf8(mut value: String, maximum: usize) -> (String, bool) {
    if value.len() <= maximum {
        return (value, false);
    }
    let mut boundary = maximum;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    (value, true)
}

fn query_password_authorized(query: Option<&str>, expected: &str) -> bool {
    let Some(query) = query else {
        return false;
    };
    let mut supplied = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key != "passwd" && key != "password" {
            continue;
        }
        if supplied.replace(value).is_some() {
            return false;
        }
    }
    supplied
        .filter(|value| value.len() == expected.len())
        .is_some_and(|value| bool::from(value.as_bytes().ct_eq(expected.as_bytes())))
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, token)| token);
    supplied
        .filter(|value| value.len() == expected.len())
        .is_some_and(|value| bool::from(value.as_bytes().ct_eq(expected.as_bytes())))
}

fn trusted_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Some(origin) = origin.to_str().ok() else {
        return false;
    };
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let same_origin = origin == format!("https://{host}") || origin == format!("http://{host}");
    let cross_site = headers
        .get("sec-fetch-site")
        .is_some_and(|value| value == "cross-site");
    same_origin && !cross_site
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

fn accepts_json(headers: &HeaderMap) -> bool {
    if !headers.contains_key(header::ACCEPT) {
        return true;
    }
    headers.get_all(header::ACCEPT).iter().any(|value| {
        value.to_str().ok().is_some_and(|value| {
            value.split(',').any(|item| {
                let mut pieces = item.split(';');
                let media_type = pieces.next().unwrap_or_default().trim();
                let rejected = pieces.any(|parameter| {
                    let parameter = parameter.trim();
                    parameter.split_once('=').is_some_and(|(name, quality)| {
                        name.trim().eq_ignore_ascii_case("q")
                            && quality
                                .trim()
                                .parse::<f32>()
                                .is_ok_and(|quality| quality <= 0.0)
                    })
                });
                !rejected
                    && (media_type.eq_ignore_ascii_case("application/json")
                        || media_type.eq_ignore_ascii_case("application/*")
                        || media_type == "*/*")
            })
        })
    })
}

fn protocol_version_header(headers: &HeaderMap) -> Option<std::result::Result<&str, ()>> {
    headers
        .get("mcp-protocol-version")
        .map(|value| value.to_str().map_err(|_| ()))
}

fn plain_error(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        message,
    )
        .into_response()
}

fn rpc_response(status: StatusCode, message: Value) -> Response {
    (status, Json(message)).into_response()
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

type RpcResult = std::result::Result<Value, RpcFailure>;

struct RpcFailure {
    code: i64,
    message: String,
}

impl RpcFailure {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, format!("Invalid params: {}", message.into()))
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{HeaderName, Method, Request},
    };
    use tempfile::TempDir;
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "test-mcp-token-0123456789abcdef";

    fn test_config(temp: &TempDir) -> Config {
        Config {
            database_path: temp.path().join("state.db"),
            tmux_socket: temp.path().join("tmux.sock"),
            workspace_roots: vec![temp.path().to_owned()],
            auth_token: Some(TOKEN.to_owned()),
            ..Config::default()
        }
    }

    fn request(body: Value) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn json_body(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), MAX_REQUEST_BYTES * 8)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn authentication_origin_and_method_are_enforced() {
        let temp = TempDir::new().unwrap();
        let app = router(test_config(&temp));

        let missing = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
            .unwrap();
        let response = app.clone().oneshot(missing).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, no-cache, must-revalidate"
        );

        let mut bad_origin = request(json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
        bad_origin
            .headers_mut()
            .insert(header::HOST, HeaderValue::from_static("webterm.test"));
        bad_origin.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.test"),
        );
        assert_eq!(
            app.clone().oneshot(bad_origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );

        let get = Request::builder()
            .method(Method::GET)
            .uri("/mcp/")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(get).await.unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()[header::ALLOW], "POST");
    }

    #[tokio::test]
    async fn query_password_can_authenticate_without_authorization_header() {
        let temp = TempDir::new().unwrap();
        let mut config = test_config(&temp);
        config.web_password = Some("2208".to_owned());
        let app = router(config);

        let request = Request::builder()
            .method(Method::POST)
            .uri("/mcp?password=2208")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["result"], json!({}));

        let wrong = Request::builder()
            .method(Method::POST)
            .uri("/mcp?password=wrong")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(wrong).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let duplicate = Request::builder()
            .method(Method::POST)
            .uri("/mcp?password=2208&password=2208")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#))
            .unwrap();
        assert_eq!(
            app.oneshot(duplicate).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn missing_server_token_is_service_unavailable() {
        let temp = TempDir::new().unwrap();
        let mut config = test_config(&temp);
        config.auth_token = None;
        let response = router(config)
            .oneshot(request(json!({"jsonrpc":"2.0","id":1,"method":"ping"})))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn transport_media_types_and_protocol_header_are_checked() {
        let temp = TempDir::new().unwrap();
        let app = router(test_config(&temp));
        let mut unacceptable = request(json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
        unacceptable.headers_mut().insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        assert_eq!(
            app.clone().oneshot(unacceptable).await.unwrap().status(),
            StatusCode::NOT_ACCEPTABLE
        );

        let mut wrong_type = request(json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
        wrong_type
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        assert_eq!(
            app.clone().oneshot(wrong_type).await.unwrap().status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );

        let mut wrong_version = request(json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
        wrong_version.headers_mut().insert(
            HeaderName::from_static("mcp-protocol-version"),
            HeaderValue::from_static("2099-01-01"),
        );
        let response = app.oneshot(wrong_version).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(response).await["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn body_limit_applies_after_authentication_and_trailing_slash_works() {
        let temp = TempDir::new().unwrap();
        let app = router(test_config(&temp));
        let oversized = "x".repeat(MAX_REQUEST_BYTES + 1);
        let missing_auth = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(oversized.clone()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(missing_auth).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let too_large = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .body(Body::from(oversized))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(too_large).await.unwrap().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );

        let mut ping = request(json!({"jsonrpc":"2.0","id":"slash","method":"ping"}));
        *ping.uri_mut() = "/mcp/".parse().unwrap();
        ping.headers_mut()
            .insert(header::HOST, HeaderValue::from_static("webterm.test"));
        ping.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("https://webterm.test"),
        );
        let response = app.oneshot(ping).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let body = json_body(response).await;
        assert_eq!(body["id"], "slash");
        assert_eq!(body["result"], json!({}));
    }

    #[tokio::test]
    async fn initialize_negotiates_supported_versions_and_known_fallback() {
        let temp = TempDir::new().unwrap();
        let app = router(test_config(&temp));
        for version in SUPPORTED_PROTOCOL_VERSIONS {
            let response = app
                .clone()
                .oneshot(request(json!({
                    "jsonrpc":"2.0","id":version,"method":"initialize",
                    "params":{"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"test","version":"1"}}
                })))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = json_body(response).await;
            assert_eq!(body["id"], version);
            assert_eq!(body["result"]["protocolVersion"], version);
            assert_eq!(
                body["result"]["capabilities"]["tools"]["listChanged"],
                false
            );
        }
        let response = app
            .oneshot(request(json!({
                "jsonrpc":"2.0","id":9,"method":"initialize",
                "params":{"protocolVersion":"unknown","capabilities":{},"clientInfo":{"name":"test","version":"1"}}
            })))
            .await
            .unwrap();
        assert_eq!(
            json_body(response).await["result"]["protocolVersion"],
            LATEST_PROTOCOL_VERSION
        );
    }

    #[tokio::test]
    async fn notifications_and_json_rpc_errors_preserve_contract() {
        let temp = TempDir::new().unwrap();
        let app = router(test_config(&temp));
        let response = app
            .clone()
            .oneshot(request(json!({
                "jsonrpc":"2.0","method":"notifications/initialized"
            })))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(to_bytes(response.into_body(), 1).await.unwrap().is_empty());

        let malformed = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{"))
            .unwrap();
        let response = app.clone().oneshot(malformed).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_body(response).await;
        assert_eq!(body["id"], Value::Null);
        assert_eq!(body["error"]["code"], -32700);

        for (payload, code) in [
            (json!({"jsonrpc":"1.0","id":1,"method":"ping"}), -32600),
            (
                json!({"jsonrpc":"2.0","id":"x","method":"ping","params":[]}),
                -32602,
            ),
            (json!({"jsonrpc":"2.0","id":7,"method":"unknown"}), -32601),
        ] {
            let body = json_body(app.clone().oneshot(request(payload)).await.unwrap()).await;
            assert_eq!(body["error"]["code"], code);
        }
    }

    #[tokio::test]
    async fn tools_are_annotated_and_workspace_results_are_bounded_structures() {
        let temp = TempDir::new().unwrap();
        let config = test_config(&temp);
        let database = Database::open_config(&config).unwrap();
        database.create_workspace("Fixture", temp.path()).unwrap();
        let app = router(config);

        let listed = json_body(
            app.clone()
                .oneshot(request(
                    json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
                ))
                .await
                .unwrap(),
        )
        .await;
        let tools = listed["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), TOOL_NAMES.len());
        let stop = tools
            .iter()
            .find(|tool| tool["name"] == "terminal_stop")
            .unwrap();
        assert_eq!(stop["annotations"]["destructiveHint"], true);
        assert_eq!(stop["annotations"]["idempotentHint"], true);
        assert_eq!(stop["inputSchema"]["additionalProperties"], false);

        let call = json_body(
            app.oneshot(request(json!({
                "jsonrpc":"2.0","id":"list","method":"tools/call",
                "params":{"name":"workspace_list","arguments":{}}
            })))
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(call["id"], "list");
        assert_eq!(call["result"]["isError"], false);
        assert_eq!(
            call["result"]["structuredContent"]["workspaces"][0]["name"],
            "Fixture"
        );
        assert_eq!(
            call["result"]["structuredContent"]["workspaces"][0]["terminals"],
            json!([])
        );
    }

    #[tokio::test]
    async fn unknown_tools_are_rpc_errors_and_known_tool_validation_is_error_result() {
        let temp = TempDir::new().unwrap();
        let app = router(test_config(&temp));
        let unknown = json_body(
            app.clone()
                .oneshot(request(json!({
                    "jsonrpc":"2.0","id":41,"method":"tools/call",
                    "params":{"name":"not_a_tool","arguments":{}}
                })))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(unknown["id"], 41);
        assert_eq!(unknown["error"]["code"], -32602);

        let invalid = json_body(
            app.clone()
                .oneshot(request(json!({
                    "jsonrpc":"2.0","id":42,"method":"tools/call",
                    "params":{"name":"terminal_create","arguments":{"workspace_id":0,"name":"valid-name"}}
                })))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(invalid["result"]["isError"], true);
        assert!(
            invalid["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("absolute folder path")
        );

        let database = Database::open(&temp.path().join("state.db")).unwrap();
        assert!(database.list_terminals(None).unwrap().is_empty());
    }

    #[tokio::test]
    async fn terminal_creation_rolls_back_reservation_when_tmux_setup_fails() {
        let temp = TempDir::new().unwrap();
        let mut config = test_config(&temp);
        let database = Database::open_config(&config).unwrap();
        let workspace = database.create_workspace("Fixture", temp.path()).unwrap();
        config.tmux_socket = "relative-tmux.sock".into();
        let app = router(config);
        let response = json_body(
            app.oneshot(request(json!({
                "jsonrpc":"2.0","id":51,"method":"tools/call",
                "params":{"name":"terminal_create","arguments":{"workspace_id":workspace.path,"name":"rollback-test"}}
            })))
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(response["result"]["isError"], true);
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("absolute path")
        );

        let database = Database::open(&temp.path().join("state.db")).unwrap();
        assert!(database.list_terminals(None).unwrap().is_empty());
    }

    #[test]
    fn utf8_truncation_preserves_character_boundaries_and_accept_quality_is_honored() {
        let (value, truncated) = truncate_utf8("ab界cd".to_owned(), 4);
        assert_eq!(value, "ab");
        assert!(truncated);

        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/json;q=0.0, text/event-stream"),
        );
        assert!(!accepts_json(&headers));
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/json;q=0.5, text/event-stream"),
        );
        assert!(accepts_json(&headers));
    }
}

#[cfg(test)]
mod path_command_tests {
    use super::*;
    use tempfile::TempDir;

    fn config(temp: &TempDir) -> Config {
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        Config {
            database_path: temp.path().join("db.sqlite"),
            workspace_roots: vec![root],
            ..Config::default()
        }
    }

    #[test]
    fn workspace_canonical_identity_and_duplicate_basename() {
        let temp = TempDir::new().unwrap();
        let config = config(&temp);
        let a = temp.path().join("root/a/app");
        let b = temp.path().join("root/b/app");
        let args = json!({"workspace_id":a});
        let first = workspace_arg(&config, args.as_object().unwrap(), true)
            .unwrap()
            .unwrap();
        let second = workspace_arg(&config, args.as_object().unwrap(), true)
            .unwrap()
            .unwrap();
        assert_eq!(first.id, second.id);
        let other = workspace_arg(
            &config,
            json!({"workspace_id":b}).as_object().unwrap(),
            true,
        )
        .unwrap()
        .unwrap();
        assert_ne!(first.id, other.id);
        assert_ne!(first.name, other.name);
        std::os::unix::fs::symlink(&a, temp.path().join("root/alias")).unwrap();
        let alias = workspace_arg(
            &config,
            json!({"workspace_id":temp.path().join("root/alias")})
                .as_object()
                .unwrap(),
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(alias.id, first.id);
    }

    #[test]
    fn workspace_rejects_escape_before_creating_folders() {
        let temp = TempDir::new().unwrap();
        let config = config(&temp);
        let denied = temp.path().join("outside/missing");
        assert!(
            workspace_arg(
                &config,
                json!({"workspace_id":denied}).as_object().unwrap(),
                true
            )
            .is_err()
        );
        assert!(!denied.exists());
        for raw in [
            "relative".to_owned(),
            temp.path().join("root/../escape").display().to_string(),
        ] {
            assert!(
                workspace_arg(
                    &config,
                    json!({"workspace_id":raw}).as_object().unwrap(),
                    true
                )
                .is_err()
            );
        }
        std::os::unix::fs::symlink(temp.path(), temp.path().join("root/escape-link")).unwrap();
        assert!(
            workspace_arg(
                &config,
                json!({"workspace_id":temp.path().join("root/escape-link/new")})
                    .as_object()
                    .unwrap(),
                true
            )
            .is_err()
        );
        assert!(!temp.path().join("new").exists());
    }

    #[test]
    fn preview_is_exact_unicode_first_500_last_1500() {
        for len in [0, 100, 2000, 2001, 10000] {
            let text = "🙂".repeat(len);
            let mut value = json!({"output":text});
            limit_output(&mut value, false);
            assert_eq!(
                value["output"].as_str().unwrap().chars().count(),
                len.min(2000)
            );
            assert_eq!(value["omitted_chars"], len.saturating_sub(2000));
        }
        let text = format!(
            "{}{}{}",
            "a".repeat(500),
            "M".repeat(6000),
            "z".repeat(1500)
        );
        let mut value = json!({"output":text});
        limit_output(&mut value, false);
        assert_eq!(
            value["output"],
            format!("{}{}", "a".repeat(500), "z".repeat(1500))
        );
        let mut value = json!({"output":text});
        limit_output(&mut value, true);
        assert_eq!(value["output"], text);
        assert_eq!(value["output_truncated"], false);
    }

    #[test]
    fn concurrent_workspace_registration_is_unique() {
        let temp = TempDir::new().unwrap();
        let config = config(&temp);
        let path = temp.path().join("root/shared");
        fs::create_dir(&path).unwrap();
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let config = config.clone();
                let path = path.clone();
                std::thread::spawn(move || {
                    workspace_arg(
                        &config,
                        json!({"workspace_id":path}).as_object().unwrap(),
                        true,
                    )
                    .unwrap()
                    .unwrap()
                    .id
                })
            })
            .collect();
        let ids: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert!(ids.iter().all(|id| *id == ids[0]));
        assert_eq!(
            Database::open_config(&config)
                .unwrap()
                .list_workspaces()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn advertised_commands_wait_twenty_seconds_and_scope_all_terminals() {
        for tool in tool_definitions() {
            let name = tool["name"].as_str().unwrap();
            if name.starts_with("terminal_") || name == "bash" || name == "python" {
                assert_eq!(
                    tool["inputSchema"]["properties"]["workspace_id"]["type"],
                    "string"
                );
                assert!(
                    tool["inputSchema"]["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("workspace_id"))
                );
            }
            if name == "bash" || name == "python" {
                assert_eq!(tool["inputSchema"]["properties"]["wait_s"]["default"], 20);
                assert_eq!(
                    tool["inputSchema"]["properties"]["full_output"]["default"],
                    false
                );
            }
        }
    }
}

#[cfg(test)]
mod task_tracking_tests {
    use super::*;
    #[test]
    fn tracked_schema_is_required() {
        for d in tool_definitions() {
            if matches!(
                d["name"].as_str(),
                Some("bash" | "python" | "terminal_read" | "terminal_capture" | "terminal_write")
            ) {
                for key in ["task", "summary", "workspace_id"] {
                    assert!(
                        d["inputSchema"]["required"]
                            .as_array()
                            .unwrap()
                            .contains(&json!(key))
                    );
                }
            }
        }
    }
    #[test]
    fn progress_and_word_limits() {
        for n in [0, 35, 100] {
            assert!(
                validate_tracking(
                    &json!("Query preview"),
                    &json!(format!("{n}/100 Checking paths"))
                )
                .is_ok()
            );
        }
        for text in [
            "101/100 Checking",
            "-1/100 Checking",
            "25/100",
            "35/100 two\nlines",
        ] {
            assert!(validate_tracking(&json!("Task"), &json!(text)).is_err());
        }
        assert!(
            validate_tracking(
                &json!("Task"),
                &json!(format!("35/100 {}", "word ".repeat(20)))
            )
            .is_err()
        );
    }
    #[test]
    fn workspace_numeric_and_missing_fail() {
        let c = Config::default();
        assert!(
            execute_tool(
                &c,
                "terminal_list",
                &serde_json::from_value(json!({"workspace_id":1})).unwrap()
            )
            .is_err()
        );
        assert!(execute_tool(&c, "terminal_list", &Map::new()).is_err());
    }
}
