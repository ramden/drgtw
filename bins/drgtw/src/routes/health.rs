use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct HealthResponse {
    status: &'static str,
}

pub(crate) async fn handle() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}
