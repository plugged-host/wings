//! plugged.host: what a node tells anyone, without a token. Any origin may read it (`main.rs`
//! `handle_cors`).

use super::State;
use utoipa_axum::router::OpenApiRouter;

mod resources;

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .nest("/resources", resources::router(state))
        .with_state(state.clone())
}
