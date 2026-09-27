//! The authenticated gateway sets a port header; untrusted headers cannot bypass MCP auth.
#[derive(Clone, Debug)]
pub struct HostPreview;
pub const PORT_HEADER: &str = "x-webterm-preview-port";
pub fn valid_port(port: u16, self_port: u16) -> bool {
    port >= 1024 && ![self_port, 18763, 18764, 9992, 7681].contains(&port)
}
pub fn cookie(value: &str) -> Option<String> {
    let mut pieces = value.split(';');
    let first = pieces.next()?;
    let (name, _) = first.split_once('=')?;
    if reserved_cookie(name) {
        return None;
    }
    // Remove Domain, preserve the application's own Path, lifetime and other flags.
    let mut kept = vec![first.trim().to_owned()];
    let mut path = false;
    for part in pieces {
        let key = part.trim().split('=').next()?.trim();
        if key.eq_ignore_ascii_case("domain") {
            continue;
        }
        if key.eq_ignore_ascii_case("path") {
            path = true;
        }
        kept.push(part.trim().to_owned());
    }
    if !path {
        kept.push("Path=/".into());
    }
    Some(kept.join("; "))
}
pub fn reserved_cookie(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    n == "__host-colab_preview"
        || n.starts_with("webterm")
        || n.starts_with("__host-webterm")
        || n.starts_with("colab_de")
}
pub fn redirect(value: &str, port: u16) -> String {
    let absolute = if value.starts_with("//") {
        format!("http:{value}")
    } else {
        value.to_owned()
    };
    let (base, fragment) = absolute
        .split_once('#')
        .map(|(b, f)| (b, format!("#{f}")))
        .unwrap_or((&absolute, String::new()));
    if let Ok(u) = base.parse::<axum::http::Uri>() {
        if [Some("http"), Some("https")].contains(&u.scheme_str())
            && u.host()
                .is_some_and(|h| ["localhost", "127.0.0.1", "[::1]"].contains(&h))
            && u.port_u16().unwrap_or(80) == port
        {
            return format!(
                "{}{}",
                u.path_and_query().map(|p| p.as_str()).unwrap_or("/"),
                fragment
            );
        }
    }
    value.to_owned()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_and_cookies() {
        assert_eq!(
            redirect("http://localhost:3000/login?a=%2F#x", 3000),
            "/login?a=%2F#x"
        );
        assert_eq!(redirect("/a?proxyport=99", 3000), "/a?proxyport=99");
        assert_eq!(
            cookie("app=x; Path=/app; Domain=alima.freeddns.org; HttpOnly").unwrap(),
            "app=x; Path=/app; HttpOnly"
        );
        assert!(cookie("__Host-colab_preview=forged; Path=/").is_none());
        assert!(cookie("__Host-app=works; Path=/; Secure").is_some());
    }
}
