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
