// Query routing is evaluated before route handlers (including /mcp and /assets).
async fn query_port_dispatch(State(state): State<AppState>, mut request: Request<Body>, next: axum::middleware::Next) -> Response {
    if request.headers().contains_key(crate::subdomain_proxy::PORT_HEADER) {
        if !bearer_authorized(&state,request.headers()) {return StatusCode::UNAUTHORIZED.into_response();}
        let port=request.headers().get(crate::subdomain_proxy::PORT_HEADER).and_then(|v|v.to_str().ok()).and_then(|s|s.parse::<u16>().ok());
        let Some(port)=port.filter(|p|crate::subdomain_proxy::valid_port(*p,state.config.listen.port())) else {return (StatusCode::FORBIDDEN,"Preview port is invalid or reserved").into_response();};
        let path=request.uri().path_and_query().map(|v|v.as_str()).unwrap_or("/").to_string();
        request.extensions_mut().insert(crate::query_proxy::Target{port,path});
        request.extensions_mut().insert(crate::subdomain_proxy::HostPreview);
        return browser_port_proxy(State(state),request).await;
    }
    let selected=match crate::query_proxy::selected(request.uri(),request.headers()) {
        Ok(p)=>p,Err(e)=>return (StatusCode::BAD_REQUEST,e).into_response(),
    };
    let secure=request.headers().get("x-forwarded-proto").is_some_and(|v|v=="https");
    let explicit=crate::query_proxy::split_query(request.uri().query()).ok().and_then(|v|v.0).is_some();
    let websocket=request.headers().get("upgrade").is_some_and(|v|v=="websocket");
    if let Some(port) = selected {
        if port==0 {
            if let Ok(path)=crate::query_proxy::upstream(request.uri()) {if let Ok(uri)=path.parse(){*request.uri_mut()=uri;}}
            let mut response=next.run(request).await;
            if explicit && !websocket {response.headers_mut().append(header::SET_COOKIE,HeaderValue::from_str(&crate::query_proxy::cookie(0,secure)).unwrap());}
            return response;
        }
        let path=match crate::query_proxy::upstream(request.uri()){Ok(p)=>p,Err(e)=>return (StatusCode::BAD_REQUEST,e).into_response()};
        request.extensions_mut().insert(crate::query_proxy::Target {port,path});
        let mut response=browser_port_proxy(State(state),request).await;
        if response.headers().contains_key("x-webterm-proxy") {
            response.headers_mut().append(header::SET_COOKIE,HeaderValue::from_str(&crate::query_proxy::cookie(port,secure)).unwrap());
        }
        return response;
    }
    next.run(request).await
}

// These handlers are included in web.rs to reuse its session and CSRF checks.
async fn monitor_js() -> Response {
    html_response(include_str!("../web/process-monitor.js"), "text/javascript; charset=utf-8")
}
async fn monitor_css() -> Response {
    html_response(include_str!("../web/process-monitor.css"), "text/css; charset=utf-8")
}

fn bearer_authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(expected) = state.config.auth_token.as_deref() else { return false; };
    headers.get(header::AUTHORIZATION).and_then(|v|v.to_str().ok())
        .and_then(|v|v.strip_prefix("Bearer "))
        .is_some_and(|value| bool::from(value.as_bytes().ct_eq(expected.as_bytes())))
}

async fn browser_port_proxy(State(state): State<AppState>, mut request: Request<Body>) -> Response {
    let bearer = bearer_authorized(&state, request.headers());
    let session = current_session(&state, request.headers());
    if !bearer && session.is_none() {
        return (StatusCode::UNAUTHORIZED, "Sign in to WebTerm before opening a runtime port").into_response();
    }
    let headers = request.headers();
    if headers.get("sec-fetch-site").is_some_and(|s| s == "cross-site")
        || (headers.contains_key(header::ORIGIN) && !same_origin(headers)) {
        return (StatusCode::FORBIDDEN, "Cross-origin proxy request rejected").into_response();
    }
    if !bearer && !matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS)
        && !headers.contains_key(header::ORIGIN) {
        return (StatusCode::FORBIDDEN, "Same-origin request required").into_response();
    }
    if let Some((_, csrf, _)) = session {
        if valid_csrf(request.headers(), &csrf) { request.headers_mut().remove("x-csrf-token"); }
    }
    crate::port_proxy::forward(request, state.config.listen.port()).await
}

fn monitor_response(value: serde_json::Value) -> Response {
    let mut response = if let Some(error) = value.get("error").and_then(|v|v.as_str()) {
        let status = match value.get("status").and_then(|v|v.as_u64()) {
            Some(403) => StatusCode::FORBIDDEN, Some(404) => StatusCode::NOT_FOUND,
            _ => StatusCode::BAD_REQUEST,
        };
        (status, Json(serde_json::json!({"error":error}))).into_response()
    } else { Json(value.get("data").cloned().unwrap_or(serde_json::Value::Null)).into_response() };
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn monitor_task<F>(operation: F) -> Response
where F: FnOnce() -> anyhow::Result<serde_json::Value> + Send + 'static {
    match tokio::task::spawn_blocking(operation).await {
        Ok(Ok(value)) => monitor_response(value),
        _ => (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error":"Process metrics unavailable; retry shortly"}))).into_response(),
    }
}

async fn browser_processes(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if current_session(&state, &headers).is_none() { return StatusCode::UNAUTHORIZED.into_response(); }
    let monitor = state.processes.clone();
    monitor_task(move || monitor.snapshot()).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessIdentity { start_time: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessAction { start_time: String, action: String, nice: Option<i32> }

async fn browser_process_detail(State(state): State<AppState>, Path(pid): Path<u32>, Query(identity): Query<ProcessIdentity>, headers: HeaderMap) -> Response {
    if current_session(&state, &headers).is_none() { return StatusCode::UNAUTHORIZED.into_response(); }
    monitor_task(move || crate::processes::collect(serde_json::json!({"operation":"detail", "pid":pid, "start_time":identity.start_time}))).await
}

async fn browser_process_action(State(state): State<AppState>, Path(pid): Path<u32>, headers: HeaderMap, Json(action): Json<ProcessAction>) -> Response {
    if let Some(rejection) = mutation_rejection(&state, &headers) { return rejection; }
    monitor_task(move || crate::processes::collect(serde_json::json!({"operation":"action", "pid":pid, "start_time":action.start_time, "action":action.action, "nice":action.nice}))).await
}

#[cfg(test)]
mod extension_tests {
    use super::*;
    use tower::ServiceExt;
    fn app() -> Router {
        router_with_auth(Config { auth_token: Some("proxy-test-token-is-long-enough".into()), ..Config::default() },None,Duration::from_secs(600))
    }
    #[tokio::test]
    async fn monitor_routes_require_browser_session() {
        for uri in ["/api/v1/processes", "/api/v1/processes/123?start_time=1"] {
            let response=app().oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(),StatusCode::UNAUTHORIZED);
        }
    }
    #[tokio::test]
    async fn process_actions_require_origin_and_csrf() {
        let response=app().oneshot(Request::builder().method("POST").uri("/api/v1/processes/123/action")
            .header("content-type","application/json").body(Body::from(r#"{"start_time":"1","action":"kill"}"#)).unwrap()).await.unwrap();
        assert_eq!(response.status(),StatusCode::FORBIDDEN);
    }
    #[tokio::test]
    async fn proxy_requires_auth_then_rejects_own_port_and_cross_origin() {
        let response=app().oneshot(Request::builder().uri("/proxy/10000/").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(),StatusCode::UNAUTHORIZED);
        let response=app().oneshot(Request::builder().uri("/proxy/10000/").header("authorization","Bearer proxy-test-token-is-long-enough").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(),StatusCode::FORBIDDEN);
        let response=app().oneshot(Request::builder().uri("/proxy/3000/").header("authorization","Bearer proxy-test-token-is-long-enough")
            .header("host","good.example").header("origin","https://bad.example").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(),StatusCode::FORBIDDEN);
    }
    #[tokio::test]
    async fn proxy_validates_ports_and_preserves_redirect_query() {
        for port in ["0","65536","-2","oops","12@remote"] {
            let response=app().oneshot(Request::builder().uri(format!("/proxy/{port}/")).header("authorization","Bearer proxy-test-token-is-long-enough").body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(),StatusCode::BAD_REQUEST);
        }
        let response=app().oneshot(Request::builder().uri("/proxy/3000?a=%2F&a=2").header("authorization","Bearer proxy-test-token-is-long-enough").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(),StatusCode::PERMANENT_REDIRECT);
        assert_eq!(response.headers().get("location").unwrap(),"/proxy/3000/?a=%2F&a=2");
    }
}

// This read-only private API advertises only user-owned listeners, not all system ports.
async fn preview_ports(State(state):State<AppState>,headers:HeaderMap)->Response {
 if !bearer_authorized(&state,&headers){return StatusCode::UNAUTHORIZED.into_response();}
 let monitor=state.processes.clone();let self_port=state.config.listen.port();
 match tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u16>> {
  let sample=monitor.snapshot()?;let uid=unsafe{libc::geteuid()} as u64;let mut ports=std::collections::BTreeSet::new();
  if let Some(rows)=sample["data"]["processes"].as_array(){for row in rows {
   if row["uid"].as_u64()!=Some(uid) || row["can_control"]!=true {continue;}
   if let Some(items)=row["ports"].as_array(){for p in items {if p["protocol"]=="tcp" {if let Some(port)=p["port"].as_u64().and_then(|n|u16::try_from(n).ok()).filter(|p|crate::subdomain_proxy::valid_port(*p,self_port)) {ports.insert(port);}}}}
  }} Ok(ports.into_iter().collect())
 }).await {Ok(Ok(ports))=>Json(serde_json::json!({"ports":ports})).into_response(),_=>StatusCode::SERVICE_UNAVAILABLE.into_response()}
}
