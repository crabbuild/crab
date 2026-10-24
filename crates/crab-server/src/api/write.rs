use crate::app::Error;
use axum::{
    http::HeaderValue,
    response::{IntoResponse, Response},
};

// Mark only immutable-payload validation or authoritative native rejection.
// Access, routing, transport and identity-conflict errors can follow an earlier
// committed attempt; they must retain the original intent on retry.
pub(super) fn rejected(error: Error) -> Response {
    let mut response = error.into_response();
    response
        .headers_mut()
        .insert("x-crab-write-outcome", HeaderValue::from_static("rejected"));
    response
}
