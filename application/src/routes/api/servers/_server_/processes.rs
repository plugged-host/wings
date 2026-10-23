//! plugged.host: `GET /api/servers/{server}/processes`, what runs in one server's container
//! (`src/processes.rs`). `container` is null while the server is not running.

use super::State;
use utoipa_axum::{router::OpenApiRouter, routes};

mod get {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::{ApiError, GetState, api::servers::_server_::GetServer},
    };
    use serde::Serialize;
    use utoipa::ToSchema;

    #[derive(ToSchema, Serialize)]
    struct Response {
        container: Option<crate::processes::Container>,
        /// Seconds CPU use was measured over.
        span: f64,
    }

    #[utoipa::path(get, path = "/", responses(
        (status = OK, body = inline(Response)),
        (status = NOT_FOUND, body = ApiError),
        (status = INTERNAL_SERVER_ERROR, body = ApiError),
    ), params(
        (
            "server" = uuid::Uuid,
            description = "The server uuid",
            example = "123e4567-e89b-12d3-a456-426614174000",
        ),
    ))]
    pub async fn route(state: GetState, server: GetServer) -> ApiResponseResult {
        let servers = crate::processes::container_servers(&state).await;
        let report = crate::processes::report(servers).await?;

        ApiResponse::new_serialized(Response {
            container: report
                .containers
                .into_iter()
                .find(|container| container.server == Some(server.uuid)),
            span: report.span,
        })
        .ok()
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .routes(routes!(get::route))
        .with_state(state.clone())
}
