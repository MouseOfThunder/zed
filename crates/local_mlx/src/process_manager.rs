use anyhow::{Context, Result, anyhow};
use gpui::{BackgroundExecutor, Task};
use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command as StdCommand, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex as ParkingMutex;

pub struct ProcessManager {
    inner: Arc<ParkingMutex<Inner>>,
}

struct Inner {
    state: ProcessState,
    child: Option<Child>,
    port: u16,
    last_used: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessState {
    Stopped,
    Starting,
    Running,
    Stopping,
}

impl ProcessManager {
    pub fn new(port: u16) -> Self {
        Self {
            inner: Arc::new(ParkingMutex::new(Inner {
                state: ProcessState::Stopped,
                child: None,
                port,
                last_used: Instant::now(),
            })),
        }
    }

    pub fn port(&self) -> u16 {
        self.inner.lock().port
    }

    pub fn is_running(&self) -> bool {
        let mut inner = self.inner.lock();
        if inner.state != ProcessState::Running {
            return false;
        }
        if let Some(ref mut child) = inner.child {
            child.try_wait().map(|s| s.is_none()).unwrap_or(false)
        } else {
            false
        }
    }

    pub fn is_starting(&self) -> bool {
        self.inner.lock().state == ProcessState::Starting
    }

    pub fn touch(&self) {
        self.inner.lock().last_used = Instant::now();
    }

    /// Start a single mlx_lm.server process on the configured port.
    /// The server handles model discovery and loading internally —
    /// no model argument needed. Just start it once and use it for
    /// all local MLX requests.
    pub fn start(
        self: &Arc<Self>,
        server_binary: &str,
        server_args: &[String],
        model: Option<&str>,
        executor: &BackgroundExecutor,
    ) -> Task<Result<u16>> {
        let this = self.clone();
        let server_binary = resolve_binary(server_binary);
        let server_args = server_args.to_vec();
        let port = self.port();
        let model = model.map(|m| m.to_string());

        executor.spawn(async move {
            let args: Vec<String> = server_args
                .iter()
                .map(|arg| {
                    arg.replace("{port}", &port.to_string())
                        .replace("{model}", model.as_deref().unwrap_or(""))
                })
                .collect();

            log::info!(
                "Starting local MLX server on port {}: {} {}",
                port,
                server_binary,
                args.join(" ")
            );

            let mut cmd = StdCommand::new(&server_binary);
            cmd.args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let child = cmd
                .spawn()
                .with_context(|| format!("failed to spawn {} {}", server_binary, args.join(" ")))?;

            {
                let mut inner = this.inner.lock();
                inner.state = ProcessState::Starting;
                inner.child = Some(child);
                inner.last_used = Instant::now();
            }

            // Health check: TCP connect with backoff
            let max_attempts = 15u32;
            let base_delay_ms = 150u64;

            for attempt in 0..max_attempts {
                {
                    let mut inner = this.inner.lock();
                    if inner.state != ProcessState::Starting {
                        return Err(anyhow!("Server start was cancelled"));
                    }
                    if let Some(ref mut child) = inner.child {
                        if child.try_wait()?.is_some() {
                            let stderr_output = read_stderr(&mut inner);
                            inner.state = ProcessState::Stopped;
                            inner.child = None;
                            let mut msg =
                                "Server process exited unexpectedly during startup".to_string();
                            if !stderr_output.is_empty() {
                                msg.push_str(": ");
                                msg.push_str(&stderr_output);
                            }
                            return Err(anyhow!("{}", msg));
                        }
                    }
                }

                let addr = format!("127.0.0.1:{}", port);
                match TcpStream::connect_timeout(
                    &addr
                        .parse()
                        .map_err(|e| anyhow!("Invalid address: {}", e))?,
                    Duration::from_secs(2),
                ) {
                    Ok(_) => {
                        std::thread::sleep(Duration::from_millis(500));
                        {
                            let mut inner = this.inner.lock();
                            if let Some(ref mut child) = inner.child {
                                if child.try_wait()?.is_some() {
                                    let stderr_output = read_stderr(&mut inner);
                                    inner.state = ProcessState::Stopped;
                                    inner.child = None;
                                    let mut msg =
                                        "Server process exited during startup".to_string();
                                    if !stderr_output.is_empty() {
                                        msg.push_str(": ");
                                        msg.push_str(&stderr_output);
                                    }
                                    return Err(anyhow!("{}", msg));
                                }
                            }
                        }
                        let mut inner = this.inner.lock();
                        inner.state = ProcessState::Running;
                        log::info!("Local MLX server ready on port {}", port,);
                        return Ok(port);
                    }
                    Err(_) => {}
                }

                let delay_ms = if attempt < 5 {
                    base_delay_ms * 2u64.pow(attempt)
                } else {
                    2000u64
                };
                std::thread::sleep(Duration::from_millis(delay_ms));
            }

            let (stderr_output, child_to_kill) = {
                let mut inner = this.inner.lock();
                inner.state = ProcessState::Stopping;
                let stderr_output = read_stderr(&mut inner);
                let child_to_kill = inner.child.take();
                (stderr_output, child_to_kill)
            };

            if let Some(mut child) = child_to_kill {
                let _ = child.kill();
                let _ = child.wait();
            }

            {
                let mut inner = this.inner.lock();
                inner.state = ProcessState::Stopped;
            }

            let mut msg = format!(
                "Server did not become healthy after {} attempts",
                max_attempts
            );
            if !stderr_output.is_empty() {
                msg.push_str(": ");
                msg.push_str(&stderr_output);
            }
            Err(anyhow!("{}", msg))
        })
    }

    pub fn stop(self: &Arc<Self>, executor: &BackgroundExecutor) -> Task<Result<()>> {
        let this = self.clone();

        executor.spawn(async move {
            let child_to_kill = {
                let mut inner = this.inner.lock();
                inner.state = ProcessState::Stopping;
                inner.child.take()
            };

            if let Some(mut child) = child_to_kill {
                log::info!("Stopping local MLX server...");
                let _ = child.kill();

                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(5) {
                    if child.try_wait().map(|s| s.is_some()).unwrap_or(true) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }

            let mut inner = this.inner.lock();
            inner.state = ProcessState::Stopped;
            inner.last_used = Instant::now();
            log::info!("Local MLX server stopped");
            Ok(())
        })
    }

    pub fn spawn_idle_watcher(
        self: &Arc<Self>,
        idle_timeout: Duration,
        executor: &BackgroundExecutor,
    ) -> Task<()> {
        let this = self.clone();

        executor.spawn(async move {
            loop {
                std::thread::sleep(Duration::from_secs(30));

                let should_stop = {
                    let inner = this.inner.lock();
                    inner.state == ProcessState::Running && inner.last_used.elapsed() > idle_timeout
                };

                if should_stop {
                    log::info!("Local MLX server idle for {:?}, stopping...", idle_timeout);
                    let child_to_kill = {
                        let mut inner = this.inner.lock();
                        inner.state = ProcessState::Stopping;
                        inner.child.take()
                    };

                    if let Some(mut child) = child_to_kill {
                        let _ = child.kill();
                        std::thread::sleep(Duration::from_secs(5));
                        let _ = child.try_wait();
                    }

                    let mut inner = this.inner.lock();
                    inner.state = ProcessState::Stopped;
                    inner.last_used = Instant::now();
                    return;
                }

                if this.inner.lock().state != ProcessState::Running {
                    return;
                }
            }
        })
    }
}

impl Drop for ProcessManager {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
        if let Some(mut child) = inner.child.take() {
            let _ = child.kill();
        }
    }
}

fn read_stderr(inner: &mut Inner) -> String {
    inner
        .child
        .as_mut()
        .and_then(|c| c.stderr.take())
        .map(|mut stderr| {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            buf
        })
        .unwrap_or_default()
}

fn resolve_binary(name: &str) -> String {
    if name.contains('/') {
        return name.to_string();
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        format!("{}/.local/bin/{}", home, name),
        format!("{}/miniforge3/bin/{}", home, name),
        format!("{}/miniconda3/bin/{}", home, name),
        format!("{}/anaconda3/bin/{}", home, name),
        format!("/opt/homebrew/bin/{}", name),
        format!("/usr/local/bin/{}", name),
    ];

    for candidate in &candidates {
        let path = PathBuf::from(candidate);
        if path.exists() {
            log::info!("Resolved {} to {}", name, path.display());
            return path.to_string_lossy().to_string();
        }
    }

    log::info!("Using {} from PATH", name);
    name.to_string()
}
