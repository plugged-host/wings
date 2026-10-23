use super::State;
use crate::{
    remote::backups::ResticBackupConfiguration, server::backup::adapters::restic::ResticBackup,
};
use serde::Serialize;
use std::sync::Arc;
use tokio::process::Command;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;

mod prune;
mod stats;
mod unlock;

#[derive(ToSchema, Serialize, Clone)]
pub struct ResticTaskResult {
    started: chrono::DateTime<chrono::Utc>,
    finished: chrono::DateTime<chrono::Utc>,
    duration_ms: u128,
    successful: bool,
    data: serde_json::Value,
    stderr: compact_str::CompactString,
}

fn build_restic_command(
    config: &crate::config::Config,
    configuration: &ResticBackupConfiguration,
) -> Command {
    let mut command = ResticBackup::command(config, configuration);
    command.arg("--json");

    command
}

#[derive(Clone, Copy)]
enum RepositoryLock {
    None,
    Shared,
    Exclusive,
}

async fn execute_restic_command(
    config: &crate::config::Config,
    configuration: &ResticBackupConfiguration,
    lock: RepositoryLock,
    args: impl Fn(&mut Command),
    operation: &'static str,
) -> ResticTaskResult {
    let repository_lock = ResticBackup::repository_lock(configuration).await;
    let (_shared_guard, _exclusive_guard) = match lock {
        RepositoryLock::None => (None, None),
        RepositoryLock::Shared => (Some(repository_lock.read().await), None),
        RepositoryLock::Exclusive => (None, Some(repository_lock.write().await)),
    };

    let started = chrono::Utc::now();
    let mut retried = false;
    let output = loop {
        let mut command = build_restic_command(config, configuration);
        args(&mut command);

        let output = command.output().await;
        if let Ok(output) = &output
            && !retried
            && ResticBackup::recover_from_lock_error(
                config,
                configuration,
                output.status,
                &String::from_utf8_lossy(&output.stderr),
            )
            .await
        {
            retried = true;
            continue;
        }

        break output;
    };
    let finished = chrono::Utc::now();
    let duration_ms = (finished - started).num_milliseconds().max(0) as u128;

    match output {
        Ok(output) => {
            let data =
                serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap_or_else(|_| {
                    serde_json::Value::String(String::from_utf8_lossy(&output.stdout).into_owned())
                });

            ResticTaskResult {
                started,
                finished,
                duration_ms,
                successful: output.status.success(),
                data,
                stderr: compact_str::CompactString::from_utf8_lossy(&output.stderr),
            }
        }
        Err(err) => {
            tracing::error!("failed to run restic {}: {:#?}", operation, err);

            ResticTaskResult {
                started,
                finished,
                duration_ms,
                successful: false,
                data: serde_json::Value::Null,
                stderr: compact_str::format_compact!("failed to spawn restic: {err}"),
            }
        }
    }
}

fn system_restic_configuration(config: &crate::config::Config) -> Arc<ResticBackupConfiguration> {
    Arc::new(ResticBackup::system_configuration(config))
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .nest("/stats", stats::router(state))
        .nest("/prune", prune::router(state))
        .nest("/unlock", unlock::router(state))
        .with_state(state.clone())
}
