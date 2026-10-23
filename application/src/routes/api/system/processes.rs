//! plugged.host: `GET /api/system/processes`, everything that runs on the node: each server
//! container's processes and connections, and the host's processes (the panel's abuse protection,
//! `src/processes.rs`).

use super::State;
use utoipa_axum::{router::OpenApiRouter, routes};

mod get {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::{ApiError, GetState},
    };

    #[utoipa::path(get, path = "/", responses(
        (status = OK, body = crate::processes::Report),
        (status = INTERNAL_SERVER_ERROR, body = ApiError),
    ))]
    pub async fn route(state: GetState) -> ApiResponseResult {
        let servers = crate::processes::container_servers(&state).await;

        ApiResponse::new_serialized(crate::processes::report(servers).await?).ok()
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .routes(routes!(get::route))
        .with_state(state.clone())
}
