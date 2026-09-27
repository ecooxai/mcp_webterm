//! Authenticated path proxy to a fixed loopback host. Authentication is in web.rs.
//! Public HTTP/2 and HTTP/3 terminate at the HTTPS edge; this hop streams HTTP/1.1.
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri, Version, header},
    response::{IntoResponse, Response},
};
use hyper_util::rt::TokioIo;
use std::time::Duration;

fn error(status: StatusCode, text: &'static str) -> Response {
    (status, text).into_response()
}

fn hop_headers(headers: &mut HeaderMap, websocket: bool) {
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

fn private_cookie(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    name.starts_with("__host-") || name.starts_with("webterm") || name.starts_with("colab_de")
}

fn filtered_cookies(headers: &HeaderMap) -> String {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter(|v| {
            v.split_once('=')
                .is_some_and(|(name, _)| !private_cookie(name))
        })
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("; ")
}

fn scoped_cookie(value: &str, prefix: &str) -> Option<String> {
    let mut parts = value.split(';');
    let first = parts.next()?;
    let (name, _) = first.split_once('=')?;
    if private_cookie(name) {
        return None;
    }
    let mut kept = vec![first.trim().to_owned()];
    if prefix.is_empty() {
        let mut has_path = false;
        for part in parts {
            let key = part.trim().split('=').next()?.trim();
            if key.eq_ignore_ascii_case("domain") {
                continue;
            }
            if key.eq_ignore_ascii_case("path") {
                has_path = true;
            }
            kept.push(part.trim().to_owned());
        }
        if !has_path {
            kept.push("Path=/".into());
        }
        return Some(kept.join("; "));
    }
    for part in parts {
        let key = part.trim().split('=').next()?.trim();
        if !key.eq_ignore_ascii_case("path") && !key.eq_ignore_ascii_case("domain") {
            kept.push(part.trim().to_owned());
        }
    }
    kept.push(format!("Path={}/", prefix.trim_end_matches('/')));
    Some(kept.join("; "))
}

fn redirect(value: &str, prefix: &str, port: u16) -> String {
    if value.starts_with('/') && !value.starts_with("//") {
        return format!("{prefix}{value}");
    }
    if let Ok(uri) = value.parse::<Uri>() {
        if uri
            .scheme_str()
            .is_some_and(|s| s == "http" || s == "https")
            && uri
                .host()
                .is_some_and(|h| ["127.0.0.1", "localhost", "[::1]"].contains(&h))
            && uri.port_u16().unwrap_or(80) == port
        {
            return format!(
                "{prefix}{}",
                uri.path_and_query().map(|p| p.as_str()).unwrap_or("/")
            );
        }
    }
    value.to_owned()
}

pub async fn forward(mut request: Request<Body>, self_port: u16) -> Response {
    let host_preview=request.extensions().get::<crate::subdomain_proxy::HostPreview>().is_some();
    let app_authorization=if host_preview{request.headers().get("x-webterm-app-authorization").cloned()}else{None};
    if request.method() == Method::CONNECT {
        return error(
            StatusCode::METHOD_NOT_ALLOWED,
            "CONNECT is not supported; use HTTP or WebSocket",
        );
    }
    let target = request
        .extensions_mut()
        .remove::<crate::query_proxy::Target>();
    let query_mode = target.is_some();
    let (port, prefix, upstream_path) = if let Some(target) = target {
        (target.port, String::new(), target.path)
    } else {
        let raw = request.uri().path();
        let rest = match raw.strip_prefix("/proxy/") {
            Some(s) => s,
            None => return error(StatusCode::NOT_FOUND, "Invalid proxy path"),
        };
        let (port_text, tail) = rest
            .split_once('/')
            .map(|(a, b)| (a, Some(b)))
            .unwrap_or((rest, None));
        if port_text.is_empty() || !port_text.bytes().all(|b| b.is_ascii_digit()) {
            return error(StatusCode::BAD_REQUEST, "Invalid proxy port");
        }
        let port: u16 = match port_text.parse() {
            Ok(p) if p > 0 => p,
            _ => return error(StatusCode::BAD_REQUEST, "Invalid proxy port"),
        };
        let prefix = format!("/proxy/{port}");
        let query = request
            .uri()
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default();
        let Some(tail) = tail else {
            let mut r = StatusCode::PERMANENT_REDIRECT.into_response();
            r.headers_mut().insert(
                header::LOCATION,
                HeaderValue::from_str(&format!("{prefix}/{query}")).unwrap(),
            );
            return r;
        };
        (port, prefix, format!("/{tail}{query}"))
    };
    if port == 0 || port == self_port {
        return error(
            StatusCode::FORBIDDEN,
            "The WebTerm control port cannot be proxied",
        );
    }
    let uri: Uri = match upstream_path.parse() {
        Ok(u) => u,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid proxy path"),
    };
    let websocket = request
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.eq_ignore_ascii_case("websocket"));
    if request.headers().contains_key(header::UPGRADE) && !websocket {
        return error(
            StatusCode::BAD_REQUEST,
            "Only WebSocket upgrades are supported",
        );
    }
    let downstream_upgrade = websocket.then(|| hyper::upgrade::on(&mut request));
    let host = request.headers().get(header::HOST).cloned();
    let public_host = host
        .as_ref()
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned);
    let proto = request
        .headers()
        .get("x-forwarded-proto")
        .cloned()
        .unwrap_or(HeaderValue::from_static("http"));
    let cookies = if host_preview {
        request.headers().get_all(header::COOKIE).iter().filter_map(|v|v.to_str().ok()).flat_map(|v|v.split(';')).filter(|v|v.split_once('=').is_some_and(|(n,_)|!crate::subdomain_proxy::reserved_cookie(n))).map(str::trim).collect::<Vec<_>>().join("; ")
    } else { filtered_cookies(request.headers()) };
    let origin = request.headers().contains_key(header::ORIGIN);
    let headers = request.headers_mut();
    hop_headers(headers, websocket);
    for name in [
        "authorization",
        "proxy-authorization",
        "cookie",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "x-forwarded-prefix",
        "x-webterm-preview-port",
        "x-webterm-app-authorization",
        "x-webterm-control",
        "x-webterm-control",
    ] {
        headers.remove(name);
    }
    headers.insert(
        header::HOST,
        HeaderValue::from_str(&format!("127.0.0.1:{port}")).unwrap(),
    );
    if let Some(host) = host {
        headers.insert("x-forwarded-host", host);
    }
    headers.insert("x-forwarded-proto", proto);
    headers.insert("x-forwarded-prefix",HeaderValue::from_str(&prefix).unwrap());
    if host_preview {headers.remove("x-forwarded-prefix");if let Some(auth)=app_authorization {headers.insert(header::AUTHORIZATION,auth);}}
    if !cookies.is_empty() {
        if let Ok(cookie) = HeaderValue::from_str(&cookies) {
            headers.insert(header::COOKIE, cookie);
        }
    }
    if origin {
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_str(&format!("http://127.0.0.1:{port}")).unwrap(),
        );
    }
    *request.uri_mut() = uri;
    *request.version_mut() = Version::HTTP_11;
    let stream = match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        _ => {
            return error(
                StatusCode::BAD_GATEWAY,
                "Nothing is reachable on this runtime's loopback port",
            );
        }
    };
    let (mut sender, connection) =
        match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await {
            Ok(pair) => pair,
            Err(_) => return error(StatusCode::BAD_GATEWAY, "Upstream HTTP handshake failed"),
        };
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let mut response =
        match tokio::time::timeout(Duration::from_secs(60), sender.send_request(request)).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => return error(StatusCode::BAD_GATEWAY, "Upstream HTTP request failed"),
            Err(_) => {
                return error(
                    StatusCode::GATEWAY_TIMEOUT,
                    "Upstream did not return response headers within 60 seconds",
                );
            }
        };
    let upgraded = response.status() == StatusCode::SWITCHING_PROTOCOLS;
    if upgraded && !websocket {
        return error(
            StatusCode::BAD_GATEWAY,
            "Unexpected upstream protocol upgrade",
        );
    }
    if upgraded {
        let upstream_upgrade = hyper::upgrade::on(&mut response);
        let downstream_upgrade = downstream_upgrade.unwrap();
        tokio::spawn(async move {
            let pair = tokio::time::timeout(Duration::from_secs(10), async {
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
    let headers = response.headers_mut();
    hop_headers(headers, upgraded);
    let location = headers
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            if host_preview { crate::subdomain_proxy::redirect(v,port) } else if query_mode {
                crate::query_proxy::redirect(v, port, public_host.as_deref())
            } else {
                redirect(v, &prefix, port)
            }
        });
    if let Some(location) = location {
        if let Ok(value) = HeaderValue::from_str(&location) {
            headers.insert(header::LOCATION, value);
        }
    }
    let cookies: Vec<String> = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| if host_preview {crate::subdomain_proxy::cookie(v)}else{scoped_cookie(v, &prefix)})
        .collect();
    headers.remove(header::SET_COOKIE);
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            headers.append(header::SET_COOKIE, value);
        }
    }
    if query_mode && !host_preview {
        headers.insert("referrer-policy", HeaderValue::from_static("same-origin"));
        headers.insert("cache-control", HeaderValue::from_static("no-store"));
        headers.insert(
            "x-webterm-proxy-port",
            HeaderValue::from_str(&port.to_string()).unwrap(),
        );
    }
    headers.insert("x-webterm-proxy", HeaderValue::from_static("loopback"));
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, Body::new(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auth_cookies_never_reach_apps() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static(
                "__Host-webterm_session=secret; app=ok; colab_de_log_session=private",
            ),
        );
        assert_eq!(filtered_cookies(&h), "app=ok");
        assert!(scoped_cookie("__Host-webterm_session=bad; Path=/", "/proxy/3000").is_none());
        assert_eq!(
            scoped_cookie(
                "app=ok; Domain=example.com; Path=/; HttpOnly",
                "/proxy/3000"
            )
            .unwrap(),
            "app=ok; HttpOnly; Path=/proxy/3000/"
        );
    }
    #[test]
    fn redirects_keep_prefix_queries_and_encoding() {
        assert_eq!(
            redirect("/app?x=%2F&x=2", "/proxy/3000", 3000),
            "/proxy/3000/app?x=%2F&x=2"
        );
        assert_eq!(
            redirect("http://localhost:3000/test?q=hi", "/proxy/3000", 3000),
            "/proxy/3000/test?q=hi"
        );
        assert_eq!(
            redirect("https://example.com/", "/proxy/3000", 3000),
            "https://example.com/"
        );
    }
    #[test]
    fn named_hop_headers_are_stripped() {
        let mut h = HeaderMap::new();
        h.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, x-internal"),
        );
        h.insert("x-internal", HeaderValue::from_static("secret"));
        hop_headers(&mut h, false);
        assert!(h.get("x-internal").is_none());
        assert!(h.get(header::CONNECTION).is_none());
    }
}
