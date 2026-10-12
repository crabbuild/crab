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
    // Repository deep links must survive reloads. Reserve service namespaces
    // and recognize only browser pages so missing APIs/assets still return 404.
    let components: Vec<_> = path.trim_end_matches('/').split('/').collect();
    let application_route = path.is_empty()
        || (components.len() >= 2
            && components[0] != "api"
            && components[0] != "assets"
            && components[0] != "auth"
            && components[0] != "git"
            && (components.len() == 2
                || matches!(
                    components[2],
                    "tree"
                        | "blob"
                        | "blame"
                        | "commits"
                        | "commit"
                        | "issues"
                        | "pulls"
                        | "pull"
                        | "compare"
                        | "branches"
                        | "tags"
                        | "releases"
                        | "settings"
                        | "labels"
                        | "new"
                        | "upload"
                        | "edit"
                        | "delete"
                ))
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
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
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
            ("/team/repo.name", StatusCode::OK),
            ("/team/repo.name/tree/main/src", StatusCode::OK),
            (
                "/team/repo.name/blob/feature%2Fx/src/main.rs",
                StatusCode::OK,
            ),
            ("/team/repo.name/commits/main/src", StatusCode::OK),
            (
                "/team/repo.name/commit/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                StatusCode::OK,
            ),
            ("/team/repo.name/issues/1", StatusCode::OK),
            ("/team/repo.name/pull/2/files", StatusCode::OK),
            ("/team/repo.name/compare/main...feature%2Fx", StatusCode::OK),
            ("/team/repo.name/releases/tag/v1.0.0", StatusCode::OK),
            ("/team/repo.name/settings/access", StatusCode::OK),
            ("/team/repo.name/unknown.js", StatusCode::NOT_FOUND),
            ("/assets/missing.js", StatusCode::NOT_FOUND),
            ("/assets/repo/blob/main/missing.js", StatusCode::NOT_FOUND),
            ("/api/missing", StatusCode::NOT_FOUND),
            ("/api/repos/team/blob/main/missing", StatusCode::NOT_FOUND),
            ("/auth/repo/issues/1", StatusCode::NOT_FOUND),
            ("/git/repo/tree/main", StatusCode::NOT_FOUND),
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

    #[tokio::test]
    async fn executable_assets_receive_browser_media_types() {
        for (extension, content_type) in [
            (".mjs", "text/javascript; charset=utf-8"),
            (".wasm", "application/wasm"),
        ] {
            let path = ASSETS
                .iter()
                .map(|(name, _)| *name)
                .find(|name| name.ends_with(extension))
                .expect("the repository bundle includes this executable asset");
            let response = serve(
                Request::builder()
                    .uri(format!("/{path}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.headers()["content-type"], content_type);
        }
    }
}
