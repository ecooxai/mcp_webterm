// Included in web.rs: reuse the existing browser session and bearer authorization.
#[derive(Deserialize)]
struct ExplorerQuery {
    workspace_id: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    offset: usize,
}
fn explorer_authorized(state: &AppState, headers: &HeaderMap) -> bool {
    current_session(state, headers).is_some() || bearer_authorized(state, headers)
}
async fn explorer_js() -> Response {
    html_response(
        include_str!("../web/explorer.js"),
        "text/javascript; charset=utf-8",
    )
}
async fn terminal_links_js() -> Response {
    html_response(
        include_str!("../web/terminal-links.js"),
        "text/javascript; charset=utf-8",
    )
}
async fn explorer_css() -> Response {
    html_response(
        include_str!("../web/explorer.css"),
        "text/css; charset=utf-8",
    )
}
async fn logs_js() -> Response {
    html_response(
        include_str!("../web/tool-log.js"),
        "text/javascript; charset=utf-8",
    )
}
async fn model_viewer_js() -> Response {
    html_response(
        include_str!("../web/vendor/model-viewer.min.js"),
        "text/javascript; charset=utf-8",
    )
}
async fn explorer_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ExplorerQuery>,
) -> Response {
    if !explorer_authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let config = state.config.clone();
    let mut r = api_task(
        move || crate::workspace_files::listing(&config, &q.workspace_id, &q.path, q.offset),
        StatusCode::OK,
    )
    .await;
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}
async fn explorer_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ExplorerQuery>,
) -> Response {
    if !explorer_authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let config = state.config.clone();
    let mut r = api_task(
        move || crate::workspace_files::preview(&config, &q.workspace_id, &q.path),
        StatusCode::OK,
    )
    .await;
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}
#[derive(Deserialize)]
struct ResolveQuery {
    workspace_id: String,
    #[serde(default)]
    terminal_id: Option<i64>,
    text: String,
}
async fn explorer_resolve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ResolveQuery>,
) -> Response {
    if !explorer_authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let config = state.config.clone();
    let mut r = api_task(
        move || {
            // Relative paths printed by a program are relative to its cwd, not the workspace.
            let cwd = q.terminal_id.and_then(|id| {
                let terminal = Database::open_config(&config).ok()?.terminal_by_id(id).ok()?;
                if terminal.backend() != "native-pty" {
                    return None;
                }
                let info = TerminalManager::new(&config).ok()?.native().info(&terminal.tmux_session).ok()?;
                crate::workspace_files::foreground_cwd(info.pid?)
            });
            crate::workspace_files::resolve(&config, &q.workspace_id, cwd.as_deref(), &q.text)
        },
        StatusCode::OK,
    )
    .await;
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}
#[derive(Deserialize)]
struct ToolsQuery {
    workspace_id: String,
    #[serde(default)]
    path: String,
}
fn no_store(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}
async fn workspace_activity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ToolsQuery>,
) -> Response {
    if !explorer_authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let config = state.config.clone();
    let monitor = state.processes.clone();
    let self_port = state.config.listen.port();
    no_store(
        api_task(
            move || {
                let db = Database::open_config(&config)?;
                let workspace = db.workspace(&q.workspace_id)?;
                let manager = TerminalManager::new(&config)?;
                let shells = db
                    .list_terminals(Some(workspace.id))?
                    .into_iter()
                    .filter(|t| t.backend() == "native-pty")
                    .filter_map(|t| {
                        let pid = manager.native().info(&t.tmux_session).ok()?.pid?;
                        Some((t.id, t.name, pid))
                    })
                    .collect::<Vec<_>>();
                let sample = monitor.snapshot()?;
                let rows = sample["data"]["processes"].as_array().cloned().unwrap_or_default();
                crate::workspace_tools::activity(&config, &workspace.id.to_string(), &shells, &rows, self_port)
            },
            StatusCode::OK,
        )
        .await,
    )
}
async fn workspace_git_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ToolsQuery>,
) -> Response {
    if !explorer_authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let config = state.config.clone();
    no_store(
        api_task(
            move || crate::workspace_tools::git_status(&config, &q.workspace_id),
            StatusCode::OK,
        )
        .await,
    )
}
async fn workspace_git_diff(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ToolsQuery>,
) -> Response {
    if !explorer_authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let config = state.config.clone();
    no_store(
        api_task(
            move || crate::workspace_tools::git_diff(&config, &q.workspace_id, &q.path),
            StatusCode::OK,
        )
        .await,
    )
}
async fn workspace_tools_js() -> Response {
    html_response(
        include_str!("../web/workspace-tools.js"),
        "text/javascript; charset=utf-8",
    )
}
async fn explorer_raw(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((workspace, path)): Path<(String, String)>,
) -> Response {
    if !explorer_authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let config = state.config.clone();
    let opened = tokio::task::spawn_blocking(move || {
        crate::workspace_files::open(&config, &workspace, &path)
    })
    .await;
    let (file, canonical, _) = match opened {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return api_error(e),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let length = match file.metadata() {
        Ok(m) if m.is_file() => m.len(),
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let range = match crate::workspace_files::byte_range(
        headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
        length,
    ) {
        Ok(v) => v,
        Err(_) => {
            let mut r = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            r.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{length}")).unwrap(),
            );
            return r;
        }
    };
    let (start, count) = range.map(|(s, e)| (s, e - s + 1)).unwrap_or((0, length));
    let mut file = tokio::fs::File::from_std(file);
    if tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(start))
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let stream = tokio_util::io::ReaderStream::with_capacity(
        tokio::io::AsyncReadExt::take(file, count),
        64 * 1024,
    );
    let mut response = Body::from_stream(stream).into_response();
    *response.status_mut() = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let (_, mime) = crate::workspace_files::kind(&canonical);
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&count.to_string()).unwrap(),
    );
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    // Static workspace documents cannot run scripts, navigate the parent, submit forms, or make network API calls.
    h.insert("content-security-policy",HeaderValue::from_static("sandbox allow-same-origin; default-src 'none'; script-src 'none'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; font-src 'self' data:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'"));
    if let Some((s, e)) = range {
        h.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {s}-{e}/{length}")).unwrap(),
        );
    }
    response
}
async fn browser_tool_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<crate::audit::Query>,
) -> Response {
    if !explorer_authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let c = state.config.clone();
    let mut r = api_task(move || crate::audit::page(&c, &q), StatusCode::OK).await;
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}
async fn browser_tool_log_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if !explorer_authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let c = state.config.clone();
    let mut r = api_task(move || crate::audit::detail(&c, id), StatusCode::OK).await;
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}
#[cfg(test)]
mod explorer_route_tests {
    use super::*;
    use tower::ServiceExt;
    #[tokio::test]
    async fn private_endpoints_require_auth() {
        for url in [
            "/api/v1/files/list?workspace_id=1",
            "/api/v1/files/preview?workspace_id=1&path=a",
            "/api/v1/files/raw/1/a",
            "/api/v1/files/resolve?workspace_id=1&text=a",
            "/api/v1/tool-logs",
            "/api/v1/tool-logs/1",
        ] {
            let app = router_with_auth(Config::default(), None, Duration::from_secs(600));
            let r = app
                .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{url}");
        }
    }
}

#[derive(serde::Deserialize)]
struct ShareQuery {
    #[serde(default)]
    workspace_id: String,
    path: String,
}

fn encode_segment(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'~' | b'-' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// Signed-in users mint a link to a file's folder; files outside workspaces must sit under workspace_roots.
async fn explorer_share(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ShareQuery>,
) -> Response {
    if !explorer_authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let config = state.config.clone();
    let shared = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        use anyhow::Context as _;
        let canonical = if q.workspace_id.is_empty() {
            let path = PathBuf::from(&q.path).canonicalize().context("file not found")?;
            if !config.workspace_roots.iter().any(|root| path.starts_with(root)) {
                anyhow::bail!("path is outside the allowed roots");
            }
            path
        } else {
            crate::workspace_files::open(&config, &q.workspace_id, &q.path)?.1
        };
        if !canonical.is_file() {
            anyhow::bail!("only files can be opened by link");
        }
        let folder = canonical.parent().context("file has no folder")?;
        let token = crate::share::create(folder);
        let name = canonical.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(serde_json::json!({
            "url": format!("/share/{token}/{}", encode_segment(&name)),
            "expires_in_s": crate::share::TTL.as_secs(),
        }))
    })
    .await;
    match shared {
        Ok(Ok(value)) => no_store(Json(value).into_response()),
        Ok(Err(error)) => api_error(error),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

// Public by design: the unguessable token is the credential. Scripts run in an opaque sandboxed origin,
// so a shared page can never read WebTerm cookies or call its API.
async fn share_file(Path((token, path)): Path<(String, String)>) -> Response {
    let Some(folder) = crate::share::folder(&token) else {
        return (StatusCode::NOT_FOUND, "Link expired or unknown").into_response();
    };
    let opened = tokio::task::spawn_blocking(move || -> anyhow::Result<(std::fs::File, PathBuf, u64)> {
        use std::os::unix::fs::OpenOptionsExt;
        let canonical = folder.join(&path).canonicalize()?;
        if !canonical.starts_with(&folder) {
            anyhow::bail!("path escapes shared folder");
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&canonical)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            anyhow::bail!("not a file");
        }
        Ok((file, canonical, meta.len()))
    })
    .await;
    let Ok(Ok((file, canonical, length))) = opened else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let stream = tokio_util::io::ReaderStream::with_capacity(tokio::fs::File::from_std(file), 64 * 1024);
    let mut response = Body::from_stream(stream).into_response();
    let (_, mime) = crate::workspace_files::kind(&canonical);
    let h = response.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&length.to_string()).unwrap());
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("sandbox allow-scripts allow-forms allow-popups allow-modals allow-downloads"),
    );
    response
}
