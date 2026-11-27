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
    // Application deep links must survive reloads. Reserve service namespaces
    // and recognize only browser pages so missing APIs/assets still return 404.
    let components: Vec<_> = path.trim_end_matches('/').split('/').collect();
    let valid_id = |id: &str| uuid::Uuid::parse_str(id).is_ok_and(|id| !id.is_nil());
    let valid_organization = |segment: &str| {
        let id = match segment.rsplit_once("--") {
            Some((slug, id)) if !slug.is_empty() => id,
            Some(_) => return false,
            None => segment,
        };
        valid_id(id)
    };
    // Mirror the page routes of packages/ui (app/index.tsx and
    // lib/repository-route.ts); a page missing here returns 404 on reload.
    let global_route = match components.as_slice() {
        [
            "-",
            "profile" | "notifications" | "code-search" | "discussion-search" | "organizations"
            | "public" | "work" | "repositories" | "settings" | "new",
        ]
        | ["-", "notifications", "watched" | "subscriptions"]
        | ["-", "repositories", "personal"]
        | ["-", "organizations", "new"]
        | [
            "-",
            "settings",
            "profile"
            | "account"
            | "appearance"
            | "notifications"
            | "git-access"
            | "organizations"
            | "repositories"
            | "saved-replies"
            | "review-availability",
        ] => true,
        ["-", "users" | "repository-transfers", id] | ["-", "users", id, "repositories"] => {
            valid_id(id)
        }
        ["-", "organizations", id] | ["-", "organizations", id, "invitation" | "projects"] => {
            valid_organization(id)
        }
        ["-", "organizations", organization, "projects", project] => {
            valid_organization(organization) && valid_id(project)
        }
        _ => false,
    };
    let application_route = path.is_empty()
        || global_route
        || (components.len() == 1 && crate::catalog::valid_organization_handle(components[0]))
        || (components.len() >= 2
            && components[0] != "-"
            && components[0] != "api"
            && components[0] != "assets"
            && components[0] != "auth"
            && components[0] != "git"
            && (components.len() == 2
                || (components.len() == 3
                    && matches!(
                        components[2],
                        "code-search"
                            | "fork-compare"
                            | "forks"
                            | "projects"
                            | "rulesets"
                            | "security"
                    ))
                || matches!(components.as_slice(), [_, _, "fork-compare", _])
                || matches!(components.as_slice(), [_, _, "projects", id]
                    if uuid::Uuid::parse_str(id).is_ok_and(|id| !id.is_nil()))
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
                        | "milestones"
                        | "activity"
                        | "results"
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
            ("/team/repo.name/milestones/1", StatusCode::OK),
            ("/team/repo.name/milestones/new", StatusCode::OK),
            ("/team/repo.name/pull/2/files", StatusCode::OK),
            ("/team/repo.name/compare/main...feature%2Fx", StatusCode::OK),
            ("/team/repo.name/releases/tag/v1.0.0", StatusCode::OK),
            ("/team/repo.name/settings/access", StatusCode::OK),
            ("/team/repo.name/code-search", StatusCode::OK),
            ("/team/repo.name/projects", StatusCode::OK),
            (
                "/team/repo.name/projects/01900000-0000-7000-8000-000000000001",
                StatusCode::OK,
            ),
            ("/team/repo.name/projects/unknown", StatusCode::NOT_FOUND),
            (
                "/team/repo.name/projects/00000000-0000-0000-0000-000000000000",
                StatusCode::NOT_FOUND,
            ),
            (
                "/team/repo.name/projects/01900000-0000-7000-8000-000000000001/unknown",
                StatusCode::NOT_FOUND,
            ),
            ("/team/repo.name/settings/properties", StatusCode::OK),
            ("/team/repo.name/rulesets", StatusCode::OK),
            ("/team/repo.name/properties", StatusCode::NOT_FOUND),
            ("/team/repo.name/properties/unknown", StatusCode::NOT_FOUND),
            ("/crabbuild", StatusCode::OK),
            ("/bad--handle", StatusCode::NOT_FOUND),
            ("/team/repo.name/forks", StatusCode::OK),
            ("/team/repo.name/forks/unknown", StatusCode::NOT_FOUND),
            ("/team/repo.name/fork-compare", StatusCode::OK),
            ("/team/repo.name/fork-compare/feature%2Fx", StatusCode::OK),
            (
                "/team/repo.name/fork-compare/feature%2Fx/unknown",
                StatusCode::NOT_FOUND,
            ),
            ("/team/repo.name/security?branch=main", StatusCode::OK),
            ("/team/repo.name/security/unknown", StatusCode::NOT_FOUND),
            ("/team/repo.name/results", StatusCode::OK),
            (
                "/team/repo.name/results?report=ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                StatusCode::OK,
            ),
            ("/-/profile", StatusCode::OK),
            ("/-/code-search", StatusCode::OK),
            (
                "/-/code-search?organization=01900000-0000-7000-8000-000000000001&q=match",
                StatusCode::OK,
            ),
            ("/-/code-search/unknown", StatusCode::NOT_FOUND),
            ("/-/work?kind=issue&state=closed", StatusCode::OK),
            ("/-/work/unknown", StatusCode::NOT_FOUND),
            (
                "/-/discussion-search?q=release&scope=team%2Frepo",
                StatusCode::OK,
            ),
            ("/-/discussion-search/unknown", StatusCode::NOT_FOUND),
            ("/-/settings", StatusCode::OK),
            ("/-/settings/profile", StatusCode::OK),
            ("/-/settings/account", StatusCode::OK),
            ("/-/settings/appearance", StatusCode::OK),
            ("/-/settings/notifications", StatusCode::OK),
            ("/-/settings/git-access", StatusCode::OK),
            ("/-/settings/organizations", StatusCode::OK),
            ("/-/settings/repositories", StatusCode::OK),
            ("/-/settings/saved-replies", StatusCode::OK),
            ("/-/settings/review-availability", StatusCode::OK),
            ("/-/settings/unknown", StatusCode::NOT_FOUND),
            (
                "/-/settings/review-availability/unknown",
                StatusCode::NOT_FOUND,
            ),
            ("/-/notifications", StatusCode::OK),
            ("/-/notifications/watched", StatusCode::OK),
            ("/-/notifications/subscriptions", StatusCode::OK),
            (
                "/-/users/01900000-0000-7000-8000-000000000001",
                StatusCode::OK,
            ),
            ("/-/repositories", StatusCode::OK),
            ("/-/repositories?q=crab", StatusCode::OK),
            ("/-/repositories/personal", StatusCode::OK),
            ("/-/repositories/unknown", StatusCode::NOT_FOUND),
            ("/-/public?q=open-library", StatusCode::OK),
            ("/-/public/unknown", StatusCode::NOT_FOUND),
            ("/-/organizations", StatusCode::OK),
            ("/-/new", StatusCode::OK),
            (
                "/-/new?organization=01900000-0000-7000-8000-000000000001",
                StatusCode::OK,
            ),
            ("/-/new/unknown", StatusCode::NOT_FOUND),
            ("/-/organizations/new", StatusCode::OK),
            ("/-/organizations/new/unknown", StatusCode::NOT_FOUND),
            (
                "/-/users/01900000-0000-7000-8000-000000000001/repositories",
                StatusCode::OK,
            ),
            (
                "/-/organizations/crab-engineering--01900000-0000-7000-8000-000000000001/invitation",
                StatusCode::OK,
            ),
            (
                "/-/organizations/crab-engineering--00000000-0000-0000-0000-000000000000",
                StatusCode::NOT_FOUND,
            ),
            (
                "/-/organizations/01900000-0000-7000-8000-000000000001",
                StatusCode::OK,
            ),
            (
                "/-/organizations/01900000-0000-7000-8000-000000000001/invitation",
                StatusCode::OK,
            ),
            (
                "/-/organizations/01900000-0000-7000-8000-000000000001/projects",
                StatusCode::OK,
            ),
            (
                "/-/organizations/01900000-0000-7000-8000-000000000001/projects/01900000-0000-7000-8000-000000000002",
                StatusCode::OK,
            ),
            (
                "/-/organizations/not-a-uuid/projects",
                StatusCode::NOT_FOUND,
            ),
            (
                "/-/organizations/01900000-0000-7000-8000-000000000001/projects/not-a-uuid",
                StatusCode::NOT_FOUND,
            ),
            (
                "/-/organizations/01900000-0000-7000-8000-000000000001/projects/00000000-0000-0000-0000-000000000000",
                StatusCode::NOT_FOUND,
            ),
            ("/-/organizations/not-a-uuid", StatusCode::NOT_FOUND),
            (
                "/-/organizations/00000000-0000-0000-0000-000000000000",
                StatusCode::NOT_FOUND,
            ),
            (
                "/-/organizations/01900000-0000-7000-8000-000000000001/unknown",
                StatusCode::NOT_FOUND,
            ),
            ("/-/unknown", StatusCode::NOT_FOUND),
            ("/team/repo.name/unknown.js", StatusCode::NOT_FOUND),
            ("/assets/missing.js", StatusCode::NOT_FOUND),
            ("/assets/repo/blob/main/missing.js", StatusCode::NOT_FOUND),
            ("/api/missing", StatusCode::NOT_FOUND),
            ("/api/repos/team/blob/main/missing", StatusCode::NOT_FOUND),
            ("/auth/repo/issues/1", StatusCode::NOT_FOUND),
            ("/git/repo/tree/main", StatusCode::NOT_FOUND),
            ("/unknown--handle", StatusCode::NOT_FOUND),
            ("/-unknown", StatusCode::NOT_FOUND),
            ("/unknown", StatusCode::OK),
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
