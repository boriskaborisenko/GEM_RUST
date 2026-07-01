//! PID/meta files + terminal-only lifecycle: `--stop`, `--status`.

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const PID_FILE: &str = "logs/gem_rust_server.pid";
pub const META_FILE: &str = "logs/gem_rust_server.json";
pub const SERVER_LOG: &str = "logs/gem_rust_server.log";
pub const CHILD_FLAG: &str = "--server-child";
pub const DEFAULT_BIND: &str = "127.0.0.1:8787";

pub fn pid_path() -> PathBuf {
    PathBuf::from(PID_FILE)
}

pub fn meta_path() -> PathBuf {
    PathBuf::from(META_FILE)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerMeta {
    pub pid: u32,
    pub bind: String,
    pub asset: String,
    pub interval: String,
    pub execution_mode: String,
    pub dry_run: bool,
    pub strategy: String,
    pub started_at_ms: i64,
    pub run_log_dir: String,
}

pub fn write_server_files(meta: &ServerMeta) -> anyhow::Result<()> {
    let pid_path = pid_path();
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if pid_path.exists() {
        if let Ok(existing) = std::fs::read_to_string(&pid_path) {
            if let Ok(old) = existing.trim().parse::<u32>() {
                if process_alive(old) && old != meta.pid {
                    bail!(
                        "server already running (pid {old}). \
                         Status: cargo run -- --server --status | \
                         Stop:   cargo run -- --server --stop"
                    );
                }
            }
        }
    }
    std::fs::write(&pid_path, format!("{}\n", meta.pid))?;
    let json = serde_json::to_string_pretty(meta)?;
    std::fs::write(meta_path(), json)?;
    Ok(())
}

pub fn cleanup_server_files() {
    let _ = std::fs::remove_file(pid_path());
    let _ = std::fs::remove_file(meta_path());
}

pub fn read_meta() -> Option<ServerMeta> {
    let raw = std::fs::read_to_string(meta_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn read_pid() -> Option<u32> {
    let raw = std::fs::read_to_string(pid_path()).ok()?;
    raw.trim().parse().ok()
}

pub async fn print_server_status() -> anyhow::Result<()> {
    let meta = read_meta();
    let pid = read_pid();

    let Some(pid) = pid else {
        println!("STATUS: not running (no pid file)");
        return Ok(());
    };

    let alive = process_alive(pid);
    if !alive {
        cleanup_server_files();
        println!("STATUS: not running (stale pid {pid} — cleaned up)");
        return Ok(());
    }

    println!("STATUS: RUNNING");
    println!("  pid:     {pid}");

    if let Some(m) = &meta {
        println!("  asset:   {} {}", m.asset, m.interval);
        println!("  mode:    {} dry_run={}", m.execution_mode, m.dry_run);
        println!("  strategy: {}", m.strategy);
        println!("  bind:    http://{}", m.bind);
        println!("  started: {}", format_ts(m.started_at_ms));
        println!("  logs:    {}", m.run_log_dir);
        if let Ok(health) = fetch_health(&m.bind).await {
            println!("  uptime:  {} ms", health.uptime_ms);
            if health.shutdown_pending {
                println!("  note:    SHUTDOWN PENDING (finishing open windows)");
            }
            if let Some(updated) = health.updated_at_ms {
                println!("  snapshot: {}", format_ts(updated));
            }
        } else {
            println!("  http:    dashboard not responding on {}", m.bind);
        }
    } else {
        println!("  (no meta file — pid {pid} is alive)");
    }

    Ok(())
}

#[derive(Deserialize)]
struct HealthResponse {
    uptime_ms: i64,
    shutdown_pending: bool,
    updated_at_ms: Option<i64>,
}

async fn fetch_health(bind: &str) -> anyhow::Result<HealthResponse> {
    let url = format!("http://{bind}/api/health");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    let resp = client.get(&url).send().await?.error_for_status()?;
    Ok(resp.json().await?)
}

pub fn stop_server(force: bool) -> anyhow::Result<()> {
    let pid = read_pid().context("no server running (missing logs/gem_rust_server.pid)")?;

    if !process_alive(pid) {
        cleanup_server_files();
        bail!("not running (stale pid {pid} — cleaned up)");
    }

    if force {
        eprintln!("[STOP] --force → SIGKILL pid {pid}");
        send_kill(pid)?;
        cleanup_server_files();
        println!("[STOP] Server killed.");
        return Ok(());
    }

    send_term(pid).context("failed to send SIGTERM")?;
    eprintln!("[STOP] SIGTERM → pid {pid} (grace up to 25s, then kill)");

    for i in 0..50 {
        if !process_alive(pid) {
            cleanup_server_files();
            println!("[STOP] Server stopped.");
            return Ok(());
        }
        if i > 0 && i % 4 == 0 {
            eprintln!("[STOP] still running… ({:.0}s)", i as f64 * 0.5);
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    eprintln!("[STOP] Graceful timeout — SIGKILL");
    send_kill(pid)?;
    cleanup_server_files();
    println!("[STOP] Server killed.");
    Ok(())
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn send_term(pid: u32) -> anyhow::Result<()> {
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .context("kill -TERM")?;
    if status.success() {
        Ok(())
    } else {
        bail!("kill -TERM failed");
    }
}

fn send_kill(pid: u32) -> anyhow::Result<()> {
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .context("kill -KILL")?;
    if status.success() {
        Ok(())
    } else {
        bail!("kill -KILL failed");
    }
}

fn format_ts(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| ms.to_string())
}

pub fn parse_server_bind(args: &[String]) -> String {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--server-bind" {
            if let Some(next) = args.get(i + 1) {
                return next.clone();
            }
        }
    }
    DEFAULT_BIND.to_string()
}

/// Parent: spawn detached child and exit once HTTP dashboard is up.
pub async fn launch_background_server(args: &[String], bind: &str) -> anyhow::Result<()> {
    if let Some(pid) = read_pid() {
        if process_alive(pid) {
            bail!(
                "server already running (pid {pid}). \
                 Status: cargo run -- --server --status | \
                 Stop:   cargo run -- --server --stop"
            );
        }
        cleanup_server_files();
    }

    let pid = spawn_server_child(args)?;
    eprintln!("[SERVER] starting background worker pid {pid}…");

    for _ in 0..180 {
        if !process_alive(pid) {
            bail!(
                "background worker exited during startup — see {}",
                SERVER_LOG
            );
        }
        if read_pid() == Some(pid) {
            if fetch_health(bind).await.is_ok() {
                print_started_banner(pid, bind);
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!(
        "timeout waiting for http://{bind} — worker pid {pid} may still be starting; see {}",
        SERVER_LOG
    );
}

fn print_started_banner(pid: u32, bind: &str) {
    let meta = read_meta();
    println!();
    println!("════════════════════════════════════════════════════════════");
    println!("  GEM bot running in BACKGROUND");
    println!("════════════════════════════════════════════════════════════");
    println!("  pid:       {pid}");
    if let Some(m) = &meta {
        println!("  asset:     {} {}", m.asset, m.interval);
        println!("  mode:      {} (dry_run={})", m.execution_mode, m.dry_run);
        println!("  run logs:  {}", m.run_log_dir);
    }
    println!("  dashboard: http://{bind}");
    println!("  stdout:    {SERVER_LOG}");
    println!();
    println!("  From laptop:  ssh -L 8787:127.0.0.1:8787 user@your-vps");
    println!("  Then open:    http://127.0.0.1:8787");
    println!();
    println!("  status:  cargo run -- --server --status");
    println!("  stop:    cargo run -- --server --stop");
    println!("  kill:    cargo run -- --server --stop --force");
    println!();
    println!("  Safe to disconnect SSH — bot keeps running.");
    println!("════════════════════════════════════════════════════════════");
}

fn spawn_server_child(args: &[String]) -> anyhow::Result<u32> {
    use std::fs::OpenOptions;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("resolve current executable")?;
    if let Some(parent) = Path::new(SERVER_LOG).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(SERVER_LOG)
        .with_context(|| format!("open {SERVER_LOG}"))?;

    let mut cmd = Command::new(exe);
    let mut has_child = false;
    for arg in args.iter().skip(1) {
        if arg == CHILD_FLAG {
            has_child = true;
        }
        cmd.arg(arg);
    }
    if !has_child {
        cmd.arg(CHILD_FLAG);
    }

    cmd.stdin(Stdio::null());
    let log_err = log.try_clone().context("dup server log fd")?;
    cmd.stdout(Stdio::from(log));
    cmd.stderr(Stdio::from(log_err));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let child = cmd.spawn().context("spawn background server child")?;
    Ok(child.id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bind_is_localhost() {
        assert!(DEFAULT_BIND.starts_with("127.0.0.1"));
    }
}
