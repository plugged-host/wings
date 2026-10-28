//! plugged.host: burst operations, a fast lane for servers with burst CPU (paying members).
//!
//! Archives, unpacking and backups run in the Wings process, outside the server's container, so a
//! server's CPU limit never applies to them: their speed is the node's thread counts
//! (`api.file_compression_threads`, `api.file_decompression_threads`,
//! `system.backups.*.create_threads`). The panel sends `build.cpu_burst` (the burst CPU in
//! percent, 100 per burst core) for the servers of paying members. For those the thread counts are
//! multiplied by `1 + cpu_burst / 100`, up to the node's cores, and restic reads more files at
//! once. A server without burst CPU keeps the configured counts and runs its restic backups at a
//! lower CPU priority, so members' backups win while the node is busy (on an idle node nothing
//! changes for it). `api.burst_operations: false` turns all of it off.

use std::collections::BTreeMap;

/// restic's own default for `RESTIC_READ_CONCURRENCY`: files `restic backup` reads at once.
const RESTIC_READ_CONCURRENCY: usize = 2;
/// How much nicer a yielding restic backup runs. A process without an IO priority of its own gets
/// one derived from its nice value (best effort, level `(nice + 20) / 5`), so under a scheduler
/// that weighs priorities (BFQ) its reads also drop to best effort 6.
const YIELD_NICE: i32 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FastLane {
    /// Burst operations are off on this node, or the server's CPU is unlimited: upstream's
    /// behaviour.
    Neutral,
    /// A server without burst CPU: the configured threads, background work yields.
    Free,
    /// A server with burst CPU, in percent (100 per burst core).
    Burst(u32),
}

impl FastLane {
    /// The lane of `server`, from its live configuration (every sync updates `cpu_burst`).
    pub async fn of(server: &super::InnerServer) -> Self {
        let enabled = server.app_state.config.load().api.burst_operations;
        if !enabled {
            return Self::Neutral;
        }

        let configuration = server.configuration.read().await;
        Self::from_build(configuration.build.cpu_limit, configuration.build.cpu_burst)
    }

    fn from_build(cpu_limit: i64, cpu_burst: Option<u32>) -> Self {
        match cpu_burst {
            Some(burst) if burst > 0 => Self::Burst(burst),
            _ if cpu_limit <= 0 => Self::Neutral,
            _ => Self::Free,
        }
    }

    /// A configured thread count (0 meaning every core) for this lane.
    pub fn threads(self, base: usize) -> usize {
        match self {
            Self::Burst(burst) => {
                boosted_threads(base, burst, crate::threading::resolve_threads(0))
            }
            Self::Neutral | Self::Free => base,
        }
    }

    /// The threads to unpack an archive of `server` with: `api.file_decompression_threads` for
    /// its lane.
    pub async fn decompression_threads(server: &super::InnerServer) -> usize {
        let base = server.app_state.config.load().api.file_decompression_threads;

        Self::of(server).await.threads(base)
    }
}

/// `base × (1 + burst / 100)`, at most `cores` but never below `base`. A `base` of 0 (every core)
/// stays 0.
pub fn boosted_threads(base: usize, burst: u32, cores: usize) -> usize {
    if base == 0 {
        return 0;
    }

    let boosted = (base as u64).saturating_mul(100 + u64::from(burst)) / 100;

    usize::try_from(boosted)
        .unwrap_or(usize::MAX)
        .min(cores)
        .max(base)
}

/// Applies a [`FastLane`] to a `restic backup` command (never to restores or other commands).
pub trait ResticBackupLane {
    /// A burst server reads more files at once (`RESTIC_READ_CONCURRENCY`, unless the
    /// repository's environment or Wings' own already sets it; a restic without the variable
    /// ignores it), a free one runs at a lower CPU priority.
    fn backup_lane(&mut self, lane: FastLane, environment: &BTreeMap<String, String>) -> &mut Self;
}

impl ResticBackupLane for tokio::process::Command {
    fn backup_lane(
        &mut self,
        lane: FastLane,
        environment: &BTreeMap<String, String>,
    ) -> &mut Self {
        if matches!(lane, FastLane::Burst(_))
            && !environment.contains_key("RESTIC_READ_CONCURRENCY")
            && std::env::var_os("RESTIC_READ_CONCURRENCY").is_none()
        {
            self.env(
                "RESTIC_READ_CONCURRENCY",
                lane.threads(RESTIC_READ_CONCURRENCY).to_string(),
            );
        }

        if lane == FastLane::Free {
            yield_priority(self);
        }

        self
    }
}

#[cfg(unix)]
fn yield_priority(command: &mut tokio::process::Command) {
    // SAFETY: the closure runs in the forked child before exec and only makes the getpriority
    // and setpriority system calls: no allocation, no locks
    unsafe {
        command.pre_exec(|| {
            // a failed renice leaves the backup at normal priority, it never fails it
            rustix::process::nice(YIELD_NICE).ok();

            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn yield_priority(_command: &mut tokio::process::Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(command: &tokio::process::Command, key: &str) -> Option<String> {
        command
            .as_std()
            .get_envs()
            .find(|(name, _)| *name == key)
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned())
    }

    // FastLane::from_build

    #[test]
    fn burst_cpu_puts_a_server_in_the_fast_lane() {
        assert_eq!(FastLane::from_build(300, Some(100)), FastLane::Burst(100));
        assert_eq!(FastLane::from_build(400, Some(200)), FastLane::Burst(200));
    }

    #[test]
    fn a_server_without_burst_cpu_is_free() {
        assert_eq!(FastLane::from_build(200, None), FastLane::Free);
        assert_eq!(FastLane::from_build(200, Some(0)), FastLane::Free);
    }

    #[test]
    fn unlimited_cpu_without_burst_is_left_alone() {
        assert_eq!(FastLane::from_build(0, None), FastLane::Neutral);
        assert_eq!(FastLane::from_build(0, Some(100)), FastLane::Burst(100));
    }

    // FastLane::threads, boosted_threads

    #[test]
    fn only_the_fast_lane_gets_more_threads() {
        assert_eq!(FastLane::Free.threads(2), 2);
        assert_eq!(FastLane::Neutral.threads(4), 4);
        assert_eq!(FastLane::Free.threads(0), 0);
    }

    #[test]
    fn burst_multiplies_the_threads_up_to_the_cores() {
        // Plus (1 burst core) and Premium (2) on an 8 and a 16 core node
        assert_eq!(boosted_threads(2, 100, 8), 4);
        assert_eq!(boosted_threads(2, 200, 8), 6);
        assert_eq!(boosted_threads(4, 100, 8), 8);
        assert_eq!(boosted_threads(4, 200, 8), 8);
        assert_eq!(boosted_threads(4, 100, 16), 8);
        assert_eq!(boosted_threads(4, 200, 16), 12);

        // half a burst core
        assert_eq!(boosted_threads(4, 50, 16), 6);
    }

    #[test]
    fn burst_never_lowers_the_configured_threads() {
        assert_eq!(boosted_threads(12, 200, 8), 12);
        assert_eq!(boosted_threads(3, 10, 16), 3);
        assert_eq!(boosted_threads(1, 100, 1), 1);
    }

    #[test]
    fn every_core_stays_every_core() {
        assert_eq!(boosted_threads(0, 200, 8), 0);
    }

    #[test]
    fn a_huge_burst_does_not_overflow() {
        assert_eq!(boosted_threads(usize::MAX, u32::MAX, 8), usize::MAX);
        assert_eq!(boosted_threads(4, u32::MAX, 8), 8);
    }

    // ResticBackupLane::backup_lane

    #[test]
    fn a_burst_backup_reads_more_files_at_once() {
        if std::env::var_os("RESTIC_READ_CONCURRENCY").is_some() {
            eprintln!("skipping, RESTIC_READ_CONCURRENCY is set for the tests");
            return;
        }

        let cores = crate::threading::resolve_threads(0);

        let mut command = tokio::process::Command::new("restic");
        command.backup_lane(FastLane::Burst(100), &BTreeMap::new());
        assert_eq!(
            env(&command, "RESTIC_READ_CONCURRENCY"),
            Some(boosted_threads(2, 100, cores).to_string())
        );

        let mut command = tokio::process::Command::new("restic");
        command.backup_lane(FastLane::Free, &BTreeMap::new());
        assert_eq!(env(&command, "RESTIC_READ_CONCURRENCY"), None);
    }

    #[test]
    fn a_configured_read_concurrency_wins() {
        let environment =
            BTreeMap::from([("RESTIC_READ_CONCURRENCY".to_string(), "1".to_string())]);

        let mut command = tokio::process::Command::new("restic");
        command.backup_lane(FastLane::Burst(200), &environment);
        assert_eq!(env(&command, "RESTIC_READ_CONCURRENCY"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_free_backup_runs_niced() -> Result<(), anyhow::Error> {
        let before = rustix::process::getpriority_process(None)?;
        if before >= 19 {
            eprintln!("skipping, the tests already run at the lowest priority");
            return Ok(());
        }

        let output = tokio_test::block_on(async {
            let mut command = tokio::process::Command::new("sh");
            command
                .arg("-c")
                .arg("nice")
                .backup_lane(FastLane::Free, &BTreeMap::new());

            command.output().await
        });

        let output = match output {
            Ok(output) if output.status.success() => output,
            Ok(output) => {
                eprintln!("skipping, nice failed: {}", String::from_utf8_lossy(&output.stderr));
                return Ok(());
            }
            Err(err) => {
                eprintln!("skipping, no sh to run nice with: {err}");
                return Ok(());
            }
        };

        let niced: i32 = String::from_utf8_lossy(&output.stdout).trim().parse()?;
        assert_eq!(niced, (before + YIELD_NICE).min(19));

        // the parent keeps its priority
        assert_eq!(rustix::process::getpriority_process(None)?, before);

        Ok(())
    }
}
