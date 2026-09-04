use axum::{
    body::Body,
    extract::Request,
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};
include!(concat!(env!("OUT_DIR"), "/assets.rs"));

pub(crate) async fn serve(request: Request) -> Response {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return (StatusCode::METHOD_NOT_ALLOWED, [("allow", "GET, HEAD")]).into_response();
    }
    let path = request.uri().path().trim_start_matches('/');
    let asset = ASSETS.iter().find(|(name, _)| *name == path);
    // Only the catalog and repository routes fall back to the application shell.
    // Dots are valid in repository identifiers; missing assets still return 404.
    let components: Vec<_> = path.split('/').collect();
    let application_route = path.is_empty()
        || (components.len() == 2
            && components[0] != "api"
            && components[0] != "assets"
            && components[0] != "auth"
            && components
                .iter()
                .all(|part| !part.is_empty() && !matches!(*part, "." | "..")));
    let asset = asset.or_else(|| {
        application_route
            .then(|| ASSETS.iter().find(|(name, _)| *name == "index.html"))
            .flatten()
    });
    let Some((name, bytes)) = asset else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content_type = match name.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        Some("wasm") => "application/wasm",
        Some("woff2") => "font/woff2",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    };
    let cache = if name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let body = if request.method() == Method::HEAD {
        Body::empty()
    } else {
        Body::from(*bytes)
    };
    (
        [
            ("content-type", content_type.to_owned()),
            ("content-length", bytes.len().to_string()),
            ("cache-control", cache.to_owned()),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn repository_deep_links_allow_dots_but_unknown_assets_are_not_html() {
        for (path, expected) in [
            ("/team/repo.name?path=524541444d452e6d64", StatusCode::OK),
            ("/assets/missing.js", StatusCode::NOT_FOUND),
            ("/api/missing", StatusCode::NOT_FOUND),
            ("/unknown", StatusCode::NOT_FOUND),
        ] {
            let response = serve(Request::builder().uri(path).body(Body::empty()).unwrap()).await;
            assert_eq!(response.status(), expected, "{path}");
        }
    }

    #[tokio::test]
    async fn head_returns_representation_headers_without_a_body() {
        let response = serve(
            Request::builder()
                .method(Method::HEAD)
                .uri("/")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(
            response.headers()["content-length"]
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap()
                > 0
        );
        assert!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );
    }
}
