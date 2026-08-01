//! Wallpaper upload and retrieval.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::wallpapers::{Wallpaper, WallpaperError};

impl From<WallpaperError> for ApiError {
    fn from(error: WallpaperError) -> Self {
        match error {
            WallpaperError::NotFound(_) => ApiError::NotFound(error.to_string()),
            WallpaperError::InvalidId(_)
            | WallpaperError::UnsupportedFormat
            | WallpaperError::TooLarge { .. } => ApiError::Validation(error.to_string()),
            WallpaperError::Io(_) => ApiError::Internal(error.to_string()),
        }
    }
}

#[utoipa::path(
    get, path = "/api/v1/wallpapers", tag = "wallpapers",
    responses((status = 200, description = "Stored wallpapers", body = Vec<Wallpaper>))
)]
pub async fn list(State(state): State<ApiState>) -> Json<Vec<Wallpaper>> {
    Json(state.wallpapers.list())
}

#[utoipa::path(
    put, path = "/api/v1/wallpapers/{id}", tag = "wallpapers",
    params(("id" = String, Path, description = "Wallpaper identifier")),
    request_body(content = Vec<u8>, description = "Raw PNG or JPEG image",
                 content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "The stored wallpaper", body = Wallpaper),
        (status = 422, description = "Not a PNG or JPEG, or too large"),
    )
)]
pub async fn upload(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Wallpaper>> {
    let wallpaper = state.wallpapers.store(&id, &body)?;
    tracing::info!(wallpaper = %id, bytes = wallpaper.bytes, "stored wallpaper");
    // An output already referring to this id should pick up the new image.
    state.trigger.request("wallpaper uploaded");
    Ok(Json(wallpaper))
}

#[utoipa::path(
    get, path = "/api/v1/wallpapers/{id}", tag = "wallpapers",
    params(("id" = String, Path, description = "Wallpaper identifier")),
    responses(
        (status = 200, description = "The image itself", content_type = "image/png"),
        (status = 404, description = "No such wallpaper"),
    )
)]
pub async fn download(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let (bytes, format) = state.wallpapers.read(&id)?;
    Ok(([(header::CONTENT_TYPE, format.content_type())], bytes).into_response())
}

#[utoipa::path(
    delete, path = "/api/v1/wallpapers/{id}", tag = "wallpapers",
    params(("id" = String, Path, description = "Wallpaper identifier")),
    responses(
        (status = 204, description = "Removed"),
        (status = 404, description = "No such wallpaper"),
        (status = 409, description = "Still referenced by an output"),
    )
)]
pub async fn delete(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    // Removing an image something still points at would leave that output
    // permanently diverged, so say no rather than break it. Presets count as
    // users too: a wallpaper reached only through one is no less in use.
    let desired = state.store.get();
    let mut referenced: Vec<String> = desired
        .outputs
        .iter()
        .filter(|output| {
            output
                .background
                .as_ref()
                .and_then(|reference| reference.resolve(&desired.backgrounds))
                .and_then(|background| background.wallpaper.as_deref())
                == Some(id.as_str())
        })
        .map(|output| output.r#match.key())
        .collect();
    referenced.extend(
        desired
            .backgrounds
            .iter()
            .filter(|preset| preset.background.wallpaper.as_deref() == Some(id.as_str()))
            .map(|preset| format!("preset {:?}", preset.id)),
    );
    referenced.dedup();
    if !referenced.is_empty() {
        return Err(ApiError::Conflict(format!(
            "wallpaper {id} is still used by {}; change those outputs first",
            referenced.join(", ")
        )));
    }

    state.wallpapers.remove(&id)?;
    tracing::info!(wallpaper = %id, "removed wallpaper");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use crate::api::test_support::{harness, Harness};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    async fn send(
        harness: &Harness,
        method: &str,
        uri: &str,
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::from(body))
            .unwrap();
        let response = harness.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    #[tokio::test]
    async fn upload_list_download_delete() {
        let harness = harness(None);
        let (status, body) = send(&harness, "PUT", "/api/v1/wallpapers/lobby", PNG.to_vec()).await;
        assert_eq!(status, StatusCode::OK);
        let meta: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(meta["id"], "lobby");
        assert_eq!(meta["contentType"], "image/png");

        let (status, body) = send(&harness, "GET", "/api/v1/wallpapers", vec![]).await;
        assert_eq!(status, StatusCode::OK);
        let listed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 1);

        let (status, bytes) = send(&harness, "GET", "/api/v1/wallpapers/lobby", vec![]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, PNG, "the image must come back byte for byte");

        let (status, _) = send(&harness, "DELETE", "/api/v1/wallpapers/lobby", vec![]).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = send(&harness, "GET", "/api/v1/wallpapers/lobby", vec![]).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_non_image_is_refused() {
        let harness = harness(None);
        let (status, body) = send(
            &harness,
            "PUT",
            "/api/v1/wallpapers/bad",
            b"<html>not an image</html>".to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(problem["detail"].as_str().unwrap().contains("PNG and JPEG"));
    }

    #[tokio::test]
    async fn an_id_cannot_escape_the_directory() {
        let harness = harness(None);
        let (status, _) = send(
            &harness,
            "PUT",
            "/api/v1/wallpapers/..%2Fescape",
            PNG.to_vec(),
        )
        .await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::NOT_FOUND,
            "got {status}"
        );
    }

    #[tokio::test]
    async fn a_wallpaper_in_use_cannot_be_deleted() {
        let harness = harness(None);
        send(&harness, "PUT", "/api/v1/wallpapers/lobby", PNG.to_vec()).await;
        harness
            .state
            .store
            .update(|state| {
                let mut output =
                    crate::model::OutputConfig::new(crate::model::OutputMatch::by_name("HDMI-A-1"));
                output.background = Some(crate::model::BackgroundRef::Inline(
                    crate::model::Background {
                        wallpaper: Some("lobby".into()),
                        color: None,
                        mode: Default::default(),
                    },
                ));
                state.outputs.push(output);
            })
            .unwrap();

        let (status, body) = send(&harness, "DELETE", "/api/v1/wallpapers/lobby", vec![]).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(problem["detail"].as_str().unwrap().contains("HDMI-A-1"));
        // And it is still there.
        let (status, _) = send(&harness, "GET", "/api/v1/wallpapers/lobby", vec![]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn uploads_need_a_token_when_one_is_configured() {
        let harness = harness(Some("secret"));
        let (status, _) = send(&harness, "PUT", "/api/v1/wallpapers/x", PNG.to_vec()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
