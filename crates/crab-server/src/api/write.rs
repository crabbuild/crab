use crate::app::Error;
use axum::{
    http::HeaderValue,
    response::{IntoResponse, Response},
};

// Keep handler errors small while retaining the complete response, including
// the write-outcome header used to recover an uncertain mutation.
#[derive(Debug)]
pub(super) struct ResponseError(Box<Response>);

impl From<Response> for ResponseError {
    fn from(response: Response) -> Self {
        Self(Box::new(response))
    }
}

impl IntoResponse for ResponseError {
    fn into_response(self) -> Response {
        *self.0
    }
}

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
