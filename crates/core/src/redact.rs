//! Redaction, applied in the normaliser before anything reaches disk.
//!
//! The window title is the one field that reliably leaks document content, and
//! a URL is the one field that reliably leaks credentials. Both are scrubbed
//! here, on the write path, so the sensitive form never exists in the database
//! for a later bug to expose.

use regex::Regex;
use std::sync::LazyLock;

/// Patterns that mean "this string contains a secret, drop the whole thing".
/// Deliberately biased toward false positives: losing a window title costs the
/// user a nicer session name, keeping a token costs them a great deal more.
static SECRETS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // JSON Web Tokens
        r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
        // Long unbroken hex — session ids, raw keys, hashes
        r"\b[0-9a-fA-F]{32,}\b",
        // AWS access key id
        r"\bAKIA[0-9A-Z]{16}\b",
        // Stripe and friends
        r"\b[sprk]k_(live|test)_[A-Za-z0-9]{16,}\b",
        // GitHub tokens
        r"\bgh[pousr]_[A-Za-z0-9]{20,}\b",
        // Slack tokens
        r"\bxox[baprse]-[A-Za-z0-9-]{10,}\b",
        // Google API keys
        r"\bAIza[0-9A-Za-z_\-]{35}\b",
        // PEM headers
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
        // key = value where the key names a secret
        r"(?i)\b(pass(word|wd)?|secret|token|api[_\-]?key|bearer|credential)\s*[:=]\s*\S{4,}",
    ]
    .iter()
    .filter_map(|p| Regex::new(p).ok())
    .collect()
});

/// Query parameters dropped from every URL, on every host, always.
static SENSITIVE_PARAMS: &[&str] = &[
    "token", "access_token", "id_token", "refresh_token", "oauth_token",
    "code", "api_key", "apikey", "key", "secret", "client_secret",
    "password", "passwd", "pwd", "auth", "authorization", "credential",
    "session", "sessionid", "session_id", "sid", "jwt", "bearer",
    "signature", "sig", "hash", "otp", "pin", "nonce", "state",
    "ticket", "invite", "share_token", "magic", "sso", "unsubscribe",
    "email", "e-mail", "username", "user", "phone", "ssn",
];

/// Hosts where the query string, not the path, identifies the page. Only the
/// listed parameters survive; everything else is still dropped.
static QUERY_BEARING_HOSTS: &[(&str, &[&str])] = &[
    ("google.com", &["q"]),
    ("google.co.in", &["q"]),
    ("bing.com", &["q"]),
    ("duckduckgo.com", &["q"]),
    ("youtube.com", &["v", "list", "t"]),
    ("github.com", &["q", "tab"]),
    ("figma.com", &["node-id", "page-id"]),
    ("stackoverflow.com", &["q"]),
    ("reddit.com", &["q"]),
    ("linkedin.com", &["keywords"]),
    ("amazon.in", &["k"]),
    ("amazon.com", &["k"]),
];

/// True when the string appears to contain a credential.
pub fn looks_secret(s: &str) -> bool {
    SECRETS.iter().any(|re| re.is_match(s))
}

/// Clean a window title. Returns the app name when the title looks like it
/// carries a secret, and when the app is set to title-blind.
pub fn redact_title(title: &str, app_name: &str, titles_allowed: bool) -> String {
    if !titles_allowed {
        return app_name.to_string();
    }
    let t = collapse_ws(title);
    if t.is_empty() || looks_secret(&t) {
        return app_name.to_string();
    }
    truncate_chars(&t, 300)
}

/// Clean a URL. Returns `None` when the URL cannot be made safe — a magic
/// link, a token in the path — in which case the page is simply not recorded.
pub fn redact_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // Scheme
    let (scheme, rest) = raw.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "file" | "figma" | "vscode" | "slack" | "notion") {
        return None;
    }

    // Fragment. Hash routes identify a page and are kept; anything else goes,
    // because implicit OAuth flows return tokens in the fragment.
    let (rest, fragment) = match rest.split_once('#') {
        Some((r, f)) if f.starts_with('/') => (r, Some(f)),
        Some((r, _)) => (r, None),
        None => (rest, None),
    };

    let (authority_path, query) = match rest.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (rest, None),
    };

    // Strip user:password@ out of the authority.
    let (authority, path) = match authority_path.find('/') {
        Some(i) => (&authority_path[..i], &authority_path[i..]),
        None => (authority_path, ""),
    };
    let host = authority.rsplit('@').next().unwrap_or(authority).to_ascii_lowercase();
    if host.is_empty() && scheme != "file" {
        return None;
    }

    let allowed = allowed_params(&host);
    let keep_query = !allowed.is_empty() || path.len() <= 1;

    let mut out = String::with_capacity(raw.len());
    out.push_str(&scheme);
    out.push_str("://");
    out.push_str(&host);
    out.push_str(path);

    if let (Some(q), true) = (query, keep_query) {
        let kept: Vec<&str> = q
            .split('&')
            .filter(|pair| {
                let key = pair.split('=').next().unwrap_or("").to_ascii_lowercase();
                if SENSITIVE_PARAMS.contains(&key.as_str()) {
                    return false;
                }
                allowed.is_empty() || allowed.contains(&key.as_str())
            })
            .collect();
        if !kept.is_empty() {
            out.push('?');
            out.push_str(&kept.join("&"));
        }
    }

    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }

    // Last gate: a token sitting in the path defeats every rule above.
    if looks_secret(&out) {
        return None;
    }
    Some(truncate_chars(&out, 2000))
}

fn allowed_params(host: &str) -> &'static [&'static str] {
    for (suffix, params) in QUERY_BEARING_HOSTS {
        if host == *suffix || host.ends_with(&format!(".{suffix}")) {
            return params;
        }
    }
    &[]
}

/// Human-facing label for a URL: host without `www.`, plus the page title if
/// one survived redaction.
pub fn url_display(url: &str, title: Option<&str>) -> String {
    let host = url
        .split_once("://")
        .map(|(_, r)| r.split('/').next().unwrap_or(r))
        .unwrap_or(url)
        .trim_start_matches("www.")
        .to_string();
    match title {
        Some(t) if !t.is_empty() => format!("{host} — {t}"),
        _ => host,
    }
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_oauth_codes_and_tokens() {
        let u = redact_url("https://app.example.com/callback?code=abc123&state=xyz&view=grid");
        assert_eq!(u, Some("https://app.example.com/callback".into()));
    }

    #[test]
    fn keeps_the_search_query_that_identifies_the_page() {
        let u = redact_url("https://www.google.com/search?q=otp+autofill&client=firefox");
        assert_eq!(u, Some("https://www.google.com/search?q=otp+autofill".into()));
    }

    #[test]
    fn drops_query_when_the_path_already_identifies_the_page() {
        let u = redact_url("https://stackoverflow.com/questions/12345/otp-autofill?utm_source=x");
        assert_eq!(u, Some("https://stackoverflow.com/questions/12345/otp-autofill".into()));
    }

    #[test]
    fn refuses_a_magic_link() {
        assert_eq!(
            redact_url("https://mail.example.com/login/eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0"),
            None
        );
    }

    #[test]
    fn strips_basic_auth_credentials() {
        let u = redact_url("https://admin:hunter2@internal.example.com/dashboard");
        assert_eq!(u, Some("https://internal.example.com/dashboard".into()));
    }

    #[test]
    fn keeps_figma_node_ids_because_restore_needs_them() {
        let u = redact_url("https://www.figma.com/file/AbCd/Login?node-id=12%3A34&t=zzz");
        assert_eq!(u, Some("https://www.figma.com/file/AbCd/Login?node-id=12%3A34".into()));
    }

    #[test]
    fn drops_a_title_that_carries_a_token() {
        let t = redact_title("api_key = sk_live_abcdefghij0123456789", "Notepad", true);
        assert_eq!(t, "Notepad");
    }

    #[test]
    fn title_blind_apps_report_only_their_name() {
        let t = redact_title("Quarterly figures.xlsx", "Excel", false);
        assert_eq!(t, "Excel");
    }

    #[test]
    fn ordinary_titles_survive() {
        let t = redact_title("  LoginViewModel.kt —  spotted-android ", "VS Code", true);
        assert_eq!(t, "LoginViewModel.kt — spotted-android");
    }
}
