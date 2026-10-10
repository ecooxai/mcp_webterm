(() => {
  "use strict";
  const form = document.getElementById("login-form");
  const input = document.getElementById("password");
  const toggle = document.getElementById("password-toggle");
  const button = document.getElementById("login-submit");
  const label = document.getElementById("submit-label");
  const spinner = button.querySelector(".spinner");
  const error = document.getElementById("login-error");
  const status = document.getElementById("login-status");
  let pending = false;
  const logPath = location.pathname.replace(/\/$/, "");
  const destination = logPath === "/log" || logPath === "/webterm/log" ? logPath : location.pathname.startsWith("/webterm") ? "/webterm/" : "/";
  toggle.addEventListener("click", () => {
    const showing = input.type === "text";
    input.type = showing ? "password" : "text";
    toggle.textContent = showing ? "Show" : "Hide";
    toggle.setAttribute("aria-label", showing ? "Show password" : "Hide password");
    toggle.setAttribute("aria-pressed", String(!showing));
    input.focus({preventScroll:true});
  });
  async function signIn(value) {
    if (pending) return;
    error.textContent = "";
    if (!value) { error.textContent = "Enter your password."; input.focus(); return; }
    pending = true; button.disabled = true; spinner.hidden = false;
    button.setAttribute("aria-busy", "true"); label.textContent = "Signing in…";
    status.textContent = "Connecting to your workspace…";
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 12000);
    input.value = "";
    let signedIn = false;
    try {
      await window.WebTermAuth.login(value);
      value = "";
      signedIn = true;
      // Password accepted: never show the form again while the terminal loads.
      for (const node of form.querySelectorAll("label, .password-row, #login-submit, #login-error")) node.style.display = "none";
      status.textContent = "Signed in. Opening your terminals…";
      location.replace(destination + "?app=1&proxyport=0");
    } catch (failure) {
      error.textContent = failure.name === "AbortError" ? "The workspace took too long to respond. Try again when it is ready." : failure.message;
      status.textContent = "Your terminal sessions have not been changed.";
      input.focus({preventScroll:true});
    } finally {
      value = ""; clearTimeout(timeout);
      if (signedIn) return;
      pending = false; button.disabled = false;
      button.removeAttribute("aria-busy"); spinner.hidden = true; label.textContent = "Sign in";
    }
  }
  form.addEventListener("submit", event => {event.preventDefault(); signIn(input.value);});
  performance.mark("webterm-login-ready");
  const query = new URL(location.href);
  if (query.searchParams.has("passwd")) {
    query.searchParams.delete("passwd");
    history.replaceState(null, "", query.pathname + query.search + query.hash);
    error.textContent = "Enter the password here; credentials are no longer accepted in page URLs.";
  } else if (window.WebTermAuth.saved()) {
    location.replace(destination + "?app=1&proxyport=0");
  }
})();
