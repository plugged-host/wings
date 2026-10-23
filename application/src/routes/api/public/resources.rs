//! plugged.host: `GET /api/public/resources`, the node's RAM, disk and CPU for anyone (a status page,
//! a "which location has room" list). `api.public_resources: false` turns it off.

use super::State;
use utoipa_axum::{router::OpenApiRouter, routes};

mod get {
    use crate::{
        response::{ApiResponse, ApiResponseResult},
        routes::{ApiError, GetState},
    };
    use axum::http::StatusCode;
    use serde::Serialize;
    use utoipa::ToSchema;

    const MIB: u64 = 1024 * 1024;

    #[derive(ToSchema, Serialize, Default, Debug, PartialEq)]
    struct Amount {
        /// Bytes.
        total: u64,
        used: u64,
        free: u64,
        /// What the servers on this node may use together, by their limits. Servers without a
        /// limit are not counted.
        allocated: u64,
    }

    #[derive(ToSchema, Serialize)]
    struct Cpu {
        model: String,
        threads: usize,
        /// All threads together, 0 to 100.
        used_percent: f32,
        /// Threads the servers may use together, by their limits (a limit of 150% is 1.5).
        allocated_threads: f64,
    }

    #[derive(ToSchema, Serialize)]
    struct Response {
        #[schema(inline)]
        memory: Amount,
        /// The filesystem that holds the servers' files.
        #[schema(inline)]
        disk: Amount,
        #[schema(inline)]
        cpu: Cpu,
    }

    #[utoipa::path(get, path = "/", responses(
        (status = OK, body = inline(Response)),
        (status = NOT_FOUND, body = ApiError),
    ))]
    pub async fn route(state: GetState) -> ApiResponseResult {
        if !state.config.load().api.public_resources {
            return ApiResponse::error("not found")
                .with_status(StatusCode::NOT_FOUND)
                .ok();
        }

        let stats = state.stats_manager.get_stats();

        let (mut memory_allocated, mut disk_allocated, mut cpu_allocated) = (0, 0, 0);
        for server in state.server_manager.get_servers().await.iter() {
            let build = &server.configuration.read().await.build;
            memory_allocated += mib(build.memory_limit);
            disk_allocated += build.disk_space * MIB;
            // borrowed CPU (burst cores) is not allocated
            cpu_allocated += (build.cpu_limit - build.cpu_burst.unwrap_or(0) as i64).max(0) as u64;
        }

        let data_directory = state
            .config
            .resolve_as_path(|config| &config.system.data_directory);
        let disk = tokio::task::spawn_blocking(move || filesystem(&data_directory))
            .await
            .unwrap_or_default();

        let memory_total = stats.memory_total();
        let memory_used = stats.memory_used().min(memory_total);

        ApiResponse::new_serialized(Response {
            memory: Amount {
                total: memory_total,
                used: memory_used,
                free: memory_total - memory_used,
                allocated: memory_allocated,
            },
            disk: Amount {
                allocated: disk_allocated,
                ..disk
            },
            cpu: Cpu {
                model: stats.cpu_model().to_string(),
                threads: stats.cpu_threads(),
                used_percent: (stats.cpu_used() * 10.0).round() / 10.0,
                allocated_threads: cpu_allocated as f64 / 100.0,
            },
        })
        .with_header("Cache-Control", "public, max-age=5")
        .ok()
    }

    /// A server limit in MiB as bytes; 0 (no limit) and below count as nothing.
    fn mib(limit: i64) -> u64 {
        u64::try_from(limit).unwrap_or_default() * MIB
    }

    /// Size, used and free space of the filesystem holding `path`.
    #[cfg(unix)]
    fn filesystem(path: &std::path::Path) -> Amount {
        let Ok(stat) = rustix::fs::statvfs(path) else {
            return Amount::default();
        };
        let block = stat.f_frsize;

        let total = stat.f_blocks * block;
        Amount {
            total,
            used: total.saturating_sub(stat.f_bfree * block),
            free: stat.f_bavail * block,
            allocated: 0,
        }
    }

    #[cfg(not(unix))]
    fn filesystem(path: &std::path::Path) -> Amount {
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let Some(disk) = disks
            .iter()
            .filter(|disk| path.starts_with(disk.mount_point()))
            .max_by_key(|disk| disk.mount_point().as_os_str().len())
        else {
            return Amount::default();
        };

        Amount {
            total: disk.total_space(),
            used: disk.total_space().saturating_sub(disk.available_space()),
            free: disk.available_space(),
            allocated: 0,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn limits_count_as_bytes_and_no_limit_as_nothing() {
            assert_eq!(mib(2048), 2048 * MIB);
            assert_eq!(mib(0), 0);
            assert_eq!(mib(-1), 0);
        }

        #[cfg(unix)]
        #[test]
        fn the_root_filesystem_has_a_size() {
            let root = filesystem(std::path::Path::new("/"));

            assert!(root.total > 0);
            assert!(root.used <= root.total);
            assert!(root.free <= root.total);
        }
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .routes(routes!(get::route))
        .with_state(state.clone())
}
