//! A JSON extractor that explains itself when it refuses.
//!
//! Axum's own `Json` rejection answers with a bare status and a plain-text
//! body, which is not the `application/problem+json` shape every other error
//! from this API uses, and which a client parsing JSON sees as nothing at
//! all. That was tolerable while the only way to fail was malformed syntax.
//! It stopped being tolerable when the desired-state types began refusing
//! fields they do not recognise: the whole value of that strictness is
//! telling somebody *which* field, and a 422 with an empty body tells them
//! less than silently accepting it did.
//!
//! This wraps `axum::Json`, keeps the same `Json(value)` spelling for
//! extraction and for responses, and turns a rejection into the usual
//! problem document.

use axum::extract::{FromRequest, Request};
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;

pub struct Json<T>(pub T);

impl<S, T> FromRequest<S> for Json<T>
where
    axum::Json<T>: FromRequest<S, Rejection = axum::extract::rejection::JsonRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(request, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(translate(rejection)),
        }
    }
}

impl<T: serde::Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// Map a rejection onto the error the client should see.
///
/// The distinction that matters to whoever is holding the failing request is
/// between "this is not JSON" and "this is JSON, but not a document I accept";
/// the second is the one that names a field, and it is a validation failure
/// like any other rather than a transport problem.
fn translate(rejection: axum::extract::rejection::JsonRejection) -> ApiError {
    use axum::extract::rejection::JsonRejection;
    let detail = rejection.body_text();
    match rejection {
        JsonRejection::JsonDataError(_) => ApiError::Validation(detail),
        _ => ApiError::BadRequest(detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, StatusCode};

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Document {
        #[allow(dead_code)]
        wanted: u32,
    }

    async fn extract(body: &str) -> Result<Json<Document>, ApiError> {
        let request = Request::builder()
            .method("PUT")
            .uri("/")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        Json::<Document>::from_request(request, &()).await
    }

    #[tokio::test]
    async fn an_unknown_field_is_named_in_the_response() {
        let error = extract(r#"{"wanted":1,"wnated":2}"#).await.err().unwrap();
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("wnated"), "the typo must appear: {text}");
        // The usual envelope, not axum's plain text.
        assert!(text.contains("\"detail\""), "{text}");
    }

    #[tokio::test]
    async fn syntax_errors_are_a_bad_request_not_a_validation_failure() {
        let error = extract("{ this is not json").await.err().unwrap();
        assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_document_that_fits_is_extracted() {
        assert!(extract(r#"{"wanted":7}"#).await.is_ok());
    }
}
