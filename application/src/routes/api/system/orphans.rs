//! plugged.host: `POST /api/system/orphans`, removes what is left on this node of a server the panel
//! moved to another node without Wings (node failover: this node was down). This node no longer
//! gets the server's configuration from the panel, so the panel sends it along.

use super::State;
use utoipa_axum::{router::OpenApiRouter, routes};

mod post {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::GetState,
    };
    use serde::Serialize;
    use utoipa::ToSchema;

    #[derive(ToSchema, Serialize)]
    struct Response {
        /// The server was still loaded (this node had not restarted) and was deleted like any
        /// other; otherwise its container and files were removed from the configuration sent.
        was_loaded: bool,
    }

    #[utoipa::path(post, path = "/", responses(
        (status = OK, body = inline(Response)),
    ), request_body = crate::remote::servers::RawServer)]
    pub async fn route(
        state: GetState,
        crate::Payload(data): crate::Payload<crate::remote::servers::RawServer>,
    ) -> ApiResponseResult {
        let uuid = data.settings.uuid;

        if let Some(server) = state.server_manager.get_server(uuid).await {
            tracing::info!(server = %uuid, "removing a server the panel moved away");
            state.server_manager.delete_server(&server).await;

            return ApiResponse::new_serialized(Response { was_loaded: true }).ok();
        }

        // never added to the server manager: the instance only exists to tear down what the
        // server left behind (its container, files, mounts, install logs)
        tracing::info!(server = %uuid, "removing the leftovers of a server the panel moved away");
        let server =
            crate::server::Server::new(data.settings, data.process_configuration, state.0.clone());
        server.destroy().await;

        ApiResponse::new_serialized(Response { was_loaded: false }).ok()
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .routes(routes!(post::route))
        .with_state(state.clone())
}
