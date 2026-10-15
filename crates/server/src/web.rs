//! Static frontend serving: built SvelteKit output with SPA fallback,
//! security headers, and an honest placeholder when the UI isn't built yet.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use base64::Engine;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower_http::services::ServeDir;

use crate::state::AppState;

pub fn router(web_dir: &PathBuf) -> Router<AppState> {
    let index = web_dir.join("index.html");
    let csp: Arc<HeaderValue> = Arc::new(
        HeaderValue::from_str(&content_security_policy(&index))
            .unwrap_or_else(|_| HeaderValue::from_static("default-src 'self'")),
    );
    let service = ServeDir::new(web_dir)
        .append_index_html_on_directories(true)
        .fallback(tower::service_fn(move |_| {
            let index = index.clone();
            async move {
                if index.exists() {
                    match tokio::fs::read(&index).await {
                        Ok(bytes) => Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                                .body(Body::from(bytes))
                                .unwrap(),
                        ),
                        Err(e) => Ok(Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body(Body::from(e.to_string()))
                            .unwrap()),
                    }
                } else {
                    Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from(PLACEHOLDER))
                        .unwrap())
                }
            }
        }));
    Router::new()
        .fallback_service(service)
        .layer(middleware::from_fn(
            move |req: Request<Body>, next: Next| {
                let csp = csp.clone();
                async move { security_headers(req, next, &csp).await }
            },
        ))
}

/// CSP for the UI. SvelteKit's static build boots from an inline script, so
/// its exact hash is allowed rather than opening `script-src` to
/// `'unsafe-inline'`. Computed once at startup: restart after rebuilding
/// the UI.
fn content_security_policy(index: &Path) -> String {
    let html = std::fs::read_to_string(index).unwrap_or_default();
    let hashes: String = inline_scripts(&html)
        .iter()
        .map(|s| {
            let digest = Sha256::digest(s.as_bytes());
            format!(
                " 'sha256-{}'",
                base64::engine::general_purpose::STANDARD.encode(digest)
            )
        })
        .collect();
    format!(
        "default-src 'self'; connect-src 'self'; media-src 'self' blob:; \
         img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
         script-src 'self'{hashes}; base-uri 'none'; frame-ancestors 'none'"
    )
}

/// Bodies of `<script>` elements without a `src` attribute.
fn inline_scripts(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        let after = &rest[start..];
        let Some(tag_end) = after.find('>') else {
            break;
        };
        let tag = &after[..tag_end];
        let body_start = tag_end + 1;
        let Some(close) = after[body_start..].find("</script>") else {
            break;
        };
        if !tag.contains(" src=") {
            out.push(&after[body_start..body_start + close]);
        }
        rest = &after[body_start + close..];
    }
    out
}

async fn security_headers(req: Request<Body>, next: Next, csp: &HeaderValue) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    let mut set = |k: &'static str, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set("x-content-type-options", "nosniff");
    set("x-frame-options", "DENY");
    set("referrer-policy", "same-origin");
    h.insert("content-security-policy", csp.clone());
    resp
}

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Adrian</title>
<style>body{background:#121417;color:#e6e8eb;font:16px/1.5 system-ui;
display:grid;place-items:center;min-height:100vh;margin:0}
main{max-width:34rem;padding:2rem}code{background:#1e2126;padding:.15em .4em;
border-radius:6px}a{color:#7aa7ff}</style></head>
<body><main><h1>Adrian server is running</h1>
<p>The web interface has not been built yet. From the repository root:</p>
<pre><code>cd web
npm ci
npm run build</code></pre>
<p>Then restart the server. The API is available under <code>/api/</code>;
see <code>/api/health</code>.</p>
</main></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_allows_exactly_the_inline_boot_script() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("index.html");
        std::fs::write(
            &index,
            "<html><head><script type=\"module\" src=\"/_app/x.js\"></script></head>\
             <body><script>\n  boot();\n</script></body></html>",
        )
        .unwrap();
        let csp = content_security_policy(&index);
        let want =
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(b"\n  boot();\n"));
        assert!(
            csp.contains(&format!("script-src 'self' 'sha256-{want}';")),
            "{csp}"
        );
        assert!(!csp.contains("script-src 'self' 'unsafe-inline'"));
        // No UI built yet: plain 'self'.
        assert!(
            content_security_policy(&dir.path().join("missing.html"))
                .contains("script-src 'self';")
        );
    }
}
