//! Static frontend serving: built SvelteKit output with SPA fallback,
//! security headers, and an honest placeholder when the UI isn't built yet.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use std::path::PathBuf;
use tower_http::services::ServeDir;

use crate::state::AppState;

pub fn router(web_dir: &PathBuf) -> Router<AppState> {
    let index = web_dir.join("index.html");
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
        .layer(middleware::from_fn(security_headers))
}

async fn security_headers(req: Request<Body>, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    let mut set = |k: &'static str, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set("x-content-type-options", "nosniff");
    set("x-frame-options", "DENY");
    set("referrer-policy", "same-origin");
    set(
        "content-security-policy",
        "default-src 'self'; connect-src 'self'; media-src 'self' blob:; \
         img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
         script-src 'self'; base-uri 'none'; frame-ancestors 'none'",
    );
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
