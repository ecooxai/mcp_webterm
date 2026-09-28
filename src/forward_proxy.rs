//! Explicit browser proxy. HTTPS is an opaque CONNECT tunnel: no MITM, URL,
//! cookie, Origin, CAPTCHA, or TLS-certificate rewriting occurs in this mode.
use anyhow::{Context, Result, bail};
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri, Version, header},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hyper_util::rt::TokioIo;
use std::{
    env,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpStream,
    sync::{OwnedSemaphorePermit, Semaphore},
    time::timeout,
};
use url::Url;

pub struct ForwardProxy {
    password: Option<Vec<u8>>,
    slots: Arc<Semaphore>,
}

impl ForwardProxy {
    pub fn from_env() -> Result<Self> {
        let password = match env::var("WEBTERM_FORWARD_PROXY_TOKEN_FILE") {
            Ok(path) => {
                Some(std::fs::read_to_string(&path).context("read forward-proxy token file")?)
            }
            Err(_) => env::var("WEBTERM_FORWARD_PROXY_TOKEN").ok(),
        }
        .map(|s| s.trim().as_bytes().to_vec());
        if password
            .as_ref()
            .is_some_and(|p| p.len() < 24 || p.iter().any(u8::is_ascii_whitespace))
        {
            bail!("forward-proxy token must be at least 24 characters without whitespace");
        }
        Ok(Self {
            password,
            slots: Arc::new(Semaphore::new(64)),
        })
    }

    pub fn handles(req: &Request<Body>) -> bool {
        req.method() == Method::CONNECT || req.uri().scheme().is_some()
    }

    fn authorized(&self, req: &Request<Body>) -> bool {
        if let Some(password) = &self.password {
            return valid_credentials(req.headers(), password);
        }
        // Without a token, only a genuinely local connection is allowed. Never
        // trust a caller-supplied X-Forwarded-For as a loopback identity.
        req.extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|peer| peer.0.ip().is_loopback())
            && !req.headers().contains_key("x-forwarded-for")
            && !req.headers().contains_key("forwarded")
    }

    pub async fn serve(&self, mut req: Request<Body>) -> Response {
        if !self.authorized(&req) {
            return auth_required();
        }
        let permit = match self.slots.clone().try_acquire_owned() {
            Ok(p) => Arc::new(p),
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Proxy connection limit reached",
                )
                    .into_response();
            }
        };
        if req.method() == Method::CONNECT {
            let Some(authority) = req.uri().authority() else {
                return (StatusCode::BAD_REQUEST, "CONNECT needs host:port").into_response();
            };
            let Some(port) = authority.port_u16() else {
                return (StatusCode::BAD_REQUEST, "CONNECT needs an explicit port").into_response();
            };
            let upstream = match public_connection(authority.host(), port).await {
                Ok(s) => s,
                Err(r) => return r,
            };
            let upgrade = hyper::upgrade::on(&mut req);
            tokio::spawn(async move {
                let _permit = permit;
                if let Ok(Ok(client)) = timeout(Duration::from_secs(15), upgrade).await {
                    let mut client = TokioIo::new(client);
                    let mut upstream = upstream;
                    let _ = timeout(
                        Duration::from_secs(3600),
                        tokio::io::copy_bidirectional(&mut client, &mut upstream),
                    )
                    .await;
                }
            });
            // Hyper recognizes successful CONNECT and switches to an opaque tunnel.
            return Response::new(Body::empty());
        }

        let url = match Url::parse(&req.uri().to_string()) {
            Ok(u) if u.scheme() == "http" && u.username().is_empty() && u.password().is_none() => u,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    "Use absolute http:// URLs or CONNECT for HTTPS",
                )
                    .into_response();
            }
        };
        let upstream = match public_connection(
            url.host_str().unwrap_or(""),
            url.port_or_known_default().unwrap_or(80),
        )
        .await
        {
            Ok(s) => s,
            Err(r) => return r,
        };
        let websocket = is_websocket(req.headers());
        if req.headers().contains_key(header::UPGRADE) && !websocket {
            return (StatusCode::BAD_REQUEST, "Unsupported protocol upgrade").into_response();
        }
        let downstream_upgrade = websocket.then(|| hyper::upgrade::on(&mut req));
        let Some(authority) = req.uri().authority().map(|a| a.as_str().to_owned()) else {
            return (
                StatusCode::BAD_REQUEST,
                "An absolute HTTP URI with an authority is required",
            )
                .into_response();
        };
        let path: Uri = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .parse()
            .unwrap();
        *req.uri_mut() = path;
        *req.version_mut() = Version::HTTP_11;
        strip_hop_headers(req.headers_mut(), websocket);
        req.headers_mut().remove(header::PROXY_AUTHORIZATION);
        req.headers_mut()
            .insert(header::HOST, HeaderValue::from_str(&authority).unwrap());
        let (mut sender, connection) = match timeout(
            Duration::from_secs(10),
            hyper::client::conn::http1::handshake(TokioIo::new(upstream)),
        )
        .await
        {
            Ok(Ok(pair)) => pair,
            _ => {
                return (StatusCode::BAD_GATEWAY, "Upstream HTTP handshake failed").into_response();
            }
        };
        let conn_permit = permit.clone();
        tokio::spawn(async move {
            let _permit = conn_permit;
            let _ = connection.with_upgrades().await;
        });
        let mut response = match timeout(Duration::from_secs(60), sender.send_request(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => {
                return (StatusCode::BAD_GATEWAY, "Upstream HTTP request failed").into_response();
            }
            Err(_) => {
                return (StatusCode::GATEWAY_TIMEOUT, "Upstream response timed out")
                    .into_response();
            }
        };
        let upgraded = response.status() == StatusCode::SWITCHING_PROTOCOLS;
        if upgraded {
            let Some(downstream) = downstream_upgrade else {
                return (StatusCode::BAD_GATEWAY, "Unexpected protocol upgrade").into_response();
            };
            bridge_upgrades(downstream, hyper::upgrade::on(&mut response), permit);
        }
        strip_hop_headers(response.headers_mut(), upgraded);
        // Cookies, redirects, CSP and security headers remain native to the site.
        let (parts, body) = response.into_parts();
        Response::from_parts(parts, Body::new(body))
    }
}

fn valid_credentials(headers: &HeaderMap, password: &[u8]) -> bool {
    let Some(raw) = headers
        .get(header::PROXY_AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some((scheme, encoded)) = raw.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("Basic") {
        return false;
    }
    let Ok(decoded) = STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let Some(colon) = decoded.iter().position(|b| *b == b':') else {
        return false;
    };
    &decoded[..colon] == b"proxy" && decoded[colon + 1..].ct_eq(password).into()
}
fn auth_required() -> Response {
    let mut response = (
        StatusCode::PROXY_AUTHENTICATION_REQUIRED,
        "Original-site proxy requires authentication. Username: proxy. See /mpxx for setup.",
    )
        .into_response();
    response.headers_mut().insert(
        header::PROXY_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"WebTerm original-site proxy\", charset=\"UTF-8\""),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn public_connection(host: &str, port: u16) -> std::result::Result<TcpStream, Response> {
    if !matches!(port, 80 | 443) || host.is_empty() {
        return Err((
            StatusCode::FORBIDDEN,
            "Forward proxy permits public web destinations on ports 80 and 443 only",
        )
            .into_response());
    }
    let host = host.trim_matches(['[', ']']);
    let addresses: Vec<_> = match timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, port)),
    )
    .await
    {
        Ok(Ok(addrs)) => addrs.take(64).collect(),
        _ => return Err((StatusCode::BAD_GATEWAY, "Destination DNS lookup failed").into_response()),
    };
    if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
        return Err((
            StatusCode::FORBIDDEN,
            "Private, loopback, metadata and reserved destinations are not allowed",
        )
            .into_response());
    }
    // Connect only to the addresses validated above; do not resolve a second time.
    match timeout(
        Duration::from_secs(10),
        TcpStream::connect(addresses.as_slice()),
    )
    .await
    {
        Ok(Ok(s)) => {
            let _ = s.set_nodelay(true);
            Ok(s)
        }
        _ => Err((StatusCode::BAD_GATEWAY, "Destination connection failed").into_response()),
    }
}
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !v.is_private()
                && !v.is_loopback()
                && !v.is_link_local()
                && !v.is_multicast()
                && !v.is_broadcast()
                && !v.is_documentation()
                && !v.is_unspecified()
                && o[0] != 0
                && o[0] < 240
                && !(o[0] == 100 && (64..=127).contains(&o[1]))
                && !(o[0] == 198 && (18..=19).contains(&o[1]))
                && !(o[0] == 192 && (o[1] == 0 || o[1] == 88))
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            let s = v.segments();
            !v.is_loopback()
                && !v.is_unspecified()
                && !v.is_multicast()
                && !v.is_unique_local()
                && !v.is_unicast_link_local()
                && (s[0] & 0xe000) == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] < 0x0200))
        }
    }
}
fn is_websocket(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|s| s.eq_ignore_ascii_case("websocket"))
}
fn strip_hop_headers(headers: &mut HeaderMap, websocket: bool) {
    let names: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_ascii_lowercase())
        .collect();
    for name in names {
        if !(websocket && name == "upgrade") {
            headers.remove(name);
        }
    }
    for name in [
        "connection",
        "proxy-authorization",
        "proxy-authenticate",
        "proxy-connection",
        "keep-alive",
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
fn bridge_upgrades(
    down: hyper::upgrade::OnUpgrade,
    up: hyper::upgrade::OnUpgrade,
    permit: Arc<OwnedSemaphorePermit>,
) {
    tokio::spawn(async move {
        let _permit = permit;
        if let Ok(Ok((down, up))) = timeout(Duration::from_secs(15), async {
            Ok::<_, hyper::Error>((down.await?, up.await?))
        })
        .await
        {
            let mut down = TokioIo::new(down);
            let mut up = TokioIo::new(up);
            let _ = timeout(
                Duration::from_secs(3600),
                tokio::io::copy_bidirectional(&mut down, &mut up),
            )
            .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_and_metadata_addresses_blocked() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.0.1",
            "169.254.169.254",
            "100.100.100.200",
            "0.0.0.0",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "::1",
            "::",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "2001:db8::1",
            "2002:7f00:1::",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
    }
    #[test]
    fn authentication_and_request_detection() {
        let mut h = HeaderMap::new();
        h.insert(
            header::PROXY_AUTHORIZATION,
            HeaderValue::from_str(&format!(
                "Basic {}",
                STANDARD.encode("proxy:example-secret")
            ))
            .unwrap(),
        );
        assert!(valid_credentials(&h, b"example-secret"));
        assert!(!valid_credentials(&h, b"wrong-secret"));
        assert!(!valid_credentials(&HeaderMap::new(), b"example-secret"));
        assert!(ForwardProxy::handles(
            &Request::builder()
                .method("CONNECT")
                .uri("www.google.com:443")
                .body(Body::empty())
                .unwrap()
        ));
        assert!(ForwardProxy::handles(
            &Request::builder()
                .uri("http://example.com/mpxx")
                .body(Body::empty())
                .unwrap()
        ));
        assert!(!ForwardProxy::handles(
            &Request::builder().uri("/mpxx").body(Body::empty()).unwrap()
        ));
    }
    #[tokio::test]
    async fn dangerous_ports_rejected() {
        assert!(public_connection("example.com", 22).await.is_err());
        assert!(public_connection("127.0.0.1", 443).await.is_err());
    }
}
