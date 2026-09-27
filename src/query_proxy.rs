//! Query-selected previews. Selection never authenticates a request.
use axum::http::{HeaderMap, Uri, header};

pub const COOKIE: &str = "webterm_proxyport";
#[derive(Clone, Debug)]
pub struct Target {
    pub port: u16,
    pub path: String,
}

fn decode(s: &str) -> Result<String, &'static str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = (b[i + 1] as char)
                    .to_digit(16)
                    .ok_or("Invalid query escape")?;
                let l = (b[i + 2] as char)
                    .to_digit(16)
                    .ok_or("Invalid query escape")?;
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            b'%' => return Err("Invalid query escape"),
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            x => {
                out.push(x);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "Invalid query encoding")
}

/// Remove only the routing parameter. All other bytes, repeats and escapes survive.
pub fn split_query(query: Option<&str>) -> Result<(Option<u16>, String), &'static str> {
    let mut selected = None;
    let mut kept = Vec::new();
    if let Some(q) = query {
        for pair in q.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            // Invalid unrelated escapes are opaque application data, not ours.
            if decode(key).ok().as_deref() == Some("proxyport") {
                if selected.is_some() {
                    return Err("Duplicate proxyport parameters are not allowed");
                }
                let value = decode(value)?;
                if value.is_empty() || value.len() > 5 || !value.bytes().all(|v| v.is_ascii_digit())
                {
                    return Err("proxyport must be 1..65535, or 0 to return to WebTerm");
                }
                selected = Some(
                    value
                        .parse::<u16>()
                        .map_err(|_| "proxyport is out of range")?,
                );
            } else {
                kept.push(pair);
            }
        }
    }
    Ok((selected, kept.join("&")))
}

pub fn selected(uri: &Uri, headers: &HeaderMap) -> Result<Option<u16>, &'static str> {
    let (explicit, _) = split_query(uri.query())?;
    if explicit.is_some() {
        return Ok(explicit);
    }
    // Control clients bypass only inherited selection, never an explicit parameter.
    if headers.get("x-webterm-control").is_some_and(|v| v == "1") {
        return Ok(None);
    }
    if let Some(reference) = headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<Uri>().ok())
    {
        let host = headers
            .get("x-forwarded-host")
            .or_else(|| headers.get(header::HOST))
            .and_then(|v| v.to_str().ok());
        let proto = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("http");
        if reference.authority().map(|a| a.as_str()) == host
            && reference.scheme_str() == Some(proto)
        {
            if let Ok((Some(p), _)) = split_query(reference.query()) {
                return Ok(Some(p));
            }
        }
    }
    for cookie in headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
    {
        if let Some((name, value)) = cookie.trim().split_once('=') {
            if name == COOKIE {
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Ok(None);
                }
                return Ok(value.parse::<u16>().ok().filter(|p| *p > 0));
            }
        }
    }
    Ok(None)
}

pub fn upstream(uri: &Uri) -> Result<String, &'static str> {
    let (_, q) = split_query(uri.query())?;
    Ok(format!(
        "{}{}",
        uri.path(),
        if q.is_empty() {
            String::new()
        } else {
            format!("?{q}")
        }
    ))
}

pub fn cookie(port: u16, secure: bool) -> String {
    format!(
        "{COOKIE}={}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict{}",
        if port == 0 {
            String::new()
        } else {
            port.to_string()
        },
        if port == 0 { 0 } else { 7200 },
        if secure { "; Secure" } else { "" }
    )
}

pub fn redirect(value: &str, port: u16, public_host: Option<&str>) -> String {
    let (base, fragment) = value
        .split_once('#')
        .map(|(b, f)| (b, format!("#{f}")))
        .unwrap_or((value, String::new()));
    if base.is_empty() {
        return value.to_string();
    }
    let mut path = base.to_string();
    if base.starts_with("//") {
        let absolute = format!("http:{base}");
        if let Ok(u) = absolute.parse::<Uri>() {
            if u.authority().map(|a| a.as_str()) != public_host {
                return value.to_string();
            }
            path = u
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/")
                .to_string();
        } else {
            return value.to_string();
        }
    } else if let Ok(u) = base.parse::<Uri>() {
        if u.scheme().is_some() {
            let same = u.authority().map(|a| a.as_str()) == public_host;
            let local = u
                .host()
                .is_some_and(|h| ["localhost", "127.0.0.1", "[::1]"].contains(&h))
                && u.port_u16().unwrap_or(80) == port;
            if ![Some("http"), Some("https")].contains(&u.scheme_str()) || (!same && !local) {
                return value.to_string();
            }
            path = u
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/")
                .to_string();
        }
    }
    let (p, q) = path.split_once('?').unwrap_or((&path, ""));
    // Never let an application's redirect change the selected control port.
    let kept = q
        .split('&')
        .filter(|pair| {
            decode(pair.split('=').next().unwrap_or("")).ok().as_deref() != Some("proxyport")
        })
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("&");
    format!(
        "{p}?{}proxyport={port}{fragment}",
        if kept.is_empty() {
            String::new()
        } else {
            format!("{kept}&")
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    #[test]
    fn query_is_preserved_except_selector() {
        let u: Uri = "/app/a%2Fb?x=%2f&proxy%70ort=3000&x=a+b&empty=&bare"
            .parse()
            .unwrap();
        assert_eq!(split_query(u.query()).unwrap().0, Some(3000));
        assert_eq!(upstream(&u).unwrap(), "/app/a%2Fb?x=%2f&x=a+b&empty=&bare");
    }
    #[test]
    fn strict_ports() {
        for q in [
            "proxyport=",
            "proxyport=65536",
            "proxyport=-1",
            "proxyport=2.0",
            "proxyport=30%003",
            "proxyport=3000&proxyport=4000",
        ] {
            assert!(split_query(Some(q)).is_err(), "{q}");
        }
        assert_eq!(split_query(Some("proxyport=0")).unwrap().0, Some(0));
    }
    #[test]
    fn explicit_then_control_then_reference_then_cookie() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("webterm_proxyport=3000"),
        );
        h.insert(header::HOST, HeaderValue::from_static("example.test"));
        h.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert_eq!(
            selected(&"/resource".parse().unwrap(), &h).unwrap(),
            Some(3000)
        );
        h.insert(
            header::REFERER,
            HeaderValue::from_static("https://example.test/app?proxyport=4000"),
        );
        assert_eq!(
            selected(&"/resource".parse().unwrap(), &h).unwrap(),
            Some(4000)
        );
        h.insert("x-webterm-control", HeaderValue::from_static("1"));
        assert_eq!(
            selected(&"/api/v1/session".parse().unwrap(), &h).unwrap(),
            None
        );
        assert_eq!(
            selected(&"/api/v1/session?proxyport=5000".parse().unwrap(), &h).unwrap(),
            Some(5000)
        );
    }
    #[test]
    fn redirect_preserves_query_fragment() {
        assert_eq!(
            redirect("/login?next=%2Fapp&x=1&x=2#f", 3000, None),
            "/login?next=%2Fapp&x=1&x=2&proxyport=3000#f"
        );
        assert_eq!(
            redirect("http://localhost:3000/a", 3000, None),
            "/a?proxyport=3000"
        );
        assert_eq!(redirect("next?q=1", 3000, None), "next?q=1&proxyport=3000");
        assert_eq!(
            redirect("https://other.test/", 3000, None),
            "https://other.test/"
        );
    }
}
