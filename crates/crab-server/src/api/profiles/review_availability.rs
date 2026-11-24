use super::*;
use crate::cells::directory::review_availability::{
    Change, GetAvailability, Input, ReviewAvailability, SetAvailability,
};

pub(super) async fn get(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<ReviewAvailability>> {
    let identity = app::account(&principal)?;
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    let availability = app::query_output(
        cells
            .query::<GetAvailability>(Address::Directory, None, identity)
            .await,
    )?;
    app::account(&principal)?;
    Ok(Json(availability))
}

pub(super) async fn put(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    input: std::result::Result<Json<Change>, JsonRejection>,
) -> Result<Json<ReviewAvailability>> {
    let identity = app::account(&principal)?;
    let Json(change) = input.map_err(app::Error::Body)?;
    if change.version >= app::MAX_NUMBER {
        return Err(app::Error::Invalid(
            "Review availability version is invalid",
        ));
    }
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    let availability = app::command_output(
        cells
            .command::<SetAvailability>(
                Address::Directory,
                app::mutation_identity()?,
                Input { identity, change },
            )
            .await,
    )?;
    app::account(&principal)?;
    Ok(Json(availability.ok_or(app::Error::Conflict)?))
}

#[cfg(test)]
#[tokio::test]
async fn review_availability_retries_keep_the_original_expiry_and_reject_a_stale_change() {
    use crate::test_support::http::Harness;
    use axum::http::header;
    use reqwest::Method;
    use serde_json::{Value, json};

    let h = Harness::new(false).await;
    let session = h.login().await;
    let route = "/api/profile/review-availability";
    let choice = json!({"version":0,"choice":"thirty_minutes"});
    assert_eq!(
        h.http
            .put(format!("{}{route}", h.origin))
            .header(header::COOKIE, &session)
            .json(&choice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let saved = h
        .write(Method::PUT, route, &session, choice.clone())
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["busy"], true);
    assert!(saved["expires_at_ms"].is_number());
    let retry = h
        .write(Method::PUT, route, &session, choice)
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(retry, saved);
    let stale = h
        .write(
            Method::PUT,
            route,
            &session,
            json!({"version":0,"choice":"available"}),
        )
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(h.json(route, &session).await, saved);
    h.close().await;
}
