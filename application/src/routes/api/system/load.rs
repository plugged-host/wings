//! plugged.host: `GET /api/system/load`, the node's load averages and CPU use (the panel's node
//! telemetry).

use super::State;
use utoipa_axum::{router::OpenApiRouter, routes};

mod get {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::ApiError,
    };

    #[utoipa::path(get, path = "/", responses(
        (status = OK, body = crate::processes::Load),
        (status = INTERNAL_SERVER_ERROR, body = ApiError),
    ))]
    pub async fn route() -> ApiResponseResult {
        ApiResponse::new_serialized(crate::processes::load().await?).ok()
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .routes(routes!(get::route))
        .with_state(state.clone())
}
