//! User-selectable reverse proxy served on port 1080.
//! Reserved routes: /mpxx (controller) and /pmurl (target API).
//! Root-relative paths proxy the selected origin; legacy /proxy paths redirect to clean URLs.
use std::{env, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri, Version, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{any, get},
};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use url::Url;

pub const PROXY_PORT: u16 = 1080;
const DEFAULT_TARGET: &str = "https://www.google.com/";

type ProxyClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;

#[derive(Clone)]
struct ProxyState {
    target: Arc<RwLock<Url>>,
    client: ProxyClient,
    forward: Arc<crate::forward_proxy::ForwardProxy>,
}

#[derive(Serialize)]
struct TargetReply {
    url: String,
}

#[derive(Deserialize)]
struct SetTarget {
    url: String,
}

pub fn router() -> Result<Router> {
    let configured = env::var("WEBTERM_PROXY_TARGET").unwrap_or_else(|_| DEFAULT_TARGET.to_owned());
    let target = parse_target(&configured)?;
    let https = HttpsConnectorBuilder::new()
        .with_native_roots()
        .context("load native TLS roots for port 1080 proxy")?
        .https_or_http()
        .enable_http1()
        .build();
    let client = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(60))
        .build(https);
    let state = ProxyState {
        target: Arc::new(RwLock::new(target)),
        client,
        forward: Arc::new(crate::forward_proxy::ForwardProxy::from_env()?),
    };

    Ok(Router::new()
        .route("/", any(proxy_request))
        .route("/mpxx", get(controller))
        .route("/mpxx/", get(controller))
        .route("/mpxx/proxy.pac", get(proxy_pac))
        .route("/m", get(|| async { Redirect::temporary("/mpxx") }))
        .route("/pmurl", get(get_target).put(set_target).post(set_target))
        .route("/proxy", any(legacy_redirect))
        .route("/proxy/", any(legacy_redirect))
        .route("/proxy/{*path}", any(legacy_redirect))
        .fallback(any(proxy_request))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            forward_dispatch,
        ))
        .with_state(state))
}

fn parse_target(raw: &str) -> Result<Url> {
    let mut url = Url::parse(raw.trim()).context("proxy target must be an absolute URL")?;
    match url.scheme() {
        "http" | "https" | "ws" | "wss" => {}
        _ => anyhow::bail!("proxy target scheme must be http, https, ws, or wss"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("Do not put credentials in the target URL");
    }
    if url.host_str().is_none() {
        anyhow::bail!("proxy target must include a host");
    }
    url.set_fragment(None);
    Ok(url)
}

async fn get_target(State(state): State<ProxyState>) -> Response {
    let url = state.target.read().await.to_string();
    ([("cache-control", "no-store")], Json(TargetReply { url })).into_response()
}

async fn set_target(State(state): State<ProxyState>, Json(input): Json<SetTarget>) -> Response {
    match parse_target(&input.url) {
        Ok(url) => {
            let text = url.to_string();
            *state.target.write().await = url;
            (
                StatusCode::OK,
                [("cache-control", "no-store")],
                Json(TargetReply { url: text }),
            )
                .into_response()
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            [("cache-control", "no-store")],
            err.to_string(),
        )
            .into_response(),
    }
}

async fn controller() -> Response {
    let mut response = Html(CONTROLLER_HTML).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    response.headers_mut().insert(
        "permissions-policy",
        HeaderValue::from_static(
            "camera=*, microphone=*, geolocation=*, fullscreen=*, display-capture=*, autoplay=*, clipboard-read=*, clipboard-write=*",
        ),
    );
    response.headers_mut().insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; frame-src 'self'; connect-src 'self'; img-src 'self' data: blob:; media-src 'self' blob:",
        ),
    );
    response
}

async fn proxy_request(State(state): State<ProxyState>, mut request: Request<Body>) -> Response {
    let target = state.target.read().await.clone();
    let downstream_uri = request.uri().clone();
    let navigation = is_navigation_request(&request, &downstream_uri);
    let upstream_url = match upstream_url(&target, &downstream_uri) {
        Ok(url) => url,
        Err(err) => return (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
    };
    if matches!(*request.method(), Method::GET | Method::HEAD)
        && downstream_uri.path() == "/"
        && downstream_uri.query().is_none()
        && upstream_url.path() != "/"
    {
        return local_redirect(&local_url(&upstream_url));
    }
    let upstream_uri = match upstream_url.as_str().parse::<Uri>() {
        Ok(uri) => uri,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("build upstream request URI: {err}"),
            )
                .into_response();
        }
    };

    let websocket = request
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if request.headers().contains_key(header::UPGRADE) && !websocket {
        return (
            StatusCode::BAD_REQUEST,
            "Only WebSocket protocol upgrades are supported",
        )
            .into_response();
    }
    let downstream_upgrade = websocket.then(|| hyper::upgrade::on(&mut request));

    rewrite_request_headers(request.headers_mut(), &upstream_url, websocket);
    *request.uri_mut() = upstream_uri;
    *request.version_mut() = Version::HTTP_11;

    let mut upstream =
        match tokio::time::timeout(Duration::from_secs(60), state.client.request(request)).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("Upstream request failed: {err}"),
                )
                    .into_response();
            }
            Err(_) => {
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    "Upstream did not return response headers within 60 seconds",
                )
                    .into_response();
            }
        };

    let upgraded = upstream.status() == StatusCode::SWITCHING_PROTOCOLS;
    if upgraded && !websocket {
        return (
            StatusCode::BAD_GATEWAY,
            "Unexpected upstream protocol upgrade",
        )
            .into_response();
    }

    if upgraded {
        let upstream_upgrade = hyper::upgrade::on(&mut upstream);
        let downstream_upgrade = downstream_upgrade.expect("upgrade checked");
        tokio::spawn(async move {
            let pair = tokio::time::timeout(Duration::from_secs(15), async {
                Ok::<_, hyper::Error>((downstream_upgrade.await?, upstream_upgrade.await?))
            })
            .await;
            if let Ok(Ok((downstream, upstream))) = pair {
                let mut downstream = TokioIo::new(downstream);
                let mut upstream = TokioIo::new(upstream);
                let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
            }
        });
    }

    let redirect = upstream
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| resolve_redirect(&upstream_url, v));
    let is_redirect = upstream.status().is_redirection();
    rewrite_response_headers(upstream.headers_mut(), &target, upgraded);
    if let Some(mut next) = redirect {
        let local = local_url(&next);
        let same = same_origin(&next, &upstream_url);
        let mut retargeted = false;
        if is_redirect && navigation && !same {
            next.set_fragment(None);
            // A stale in-flight response must not overwrite a newly selected target.
            let mut active = state.target.write().await;
            if *active == target {
                *active = next;
                retargeted = true;
            }
        }
        if same || retargeted {
            if let Ok(value) = HeaderValue::from_str(&local) {
                upstream.headers_mut().insert(header::LOCATION, value);
            }
        }
    }

    let (parts, body) = upstream.into_parts();
    Response::from_parts(parts, Body::new(body))
}

fn upstream_url(target: &Url, downstream: &Uri) -> Result<Url> {
    let mut url = target.clone();
    let scheme = normalized_http_scheme(target);
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid upstream scheme"))?;
    let path = clean_legacy_path(downstream.path());
    if path != "/" || downstream.query().is_some() {
        // All browser paths are origin-root paths, not relative to a previous
        // redirect's path (e.g. /sorry/index/search was the wrong destination).
        url.set_path(&path);
        url.set_query(downstream.query());
    }
    // Preserve query encoding, repeated keys, and Google's own session tokens.
    // Never invent igu=1 or remove signed/verification-related parameters.
    url.set_fragment(None);
    Ok(url)
}
fn clean_legacy_path(path: &str) -> String {
    match path.strip_prefix("/proxy/") {
        Some(tail) => format!("/{}", tail.trim_start_matches('/')),
        None if path == "/proxy" => "/".into(),
        None => path.to_owned(),
    }
}
fn local_url(url: &Url) -> String {
    let mut value = format!("/{}", url.path().trim_start_matches('/'));
    if let Some(q) = url.query() {
        value.push('?');
        value.push_str(q);
    }
    if let Some(f) = url.fragment() {
        value.push('#');
        value.push_str(f);
    }
    value
}
fn local_redirect(location: &str) -> Response {
    let mut r = StatusCode::TEMPORARY_REDIRECT.into_response();
    if let Ok(v) = HeaderValue::from_str(location) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}
async fn legacy_redirect(request: Request<Body>) -> Response {
    let mut path = clean_legacy_path(request.uri().path());
    if let Some(q) = request.uri().query() {
        path.push('?');
        path.push_str(q);
    }
    local_redirect(&path)
}
async fn forward_dispatch(
    State(state): State<ProxyState>,
    request: Request<Body>,
    next: axum::middleware::Next,
) -> Response {
    if crate::forward_proxy::ForwardProxy::handles(&request) {
        state.forward.serve(request).await
    } else {
        next.run(request).await
    }
}

fn is_navigation_request(request: &Request<Body>, _uri: &Uri) -> bool {
    if request.headers().contains_key(header::UPGRADE) {
        return false;
    }
    if let Some(dest) = request
        .headers()
        .get("sec-fetch-dest")
        .and_then(|v| v.to_str().ok())
    {
        return matches!(dest, "document" | "iframe" | "frame");
    }
    if let Some(mode) = request
        .headers()
        .get("sec-fetch-mode")
        .and_then(|v| v.to_str().ok())
    {
        return mode == "navigate";
    }
    matches!(*request.method(), Method::GET | Method::HEAD)
}

fn resolve_redirect(current: &Url, value: &str) -> Option<Url> {
    let url = current.join(value).ok()?;
    match url.scheme() {
        "http" | "https" | "ws" | "wss" => Some(url),
        _ => None,
    }
}

fn rewrite_request_headers(headers: &mut HeaderMap, target: &Url, websocket: bool) {
    strip_hop_headers(headers, websocket);
    for name in [
        "proxy-authorization",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-forwarded-prefix",
    ] {
        headers.remove(name);
    }

    if let Some(authority) = target_authority(target)
        && let Ok(value) = HeaderValue::from_str(&authority)
    {
        headers.insert(header::HOST, value);
    }

    let cookies = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter(|p| p.split_once('=').is_some_and(|(n, _)| !control_cookie(n)))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("; ");
    headers.remove(header::COOKIE);
    if !cookies.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&cookies) {
            headers.insert(header::COOKIE, v);
        }
    }
    let target_origin = target_origin(target);
    if headers.contains_key(header::ORIGIN)
        && let Ok(value) = HeaderValue::from_str(&target_origin)
    {
        headers.insert(header::ORIGIN, value);
    }
    if headers.contains_key(header::REFERER) {
        let referer = format!("{}/", target_origin.trim_end_matches('/'));
        if let Ok(value) = HeaderValue::from_str(&referer) {
            headers.insert(header::REFERER, value);
        }
    }
    if is_loopback_target(target) {
        if let Ok(value) = HeaderValue::from_str(normalized_http_scheme(target)) {
            headers.insert("x-forwarded-proto", value);
        }
        if let Some(authority) = target_authority(target)
            && let Ok(value) = HeaderValue::from_str(&authority)
        {
            headers.insert("x-forwarded-host", value);
        }
        headers.insert("x-forwarded-prefix", HeaderValue::from_static(""));
    }
}

fn rewrite_response_headers(headers: &mut HeaderMap, _target: &Url, websocket: bool) {
    strip_hop_headers(headers, websocket);

    let cookies: Vec<String> = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(rewrite_cookie)
        .collect();
    headers.remove(header::SET_COOKIE);
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            headers.append(header::SET_COOKIE, value);
        }
    }

    headers.remove("x-frame-options");
    if let Some(csp) = headers
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .map(remove_frame_ancestors)
        && let Ok(value) = HeaderValue::from_str(&csp)
    {
        if csp.trim().is_empty() {
            headers.remove("content-security-policy");
        } else {
            headers.insert("content-security-policy", value);
        }
    }
    headers.remove("content-security-policy-report-only");
    headers.remove("permissions-policy");
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static(
            "camera=*, microphone=*, geolocation=*, fullscreen=*, display-capture=*, autoplay=*, clipboard-read=*, clipboard-write=*",
        ),
    );
    headers.insert("x-webterm-url-proxy", HeaderValue::from_static("root-v3"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

#[cfg(test)]
fn rewrite_location(value: &str, current: &Url) -> Option<String> {
    let resolved = resolve_redirect(current, value)?;
    same_origin(&resolved, current).then(|| local_url(&resolved))
}
fn control_cookie(name: &str) -> bool {
    crate::subdomain_proxy::reserved_cookie(name)
        || name.trim().starts_with("__webterm_")
        || name.trim() == "pmurl"
}
fn rewrite_cookie(value: &str) -> Option<String> {
    let mut pieces = value.split(';');
    let first = pieces.next()?.trim();
    let (name, _) = first.split_once('=')?;
    if control_cookie(name) {
        return None;
    }
    let mut kept = vec![first.to_owned()];
    for part in pieces {
        let key = part.trim().split('=').next()?.trim();
        if key.eq_ignore_ascii_case("domain") {
            continue;
        }
        kept.push(part.trim().to_owned());
    }
    // Preserve Secure, HttpOnly, SameSite, original Path, and cookie prefixes.
    // Downgrading Secure was unsafe and made SameSite=None cookies invalid.
    Some(kept.join("; "))
}

fn remove_frame_ancestors(value: &str) -> String {
    value
        .split(';')
        .map(str::trim)
        .filter(|part| !part.to_ascii_lowercase().starts_with("frame-ancestors"))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

fn strip_hop_headers(headers: &mut HeaderMap, websocket: bool) {
    let named: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();
    for name in named {
        if !(websocket && name == "upgrade") {
            headers.remove(name);
        }
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
    ] {
        headers.remove(name);
    }
    if websocket {
        headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    } else {
        headers.remove(header::UPGRADE);
    }
}

fn is_loopback_target(target: &Url) -> bool {
    target.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host == "127.0.0.1"
            || host == "::1"
            || host == "[::1]"
    })
}

fn normalized_http_scheme(target: &Url) -> &'static str {
    match target.scheme() {
        "https" | "wss" => "https",
        _ => "http",
    }
}

fn target_authority(target: &Url) -> Option<String> {
    let host = target.host_str()?;
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    match target.port() {
        Some(port) => Some(format!("{host}:{port}")),
        None => Some(host),
    }
}

fn target_origin(target: &Url) -> String {
    let scheme = normalized_http_scheme(target);
    match target_authority(target) {
        Some(authority) => format!("{scheme}://{authority}"),
        None => format!("{scheme}://localhost"),
    }
}

fn same_origin(a: &Url, b: &Url) -> bool {
    normalized_url_scheme(a) == normalized_url_scheme(b)
        && a.host_str().map(str::to_ascii_lowercase) == b.host_str().map(str::to_ascii_lowercase)
        && effective_port(a) == effective_port(b)
}

fn normalized_url_scheme(url: &Url) -> &str {
    match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        other => other,
    }
}

fn effective_port(url: &Url) -> Option<u16> {
    url.port_or_known_default()
}

async fn proxy_pac(headers: HeaderMap) -> Response {
    let Some(authority) = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<axum::http::uri::Authority>().ok())
    else {
        return (StatusCode::BAD_REQUEST, "A valid Host header is required").into_response();
    };
    let host = authority.host();
    let proxy = format!(
        "PROXY {}:{}",
        host,
        authority.port_u16().unwrap_or(PROXY_PORT)
    );
    let host_js = serde_json::to_string(host).unwrap();
    let proxy_js = serde_json::to_string(&proxy).unwrap();
    let body = format!(
        "// Configure proxy credentials separately. No secrets are embedded here.\nfunction FindProxyForURL(url, host) {{\n if (isPlainHostName(host) || host === {host_js} || host === 'localhost' || host === '::1' || shExpMatch(host, '127.*')) return 'DIRECT';\n return {proxy_js};\n}}\n"
    );
    (
        [
            ("content-type", "application/x-ns-proxy-autoconfig"),
            ("cache-control", "no-store"),
        ],
        body,
    )
        .into_response()
}

const CONTROLLER_HTML: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>WebTerm URL Proxy</title>
<style>
:root{color-scheme:dark;background:#0b0d10;color:#e8edf2;font-family:ui-sans-serif,system-ui,sans-serif}
*{box-sizing:border-box}
body{margin:0;padding:18px;min-height:100vh}
.panel{display:flex;gap:10px;align-items:flex-start;position:sticky;top:0;z-index:20;background:#0b0d10;padding-bottom:12px}
.urlbox{position:relative;flex:1;min-width:180px}
input{width:100%;background:#151a20;color:#fff;border:1px solid #333c47;border-radius:9px;padding:11px 38px 11px 12px;font-size:14px;outline:none}
input:focus{border-color:#70839a}
.chev{position:absolute;right:10px;top:8px;border:0;background:transparent;color:#aab6c2;font-size:20px;padding:2px 4px;cursor:pointer}
.dropdown{position:absolute;top:47px;left:0;right:0;max-height:310px;overflow:auto;background:#11161c;border:1px solid #303a45;border-radius:10px;box-shadow:0 12px 35px #0009;display:none;z-index:30}
.dropdown.open{display:block}
.option{display:flex;gap:10px;align-items:center;padding:10px 12px;cursor:pointer;border-bottom:1px solid #202832}
.option:last-child{border-bottom:0}
.option:hover,.option.active{background:#1c2530}
.badge{min-width:30px;height:30px;border-radius:8px;background:#252e39;display:grid;place-items:center;font-weight:800;font-size:12px}
.optiontext{min-width:0;flex:1}.name{font-weight:700}.value{font-size:12px;color:#9ca9b7;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
button.action{border:0;border-radius:9px;padding:11px 16px;font-weight:700;cursor:pointer;min-height:41px}
#status{font-size:12px;color:#9ca9b7;white-space:nowrap;padding-top:12px}
.quick{display:flex;flex-wrap:wrap;gap:7px;margin:0 0 12px}
.quick button{background:#171e26;color:#dce5ee;border:1px solid #303a45;border-radius:999px;padding:7px 11px;cursor:pointer}
.quick button:hover{background:#212c37}
details{margin:0 0 12px;line-height:1.6}
iframe{width:100%;height:calc(100vh - 150px);border:1px solid #303844;border-radius:10px;background:#fff}
@media(max-width:650px){body{padding:10px}.panel{gap:7px}.panel .action{padding:11px 12px}#status{display:none}.quick{gap:5px}.quick button{padding:6px 9px}iframe{height:calc(100vh - 140px)}}
</style>
</head>
<body>
<div class="panel">
  <div class="urlbox">
    <input id="url" autocomplete="off" autocapitalize="none" spellcheck="false" placeholder="Type youtube, bing, wikipedia, or a URL">
    <button class="chev" id="toggle" type="button" aria-label="Show saved sites">⌄</button>
    <div class="dropdown" id="dropdown" role="listbox"></div>
  </div>
  <button class="action" id="go">Proxy</button>
  <span id="status"></span>
</div>
<div class="quick" id="quick"></div>
<details>
<summary>Google verification behavior</summary>
<p>If Google redirects a search to <code>/sorry/...</code>, WebTerm now passes Google's actual verification response through the reverse preview instead of replacing it with a WebTerm help page.</p>
<p>The page is still being viewed on this server's origin, so a domain-bound Google reCAPTCHA may reject the proxy hostname. WebTerm does not rewrite Google CAPTCHA keys or claim to solve the challenge.</p>
</details>
<iframe id="preview"
  allow="camera *; microphone *; geolocation *; fullscreen *; display-capture *; autoplay *; clipboard-read *; clipboard-write *; encrypted-media *; picture-in-picture *; web-share *"
  allowfullscreen></iframe>
<script>
const u=document.getElementById('url'), f=document.getElementById('preview'), s=document.getElementById('status');
const dd=document.getElementById('dropdown'), quick=document.getElementById('quick');
const STORE='webterm.mpxx.savedUrls.v1';
const SHORTCUTS=[
  {key:'google',name:'Google',url:'https://www.google.com/',icon:'G'},
  {key:'youtube yt',name:'YouTube',url:'https://www.youtube.com/',icon:'YT'},
  {key:'bing microsoft',name:'Bing',url:'https://www.bing.com/',icon:'B'},
  {key:'wikipedia wiki',name:'Wikipedia',url:'https://en.wikipedia.org/',icon:'W'},
  {key:'github',name:'GitHub',url:'https://github.com/',icon:'GH'}
];
let active=-1;

function safeHistory(){
  try{
    const x=JSON.parse(localStorage.getItem(STORE)||'[]');
    return Array.isArray(x)?x.filter(v=>typeof v==='string'&&/^https?:\/\//i.test(v)).slice(0,20):[];
  }catch{return []}
}
function saveUrl(url){
  if(!/^https?:\/\//i.test(url))return;
  const next=[url,...safeHistory().filter(v=>v!==url)].slice(0,20);
  try{localStorage.setItem(STORE,JSON.stringify(next))}catch{}
}
function choices(query=''){
  const q=query.trim().toLowerCase();
  const out=[];
  for(const x of SHORTCUTS){
    if(!q||x.key.includes(q)||x.name.toLowerCase().includes(q)||x.url.toLowerCase().includes(q)) out.push({...x,type:'shortcut'});
  }
  for(const url of safeHistory()){
    if(out.some(x=>x.url===url))continue;
    let host=url;
    try{host=new URL(url).host}catch{}
    if(!q||url.toLowerCase().includes(q)||host.toLowerCase().includes(q)) out.push({name:host,url,icon:'↺',type:'saved'});
  }
  return out.slice(0,24);
}
function renderDropdown(force=false){
  const list=choices(force?'':u.value);
  dd.innerHTML='';
  active=-1;
  if(!list.length){dd.classList.remove('open');return}
  for(const [i,x] of list.entries()){
    const el=document.createElement('div'); el.className='option'; el.role='option';
    el.innerHTML='<div class="badge"></div><div class="optiontext"><div class="name"></div><div class="value"></div></div>';
    el.querySelector('.badge').textContent=x.icon||'↺';
    el.querySelector('.name').textContent=x.name+(x.type==='saved'?' · saved':'');
    el.querySelector('.value').textContent=x.url;
    el.addEventListener('mousedown',e=>{e.preventDefault(); choose(x.url)});
    dd.appendChild(el);
  }
  dd.classList.add('open');
}
function setActive(n){
  const els=[...dd.querySelectorAll('.option')];
  if(!els.length)return;
  active=(n+els.length)%els.length;
  els.forEach((el,i)=>el.classList.toggle('active',i===active));
  els[active].scrollIntoView({block:'nearest'});
}
function choose(url){
  u.value=url; dd.classList.remove('open'); u.focus();
}
function normalizeInput(v){
  v=v.trim();
  const exact=SHORTCUTS.find(x=>x.key.split(' ').includes(v.toLowerCase())||x.name.toLowerCase()===v.toLowerCase());
  if(exact)return exact.url;
  if(!/^[a-z][a-z0-9+.-]*:\/\//i.test(v)&&/^[\w.-]+\.[a-z]{2,}(?:[/:?#]|$)/i.test(v))return 'https://'+v;
  return v;
}
async function current(){
  const r=await fetch('/pmurl',{cache:'no-store'}), j=await r.json();
  u.value=j.url; saveUrl(j.url); renderQuick(); f.src='/';
}
async function apply(value){
  const target=normalizeInput(value??u.value);
  if(!target)return;
  s.textContent='saving…'; dd.classList.remove('open');
  try{
    const r=await fetch('/pmurl',{method:'PUT',headers:{'content-type':'application/json'},body:JSON.stringify({url:target})});
    const t=await r.text();
    if(!r.ok) throw new Error(t);
    const j=JSON.parse(t); u.value=j.url; saveUrl(j.url); renderQuick(); s.textContent='active';
    f.src='/?_mpxx='+Date.now();
  }catch(e){s.textContent=e.message}
}
function renderQuick(){
  quick.innerHTML='';
  for(const x of SHORTCUTS.slice(0,4)){
    const b=document.createElement('button'); b.type='button'; b.textContent=x.name;
    b.addEventListener('click',()=>apply(x.url)); quick.appendChild(b);
  }
}
document.getElementById('go').onclick=()=>apply();
document.getElementById('toggle').onclick=()=>{if(dd.classList.contains('open'))dd.classList.remove('open');else renderDropdown(true)};
u.addEventListener('focus',()=>renderDropdown(false));
u.addEventListener('input',()=>renderDropdown(false));
u.addEventListener('keydown',e=>{
  if(e.key==='ArrowDown'){e.preventDefault();if(!dd.classList.contains('open'))renderDropdown(false);setActive(active+1)}
  else if(e.key==='ArrowUp'){e.preventDefault();setActive(active-1)}
  else if(e.key==='Escape')dd.classList.remove('open');
  else if(e.key==='Enter'){
    const els=[...dd.querySelectorAll('.option')];
    if(dd.classList.contains('open')&&active>=0&&els[active]){e.preventDefault();choose(els[active].querySelector('.value').textContent)}
    else apply();
  }
});
document.addEventListener('mousedown',e=>{if(!e.target.closest('.urlbox'))dd.classList.remove('open')});
current().catch(e=>s.textContent=e.message);
</script>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_supported_schemes() {
        for u in [
            "http://localhost:3000",
            "https://example.com",
            "ws://localhost/ws",
            "wss://example.com/ws",
        ] {
            assert!(parse_target(u).is_ok());
        }
        for u in ["ftp://example.com", "https://user:secret@example.com/"] {
            assert!(parse_target(u).is_err());
        }
    }
    #[test]
    fn root_paths_never_append_to_previous_redirects() {
        let t = parse_target("https://www.google.com/sorry/index?continue=x").unwrap();
        assert_eq!(
            upstream_url(&t, &"/search?q=rust".parse().unwrap())
                .unwrap()
                .as_str(),
            "https://www.google.com/search?q=rust"
        );
        assert_eq!(
            upstream_url(&t, &"/api/x".parse().unwrap())
                .unwrap()
                .as_str(),
            "https://www.google.com/api/x"
        );
        assert_eq!(upstream_url(&t, &"/".parse().unwrap()).unwrap(), t);
    }
    #[test]
    fn queries_and_verification_tokens_are_not_modified() {
        let t = parse_target("https://www.google.com/").unwrap();
        let q = "q=rust%20language&q=second&source=hp&sei=abc&igu=0&x=%2F%2B";
        let u = upstream_url(&t, &format!("/search?{q}").parse().unwrap()).unwrap();
        assert_eq!(u.query(), Some(q));
    }
    #[test]
    fn legacy_urls_are_canonicalized_without_open_redirects() {
        assert_eq!(clean_legacy_path("/proxy/search"), "/search");
        assert_eq!(clean_legacy_path("/proxy/sorry/index"), "/sorry/index");
        assert_eq!(clean_legacy_path("/proxy"), "/");
        assert_eq!(clean_legacy_path("/proxyfoo"), "/proxyfoo");
        assert_eq!(clean_legacy_path("/proxy//evil.example"), "/evil.example");
    }
    #[test]
    fn redirects_are_root_relative_without_proxy_prefix() {
        let t = parse_target("https://example.com/folder/page").unwrap();
        for (input, expected) in [
            ("/login", "/login"),
            ("next?q=1#f", "/folder/next?q=1#f"),
            ("//example.com/a", "/a"),
            ("https://example.com/search?q=ls", "/search?q=ls"),
        ] {
            assert_eq!(rewrite_location(input, &t).as_deref(), Some(expected));
        }
        assert!(rewrite_location("https://other.example/a", &t).is_none());
        assert!(resolve_redirect(&t, "javascript:alert(1)").is_none());
    }
    #[test]
    fn security_cookie_attributes_preserved() {
        assert_eq!(
            rewrite_cookie("NID=abc; Domain=google.com; Secure; HttpOnly; SameSite=None; Path=/")
                .unwrap(),
            "NID=abc; Secure; HttpOnly; SameSite=None; Path=/"
        );
        assert_eq!(
            rewrite_cookie("sid=abc; Domain=example.com; Path=/app; HttpOnly").unwrap(),
            "sid=abc; Path=/app; HttpOnly"
        );
        assert!(rewrite_cookie("__Host-webterm_session=secret; Path=/; Secure").is_none());
    }
    #[test]
    fn control_cookies_are_not_forwarded() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("webterm_session=private; colab_de_token=private; app=ok"),
        );
        rewrite_request_headers(
            &mut h,
            &parse_target("https://example.com/").unwrap(),
            false,
        );
        assert_eq!(h[header::COOKIE], "app=ok");
        assert!(!h.contains_key("x-forwarded-prefix"));
    }
    #[test]
    fn subresource_redirect_does_not_retarget_navigation() {
        let r = Request::builder()
            .uri("/asset")
            .header("sec-fetch-dest", "script")
            .body(Body::empty())
            .unwrap();
        assert!(!is_navigation_request(&r, r.uri()));
        let r = Request::builder()
            .uri("/search?q=ls")
            .body(Body::empty())
            .unwrap();
        assert!(is_navigation_request(&r, r.uri()));
    }
    #[tokio::test]
    async fn router_serves_controller_and_cleans_old_paths() {
        use tower::ServiceExt;
        let app = router().unwrap();
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/proxy/search?q=a%2Fb")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(r.headers()[header::LOCATION], "/search?q=a%2Fb");
        let r = app
            .oneshot(Request::builder().uri("/mpxx").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }
}
