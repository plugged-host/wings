//! plugged.host: what runs on the node, per server container and on the host, for the panel's abuse
//! protection (`GET /api/system/processes`, `GET /api/servers/{server}/processes`) and its node
//! telemetry (`GET /api/system/load`).
//!
//! Every call reads `/proc`: each process's name, executable, command line, CPU use and container
//! (a server's container is mapped back to the server), each container's outgoing connections and
//! packet rate, and the host's own processes. Executables using real CPU, or hiding their binary, are
//! checked once for strings a renamed miner still carries (XMRig, stratum+tcp, RandomX).
//!
//! It only collects. Every rule lives in the panel, so a new signature needs no Wings update.
//! CPU use is measured since the previous call; without a recent one, a baseline is taken first and
//! the call waits [`BASELINE_SPAN`].

use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::IpAddr,
    sync::LazyLock,
    time::{Duration, Instant},
};
use utoipa::ToSchema;

/// Processes listed per container, busiest first.
const MAX_CONTAINER_PROCESSES: usize = 40;
/// Host processes listed, busiest first.
const MAX_HOST_PROCESSES: usize = 400;
/// Bytes of a command line kept.
const MAX_COMMAND: usize = 1024;
/// Listening ports listed per container.
const MAX_LISTEN: usize = 20;
/// Busiest remote ports listed per container, besides [`WATCHED_PORTS`].
const MAX_REMOTE_PORTS: usize = 25;

/// An executable is fingerprinted once it uses this much CPU (percent of one thread), or when it
/// hides its binary, so idle game servers cost nothing.
const FINGERPRINT_MIN_CPU: f32 = 15.0;
const FINGERPRINT_MAX_BYTES: usize = 64 * 1024 * 1024;
const FINGERPRINT_CACHE: usize = 5000;

/// Remote ports always reported when a container connects to them, whatever else is busier: mail,
/// remote shells and the usual mining pool ports.
const WATCHED_PORTS: &[u16] = &[
    22, 23, 25, 465, 587, 2323, 3389, 3333, 4444, 5555, 6666, 7777, 9999, 10128, 10343, 13333,
    14433, 14444, 17777, 20128, 45560, 45700,
];

/// A previous sample older than this is no base for CPU use.
const MAX_BASELINE_AGE: Duration = Duration::from_secs(300);
/// How long a call without a usable previous sample measures CPU use over.
const BASELINE_SPAN: Duration = Duration::from_secs(2);
/// The shortest span CPU use is measured over, when calls come close together.
const MIN_SPAN: Duration = Duration::from_millis(500);

/// `USER_HZ`: the unit of `/proc/<pid>/stat` CPU times, 100 on every architecture Wings builds for.
const TICKS_PER_SECOND: f64 = 100.0;

#[derive(ToSchema, Serialize, Clone, Debug, PartialEq)]
pub struct Load {
    /// The 1, 5 and 15 minute load averages.
    pub load: [f64; 3],
    pub cpus: usize,
    /// All threads together, 0 to 100, since the previous sample.
    pub cpu_percent: f32,
}

#[derive(ToSchema, Serialize, Clone, Debug)]
pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub exe: String,
    pub cmd: String,
    /// Percent of one thread since the previous sample.
    pub cpu: f32,
    /// The executable was deleted after it started.
    pub deleted: bool,
    /// Miner strings found in the executable (`xmrig`, `stratum`, `donate-level`, `randomx`,
    /// `cryptonight`); only filled for processes that are busy or hide their binary.
    pub tags: Vec<String>,
}

#[derive(ToSchema, Serialize, Clone, Debug, Default)]
pub struct ContainerNet {
    pub listen: Vec<u16>,
    /// Remote addresses with an established outgoing connection.
    pub established_ips: usize,
    /// Remote addresses still being connected to (SYN sent): a scan looks like many of these.
    pub syn_sent_ips: usize,
    /// Remote port → how many remote addresses the container talks to on it.
    pub ports: BTreeMap<u16, usize>,
    /// Packets and bits per second sent since the previous sample.
    pub tx_pps: Option<u64>,
    pub tx_bps: Option<u64>,
}

#[derive(ToSchema, Serialize, Clone, Debug)]
pub struct Container {
    /// The first 12 characters of the container id.
    pub id: String,
    /// The server the container belongs to; `None` for containers that are not a running server
    /// (an installer, or something Wings did not start).
    pub server: Option<uuid::Uuid>,
    pub processes: Vec<Process>,
    pub net: ContainerNet,
}

#[derive(ToSchema, Serialize, Clone, Debug)]
pub struct Report {
    pub host: Load,
    pub containers: Vec<Container>,
    pub host_processes: Vec<Process>,
    /// Seconds CPU use was measured over.
    pub span: f64,
}

struct RawProcess {
    pid: i32,
    ppid: i32,
    comm: String,
    exe: String,
    cmd: String,
    ticks: u64,
    start: u64,
    container: Option<String>,
}

#[derive(Default)]
struct Sampler {
    /// (pid, start time) → CPU ticks at the previous sample.
    previous: HashMap<(i32, u64), u64>,
    previous_at: Option<Instant>,
    /// (idle, total) jiffies of `/proc/stat` at the previous sample.
    host_previous: Option<(u64, u64)>,
    /// container id → (when, tx packets, tx bytes).
    net_previous: HashMap<String, (Instant, u64, u64)>,
    /// (device, inode, size, mtime) of an executable → its tags.
    fingerprints: HashMap<(u64, u64, u64, i64), Vec<String>>,
}

static SAMPLER: LazyLock<tokio::sync::Mutex<Sampler>> =
    LazyLock::new(|| tokio::sync::Mutex::new(Sampler::default()));

/// The node's load, cheap enough to call every minute per node.
pub async fn load() -> Result<Load, anyhow::Error> {
    ensure_supported()?;

    let mut sampler = SAMPLER.lock().await;
    if sampler.host_previous.is_none() {
        sampler.host_previous = read_host_cpu();
        tokio::time::sleep(MIN_SPAN).await;
    }

    let current = read_host_cpu();
    let cpu_percent = host_cpu_percent(sampler.host_previous, current);
    sampler.host_previous = current;

    Ok(Load {
        load: read_loadavg()?,
        cpus: std::thread::available_parallelism().map_or(1, |cpus| cpus.get()),
        cpu_percent,
    })
}

/// Everything that runs on the node. `servers` maps container ids to the servers they run.
pub async fn report(servers: HashMap<String, uuid::Uuid>) -> Result<Report, anyhow::Error> {
    ensure_supported()?;

    let mut sampler = SAMPLER.lock().await;

    let stale = sampler
        .previous_at
        .is_none_or(|at| at.elapsed() > MAX_BASELINE_AGE);
    if stale {
        let (processes, host) =
            tokio::task::spawn_blocking(|| (read_processes(), read_host_cpu())).await?;
        sampler.previous = processes
            .iter()
            .map(|p| ((p.pid, p.start), p.ticks))
            .collect();
        sampler.previous_at = Some(Instant::now());
        sampler.host_previous = host;
        tokio::time::sleep(BASELINE_SPAN).await;
    } else if let Some(at) = sampler.previous_at
        && at.elapsed() < MIN_SPAN
    {
        tokio::time::sleep(MIN_SPAN.saturating_sub(at.elapsed())).await;
    }

    let mut taken = std::mem::take(&mut *sampler);
    let (report, taken) = tokio::task::spawn_blocking(move || {
        let report = collect(&mut taken, &servers);
        (report, taken)
    })
    .await?;
    *sampler = taken;

    Ok(report)
}

/// Running container id → the server it runs, for [`report`].
pub async fn container_servers(state: &crate::routes::AppState) -> HashMap<String, uuid::Uuid> {
    let servers: Vec<crate::server::Server> = state
        .server_manager
        .get_servers()
        .await
        .iter()
        .cloned()
        .collect();

    state
        .executor
        .container_refs(&servers)
        .await
        .into_iter()
        .filter(|(_, container)| container.len() == 64)
        .map(|(server, container)| (container, server))
        .collect()
}

#[cfg(target_os = "linux")]
fn ensure_supported() -> Result<(), anyhow::Error> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn ensure_supported() -> Result<(), anyhow::Error> {
    Err(anyhow::anyhow!("process reports need Linux"))
}

fn collect(sampler: &mut Sampler, servers: &HashMap<String, uuid::Uuid>) -> Report {
    let processes = read_processes();
    let now = Instant::now();
    let span = sampler
        .previous_at
        .map(|at| now.duration_since(at).as_secs_f64())
        .filter(|span| *span > 0.0);

    let own = std::process::id() as i32;
    let mut current = HashMap::with_capacity(processes.len());
    let mut containers: BTreeMap<String, Vec<(Process, i32)>> = BTreeMap::new();
    let mut host = Vec::new();

    for raw in processes {
        current.insert((raw.pid, raw.start), raw.ticks);
        if raw.pid == own {
            continue;
        }

        let cpu = match (span, sampler.previous.get(&(raw.pid, raw.start))) {
            (Some(span), Some(before)) if raw.ticks >= *before => {
                round1(((raw.ticks - before) as f64 / TICKS_PER_SECOND / span * 100.0) as f32)
            }
            _ => 0.0,
        };
        let deleted = raw.exe.ends_with(" (deleted)");
        let tags = if cpu >= FINGERPRINT_MIN_CPU || hides_binary(&raw.exe) {
            fingerprint(&mut sampler.fingerprints, raw.pid)
        } else {
            Vec::new()
        };

        let process = Process {
            pid: raw.pid,
            ppid: raw.ppid,
            comm: raw.comm,
            exe: raw.exe,
            cmd: raw.cmd,
            cpu,
            deleted,
            tags,
        };
        match raw.container {
            Some(container) => containers
                .entry(container)
                .or_default()
                .push((process, raw.pid)),
            None => host.push(process),
        }
    }

    let running: HashSet<String> = containers.keys().cloned().collect();
    let mut report_containers = Vec::with_capacity(containers.len());
    for (container, mut entries) in containers {
        let first_pid = entries
            .iter()
            .map(|(_, pid)| *pid)
            .min()
            .unwrap_or_default();
        let net = container_net(sampler, &container, first_pid, now);

        entries.sort_by(|(a, _), (b, _)| b.cpu.total_cmp(&a.cpu));
        report_containers.push(Container {
            id: container.chars().take(12).collect(),
            server: servers.get(&container).copied(),
            processes: entries
                .into_iter()
                .take(MAX_CONTAINER_PROCESSES)
                .map(|(process, _)| process)
                .collect(),
            net,
        });
    }

    sampler
        .net_previous
        .retain(|container, _| running.contains(container));

    host.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
    host.truncate(MAX_HOST_PROCESSES);

    let host_cpu = read_host_cpu();
    let load = Load {
        load: read_loadavg().unwrap_or_default(),
        cpus: std::thread::available_parallelism().map_or(1, |cpus| cpus.get()),
        cpu_percent: host_cpu_percent(sampler.host_previous, host_cpu),
    };

    sampler.previous = current;
    sampler.previous_at = Some(now);
    sampler.host_previous = host_cpu;

    Report {
        host: load,
        containers: report_containers,
        host_processes: host,
        span: round1(span.unwrap_or_default() as f32) as f64,
    }
}

fn round1(value: f32) -> f32 {
    (value * 10.0).round() / 10.0
}

/// A binary that is deleted, in memory only, or in a temporary directory.
fn hides_binary(exe: &str) -> bool {
    exe.ends_with(" (deleted)")
        || exe.starts_with("/memfd:")
        || ["/tmp/", "/var/tmp/", "/dev/shm/", "/run/shm/"]
            .iter()
            .any(|dir| exe.starts_with(dir))
}

fn read_processes() -> Vec<RawProcess> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };

    let mut processes = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let base = entry.path();

        // Gone, or a kernel thread (no executable).
        let Ok(stat) = std::fs::read(base.join("stat")) else {
            continue;
        };
        let Ok(exe) = std::fs::read_link(base.join("exe")) else {
            continue;
        };
        let Some(stat) = parse_stat(&String::from_utf8_lossy(&stat)) else {
            continue;
        };

        let cmd = std::fs::read(base.join("cmdline"))
            .map(|cmd| command_line(&cmd))
            .unwrap_or_default();
        let container = std::fs::read_to_string(base.join("cgroup"))
            .ok()
            .and_then(|cgroup| container_id(&cgroup).map(str::to_string));

        processes.push(RawProcess {
            pid,
            ppid: stat.ppid,
            comm: stat.comm,
            exe: exe.to_string_lossy().into_owned(),
            cmd,
            ticks: stat.ticks,
            start: stat.start,
            container,
        });
    }

    processes
}

#[derive(Debug, PartialEq)]
struct Stat {
    comm: String,
    ppid: i32,
    ticks: u64,
    start: u64,
}

/// `/proc/<pid>/stat`: the name sits in parentheses and may contain spaces and parentheses itself.
fn parse_stat(stat: &str) -> Option<Stat> {
    let left = stat.find('(')?;
    let right = stat.rfind(')')?;
    let fields: Vec<&str> = stat.get(right + 2..)?.split_whitespace().collect();
    let field = |index: usize| fields.get(index).copied();

    Some(Stat {
        comm: stat.get(left + 1..right)?.to_string(),
        ppid: field(1)?.parse().ok()?,
        ticks: field(11)?.parse::<u64>().ok()? + field(12)?.parse::<u64>().ok()?,
        start: field(19)?.parse().ok()?,
    })
}

fn command_line(raw: &[u8]) -> String {
    let raw = raw.get(..MAX_COMMAND * 2).unwrap_or(raw);

    String::from_utf8_lossy(raw)
        .replace('\0', " ")
        .trim()
        .chars()
        .take(MAX_COMMAND)
        .collect()
}

/// The 64-character Docker container id in a process's `/proc/<pid>/cgroup`, if it runs in one.
fn container_id(cgroup: &str) -> Option<&str> {
    let bytes = cgroup.as_bytes();
    let mut run = 0;

    for (index, byte) in bytes.iter().enumerate() {
        if byte.is_ascii_digit() || (b'a'..=b'f').contains(byte) {
            run += 1;
            let next_is_hex = bytes
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_digit() || (b'a'..=b'f').contains(next));
            if run == 64 && !next_is_hex {
                return cgroup.get(index + 1 - 64..=index);
            }
        } else {
            run = 0;
        }
    }

    None
}

static FINGERPRINT: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(r"(?i)xmrig|stratum\+(?:tcp|ssl)|donate-level|randomx|cryptonight")
        .expect("valid fingerprint pattern")
});

fn fingerprint(cache: &mut HashMap<(u64, u64, u64, i64), Vec<String>>, pid: i32) -> Vec<String> {
    use std::io::Read;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    let path = format!("/proc/{pid}/exe");
    let Ok(metadata) = std::fs::metadata(&path) else {
        return Vec::new();
    };
    #[cfg(unix)]
    let key = (
        metadata.dev(),
        metadata.ino(),
        metadata.size(),
        metadata.mtime(),
    );
    #[cfg(not(unix))]
    let key = (0, 0, metadata.len(), 0);

    if let Some(tags) = cache.get(&key) {
        return tags.clone();
    }

    let mut tags = std::collections::BTreeSet::new();
    if let Ok(mut file) = std::fs::File::open(&path) {
        let mut chunk = vec![0; 1 << 20];
        let mut window: Vec<u8> = Vec::with_capacity(chunk.len() + 32);
        let mut done = 0;

        while done < FINGERPRINT_MAX_BYTES {
            let read = match file.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            done += read;

            // The last 32 bytes of the previous chunk, so a string across two chunks is found.
            let carried = window.len().saturating_sub(32);
            window.drain(..carried);
            window.extend_from_slice(chunk.get(..read).unwrap_or_default());

            for found in FINGERPRINT.find_iter(&window) {
                tags.insert(fingerprint_tag(found.as_bytes()));
            }
        }
    }

    if cache.len() >= FINGERPRINT_CACHE {
        cache.clear();
    }
    let tags: Vec<String> = tags.into_iter().collect();
    cache.insert(key, tags.clone());

    tags
}

fn fingerprint_tag(found: &[u8]) -> String {
    let found = String::from_utf8_lossy(found).to_ascii_lowercase();
    if found.starts_with("stratum") {
        "stratum".to_string()
    } else {
        found
    }
}

/// A `/proc/net/tcp{,6}` address (`0100007F:1F90`): the address is printed as host-order 32-bit
/// words.
fn parse_address(text: &str) -> Option<(IpAddr, u16)> {
    let (address, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;

    let mut bytes = Vec::with_capacity(16);
    for word in address.as_bytes().chunks(8) {
        let word = u32::from_str_radix(std::str::from_utf8(word).ok()?, 16).ok()?;
        bytes.extend_from_slice(&word.to_le_bytes());
    }

    let ip = match bytes.len() {
        4 => IpAddr::from(<[u8; 4]>::try_from(bytes).ok()?),
        16 => {
            let ip = std::net::Ipv6Addr::from(<[u8; 16]>::try_from(bytes).ok()?);
            match ip.to_ipv4_mapped() {
                Some(ip) => IpAddr::V4(ip),
                None => IpAddr::V6(ip),
            }
        }
        _ => return None,
    };

    Some((ip, port))
}

/// One line of `/proc/<pid>/net/tcp{,6}`: (state, local port, remote address, remote port).
fn parse_socket(line: &str) -> Option<(&str, u16, IpAddr, u16)> {
    let mut parts = line.split_whitespace().skip(1);
    let (_, local_port) = parse_address(parts.next()?)?;
    let (remote, remote_port) = parse_address(parts.next()?)?;

    Some((parts.next()?, local_port, remote, remote_port))
}

/// The container's connections out (not to its own listening ports, not to itself) and the busiest
/// and watched remote ports.
fn summarize_sockets(lines: &[String]) -> ContainerNet {
    let sockets: Vec<_> = lines.iter().filter_map(|line| parse_socket(line)).collect();

    let listening: HashSet<u16> = sockets
        .iter()
        .filter(|(state, ..)| *state == "0A")
        .map(|(_, port, ..)| *port)
        .collect();

    let mut established = HashSet::new();
    let mut connecting = HashSet::new();
    let mut ports: HashMap<u16, HashSet<IpAddr>> = HashMap::new();
    for (state, local_port, remote, remote_port) in &sockets {
        if !matches!(*state, "01" | "02") || listening.contains(local_port) || remote.is_loopback()
        {
            continue;
        }

        if *state == "02" {
            connecting.insert(*remote);
        } else {
            established.insert(*remote);
        }
        ports.entry(*remote_port).or_default().insert(*remote);
    }

    let mut busiest: Vec<(u16, usize)> =
        ports.iter().map(|(port, ips)| (*port, ips.len())).collect();
    busiest.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let busiest: HashSet<u16> = busiest
        .into_iter()
        .take(MAX_REMOTE_PORTS)
        .map(|(port, _)| port)
        .collect();

    let mut listen: Vec<u16> = listening.into_iter().collect();
    listen.sort_unstable();
    listen.truncate(MAX_LISTEN);

    ContainerNet {
        listen,
        established_ips: established.len(),
        syn_sent_ips: connecting.len(),
        ports: ports
            .into_iter()
            .filter(|(port, _)| WATCHED_PORTS.contains(port) || busiest.contains(port))
            .map(|(port, ips)| (port, ips.len()))
            .collect(),
        tx_pps: None,
        tx_bps: None,
    }
}

/// Packets and bytes sent from `/proc/<pid>/net/dev`, without the loopback interface.
fn parse_net_dev(dev: &str) -> (u64, u64) {
    let mut packets = 0;
    let mut bytes = 0;

    for line in dev.lines().skip(2) {
        let Some((name, data)) = line.split_once(':') else {
            continue;
        };
        if name.trim() == "lo" {
            continue;
        }

        let mut transmit = data.split_whitespace().skip(8);
        let (Some(sent_bytes), Some(sent_packets)) = (transmit.next(), transmit.next()) else {
            continue;
        };
        bytes += sent_bytes.parse::<u64>().unwrap_or_default();
        packets += sent_packets.parse::<u64>().unwrap_or_default();
    }

    (packets, bytes)
}

fn container_net(sampler: &mut Sampler, container: &str, pid: i32, now: Instant) -> ContainerNet {
    let mut lines = Vec::new();
    for table in ["tcp", "tcp6"] {
        if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/net/{table}")) {
            lines.extend(text.lines().skip(1).map(str::to_string));
        }
    }
    let mut net = summarize_sockets(&lines);

    let (packets, bytes) = std::fs::read_to_string(format!("/proc/{pid}/net/dev"))
        .map(|dev| parse_net_dev(&dev))
        .unwrap_or_default();
    if let Some((at, previous_packets, previous_bytes)) = sampler.net_previous.get(container)
        && now > *at
        && packets >= *previous_packets
        && bytes >= *previous_bytes
    {
        let elapsed = now.duration_since(*at).as_secs_f64();
        net.tx_pps = Some(((packets - previous_packets) as f64 / elapsed).round() as u64);
        net.tx_bps = Some(((bytes - previous_bytes) as f64 * 8.0 / elapsed).round() as u64);
    }
    sampler
        .net_previous
        .insert(container.to_string(), (now, packets, bytes));

    net
}

/// (idle, total) jiffies of the first line of `/proc/stat`.
fn read_host_cpu() -> Option<(u64, u64)> {
    parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
}

fn parse_proc_stat(stat: &str) -> Option<(u64, u64)> {
    let values: Vec<u64> = stat
        .lines()
        .next()?
        .split_whitespace()
        .skip(1)
        .map(|value| value.parse().ok())
        .collect::<Option<_>>()?;
    let idle = values.get(3)? + values.get(4).unwrap_or(&0);

    Some((idle, values.iter().sum()))
}

fn host_cpu_percent(previous: Option<(u64, u64)>, current: Option<(u64, u64)>) -> f32 {
    match (previous, current) {
        (Some((idle_before, total_before)), Some((idle, total)))
            if total > total_before && idle >= idle_before =>
        {
            let busy = 1.0 - (idle - idle_before) as f64 / (total - total_before) as f64;
            round1((busy * 100.0).clamp(0.0, 100.0) as f32)
        }
        _ => 0.0,
    }
}

fn read_loadavg() -> Result<[f64; 3], anyhow::Error> {
    parse_loadavg(&std::fs::read_to_string("/proc/loadavg")?)
        .ok_or_else(|| anyhow::anyhow!("unreadable /proc/loadavg"))
}

fn parse_loadavg(text: &str) -> Option<[f64; 3]> {
    let mut values = text
        .split_whitespace()
        .map(|value| value.parse::<f64>().ok());

    Some([values.next()??, values.next()??, values.next()??])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_names_may_contain_spaces_and_parentheses() {
        let stat = "4242 (my (odd) proc) S 1 4242 4242 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 3 0 98765 1000 10 18446744073709551615";
        let parsed = parse_stat(stat).expect("parses");

        assert_eq!(parsed.comm, "my (odd) proc");
        assert_eq!(parsed.ppid, 1);
        assert_eq!(parsed.ticks, 300);
        assert_eq!(parsed.start, 98765);
        assert_eq!(parse_stat("4242 (short) S 1 2"), None);
    }

    #[test]
    fn container_ids_come_from_v1_and_v2_cgroups() {
        let id = "a".repeat(32) + &"0123456789abcdef".repeat(2);
        assert_eq!(id.len(), 64);

        let v2 = format!("0::/system.slice/docker-{id}.scope\n");
        let v1 = format!("12:cpu,cpuacct:/docker/{id}\n11:memory:/docker/{id}\n");

        assert_eq!(container_id(&v2), Some(id.as_str()));
        assert_eq!(container_id(&v1), Some(id.as_str()));
        assert_eq!(
            container_id("0::/user.slice/user-0.slice/session-1.scope\n"),
            None
        );
        assert_eq!(
            container_id(&format!("0::/{}f\n", id)),
            None,
            "65 hex characters are no id"
        );
    }

    #[test]
    fn hidden_binaries() {
        assert!(hides_binary("/tmp/.x/kworker"));
        assert!(hides_binary("/dev/shm/a"));
        assert!(hides_binary("/memfd: (deleted)"));
        assert!(hides_binary("/home/container/bin (deleted)"));
        assert!(!hides_binary("/usr/bin/java"));
    }

    #[test]
    fn fingerprint_tags_are_lowercase_and_stratum_is_one_tag() {
        let found: Vec<String> = FINGERPRINT
            .find_iter(b"XMRig 6.21 --donate-level=1 stratum+ssl://pool:443 RandomX")
            .map(|found| fingerprint_tag(found.as_bytes()))
            .collect();

        assert_eq!(found, ["xmrig", "donate-level", "stratum", "randomx"]);
    }

    #[test]
    fn proc_net_addresses() {
        assert_eq!(
            parse_address("0100007F:1F90"),
            Some(("127.0.0.1".parse().unwrap(), 8080))
        );
        assert_eq!(
            parse_address("0000000000000000FFFF00000A01A8C0:0050"),
            Some(("192.168.1.10".parse().unwrap(), 80))
        );
        assert_eq!(
            parse_address("B80D0120000000000000000001000000:01BB"),
            Some(("2001:db8::1".parse().unwrap(), 443))
        );
        assert_eq!(parse_address("nonsense"), None);
    }

    #[test]
    fn sockets_count_outgoing_connections_only() {
        // 0.0.0.0:25565 listening; one player connected in on it; two connections out to port
        // 3333 (one still connecting); one to localhost.
        let lines: Vec<String> = [
            "0: 00000000:63DD 00000000:0000 0A 00000000:00000000 00:00000000 00000000 0 0 1",
            "1: 0200000A:63DD 0B00A8C0:C350 01 00000000:00000000 00:00000000 00000000 0 0 2",
            "2: 0200000A:9C40 0A0B0C0D:0D05 01 00000000:00000000 00:00000000 00000000 0 0 3",
            "3: 0200000A:9C41 0E0F1011:0D05 02 00000000:00000000 00:00000000 00000000 0 0 4",
            "4: 0100007F:9C42 0100007F:1F90 01 00000000:00000000 00:00000000 00000000 0 0 5",
        ]
        .map(str::to_string)
        .to_vec();

        let net = summarize_sockets(&lines);

        assert_eq!(net.listen, [25565]);
        assert_eq!(net.established_ips, 1);
        assert_eq!(net.syn_sent_ips, 1);
        assert_eq!(net.ports, BTreeMap::from([(3333, 2)]));
    }

    #[test]
    fn net_dev_skips_loopback() {
        let dev = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo: 100 1 0 0 0 0 0 0 100 1 0 0 0 0 0 0\n  eth0: 5000 50 0 0 0 0 0 0 7000 70 0 0 0 0 0 0\n";

        assert_eq!(parse_net_dev(dev), (70, 7000));
    }

    #[test]
    fn host_cpu_and_load() {
        let before = parse_proc_stat("cpu  100 0 100 700 100 0 0 0 0 0\ncpu0 1 2 3").unwrap();
        let after = parse_proc_stat("cpu  150 0 150 750 150 0 0 0 0 0\n").unwrap();

        assert_eq!(before, (800, 1000));
        assert_eq!(host_cpu_percent(Some(before), Some(after)), 50.0);
        assert_eq!(host_cpu_percent(None, Some(after)), 0.0);
        assert_eq!(
            parse_loadavg("0.52 0.58 0.59 2/1324 98765\n"),
            Some([0.52, 0.58, 0.59])
        );
    }

    #[test]
    fn command_lines_are_spaced_and_capped() {
        assert_eq!(
            command_line(b"java\0-Xmx2G\0-jar\0server.jar\0"),
            "java -Xmx2G -jar server.jar"
        );
        assert_eq!(command_line(&vec![b'a'; 5000]).len(), MAX_COMMAND);
    }
}
