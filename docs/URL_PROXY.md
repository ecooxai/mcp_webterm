# Port 1080: reverse preview and original-site browser proxy

## Two different modes

The same port supports two protocols/usages. Opening `http://SERVER:1080/` is a
**reverse preview** of the selected target. Configuring your browser to use
`SERVER:1080` as an **HTTP proxy** enables original-site mode. It is not a SOCKS
proxy. For HTTPS, Chrome uses CONNECT, then negotiates TLS directly with the
original website through the tunnel. No replacement CA certificate is installed.

Original-site mode is the appropriate mode for Google Search, domain-bound
verification, native cookies, login, and the website's normal browser security
policies. Browse to `https://www.google.com/`, not the reverse-preview address.
Simply opening `/mpxx` cannot change your browser or device proxy settings.

## Routes

- `/` opens the currently selected reverse target (default `https://www.google.com/`).
- `/search?q=ls` maps to the selected origin's `/search?q=ls` without a `/proxy` prefix.
- `/mpxx` is the target editor and iframe preview, with original-site setup help.
- `/pmurl` accepts GET and PUT/POST JSON `{"url":"https://www.google.com/"}`.
- `/mpxx/proxy.pac` supplies a proxy auto-configuration file without credentials.
- Old `/proxy/...` URLs redirect with HTTP 307 to their clean equivalents,
  preserving method, body, query encoding, and repeated query parameters.

The reverse target is shared, not per-browser. Same-origin redirects do not
change it. Cross-origin document redirects can change it; resource/XHR redirects
do not. Explicit forward-proxy requests use their own destination, independently
of this reverse target.

## Client setup

Proxy type: HTTP. Host: your server hostname. Port: 1080. Username: `proxy`.
The dedicated password is stored on the server at:

```
/etc/webterm/forward-proxy.token
```

Read it from an authenticated server terminal. It is not exposed by the public
controller, API, or PAC file. Do not put it in a URL or a public screenshot.

Desktop Chrome can use a separate profile:

```sh
google-chrome --user-data-dir="$HOME/.config/chrome-alima-proxy" \
  --proxy-server="http://alima.freeddns.org:1080" \
  https://www.google.com/
```

Enter the dedicated proxy username/password in **Chrome's proxy-login dialog**.
This is not a Google account sign-in. After that, website verification is handled
normally by the original site and the human using the browser.

For an encrypted connection to the proxy, use SSH forwarding:

```sh
ssh -N -L 11080:127.0.0.1:1080 admin@alima.freeddns.org
```

Then use `http://127.0.0.1:11080` as Chrome's proxy. Proxy authentication is still
required. HTTPS website traffic is end-to-end encrypted in either case, but
HTTP proxy credentials and plain-HTTP sites are not encrypted on a direct
connection to port 1080. A trusted VPN or SSH tunnel protects that outer hop.
On a phone, use the device/network's supported proxy configuration or an
appropriate tunnel. A normal web page cannot configure the cellular connection.

## Verification limits and corrected behavior

A Google reCAPTCHA site key belongs to Google-authorized domains. A page served
from the reverse-preview hostname is not `google.com`, even if its upstream Host
header says otherwise. The proxy does not forge origins, replace Google's keys,
solve CAPTCHA, or generate verification tokens. The reverse `/sorry/...` page
now explains the required original-site mode instead of presenting a broken
widget or blank page.

Secure, HttpOnly, SameSite and original cookie paths are preserved. The old
blanket removal of Secure has been removed; it weakened cookies and made
SameSite=None cookies invalid. Search parameters are preserved exactly: the
proxy no longer invents `igu=1` or strips session/verification parameters.

Google may still request human verification or reject traffic based on the
server's network. A successful demo-widget render does not prove completion of
a future Google Search challenge. Do not claim an HTTP 200 page alone proves
that a real search succeeded.

## Server protections

Set `WEBTERM_FORWARD_PROXY_TOKEN_FILE` to the private token file. With a token
configured, every explicit proxy connection requires authentication, including
loopback connections. Without a configured token, original-site forwarding is
limited to genuine loopback peers without forwarded-client headers; it is not
opened publicly. The deployed service is configured with a token.

Explicit forwarding permits public destinations on ports 80 and 443 only.
Loopback/private/metadata/reserved destinations are blocked. DNS results are
validated once and the connection uses those exact addresses. CONNECT tunnels
are bounded to 64 concurrent sessions and one hour per tunnel. The reverse
preview remains intended for trusted use; do not treat the public target editor
as a multi-user authenticated browsing service.

## Build and tests

Compiler output belongs in `/build/webterm/target`; the release artifact is
`dist/webterm`.

```sh
CARGO_TARGET_DIR=/build/webterm/target cargo test --lib -j 2
CARGO_TARGET_DIR=/build/webterm/target cargo build --release -j 2
```

`examples/url_proxy_test_server.rs` provides an isolated loopback test server.
`tests/url_proxy_integration.py` covers paths, redirects, cookie flags, POST,
WebSocket echo, proxy authentication and network destination restrictions.
`tests/url_proxy_browser_routing.cjs` checks legacy URL recovery in mobile-sized
Chrome. `tests/url_proxy_chrome_native.cjs` attaches to a normal headed Chrome
session after its native proxy login and checks actual Google results and widget
rendering. CAPTCHA interactions are never automated. CDP/Xvfb are test-only and
must remain private; no debugger endpoint is exposed in production.

Reports/screenshots from this deployment are in `dist/proxy-tests/`.

## Primary references

- https://developers.google.com/recaptcha/docs/domain_validation
- https://chromium.googlesource.com/chromium/src/+/HEAD/net/docs/proxy.md
- https://support.google.com/websearch/answer/86640
- https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Set-Cookie
