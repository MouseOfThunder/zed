use anyhow::{Context, Result, anyhow};
use gpui::{BackgroundExecutor, Task};
use smol::process::Child;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use util::command::new_std_command;

use parking_lot::Mutex as ParkingMutex;

pub struct ProcessManager {
    inner: Arc<ParkingMutex<Inner>>,
}

struct Inner {
    state: ProcessState,
    child: Option<Child>,
    port: Option<u16>,
    current_model: Option<String>,
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
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ParkingMutex::new(Inner {
                state: ProcessState::Stopped,
                child: None,
                port: None,
                current_model: None,
                last_used: Instant::now(),
            })),
        }
    }

    pub fn port(&self) -> Option<u16> {
        let inner = self.inner.lock();
        if inner.state == ProcessState::Running {
            inner.port
        } else {
            None
        }
    }

    pub fn is_running(&self) -> bool {
        let mut inner = self.inner.lock();
        if inner.state != ProcessState::Running {
            return false;
        }
        if let Some(ref mut child) = inner.child {
            child.try_status().map(|s| s.is_none()).unwrap_or(false)
        } else {
            false
        }
    }

    pub fn is_starting(&self) -> bool {
        self.inner.lock().state == ProcessState::Starting
    }

    pub fn current_model(&self) -> Option<String> {
        self.inner.lock().current_model.clone()
    }

    pub fn touch(&self) {
        self.inner.lock().last_used = Instant::now();
    }

    pub fn start(
        self: &Arc<Self>,
        server_binary: &str,
        server_args: &[String],
        model_name: &str,
        executor: &BackgroundExecutor,
    ) -> Task<Result<u16>> {
        let this = self.clone();
        let server_binary = resolve_binary(server_binary);
        let server_args = server_args.to_vec();
        let model_name = model_name.to_string();

        executor.spawn(async move {
            let port = find_free_port()?;

            let args: Vec<String> = server_args
                .iter()
                .map(|arg| {
                    arg.replace("{port}", &port.to_string())
                        .replace("{model}", &model_name)
                })
                .collect();

            log::info!(
                "Starting local MLX server: {} {}",
                server_binary,
                args.join(" ")
            );

            let mut cmd = new_std_command(&server_binary);
            cmd.args(&args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());

            let child = smol::process::Command::from(cmd)
                .spawn()
                .with_context(|| format!("failed to spawn {} {}", server_binary, args.join(" ")))?;

            {
                let mut inner = this.inner.lock();
                inner.state = ProcessState::Starting;
                inner.child = Some(child);
                inner.port = Some(port);
                inner.current_model = Some(model_name.clone());
                inner.last_used = Instant::now();
            }

            let max_attempts = 30u32;
            let base_delay_ms = 100u64;

            for attempt in 0..max_attempts {
                {
                    let mut inner = this.inner.lock();
                    if inner.state != ProcessState::Starting {
                        return Err(anyhow!("Server start was cancelled"));
                    }
                    if let Some(ref mut child) = inner.child {
                        if child.try_status()?.is_some() {
                            inner.state = ProcessState::Stopped;
                            inner.child = None;
                            return Err(anyhow!(
                                "Server process exited unexpectedly during startup"
                            ));
                        }
                    }
                }

                match smol::net::TcpStream::connect(format!("127.0.0.1:{}", port)).await {
                    Ok(_) => {
                        let mut inner = this.inner.lock();
                        inner.state = ProcessState::Running;
                        log::info!(
                            "Local MLX server ready on port {} (model: {})",
                            port,
                            model_name
                        );
                        return Ok(port);
                    }
                    Err(_) => {}
                }

                let delay_ms = base_delay_ms * 2u64.pow(attempt.min(5));
                smol::Timer::after(Duration::from_millis(delay_ms)).await;
            }

            let mut inner = this.inner.lock();
            inner.state = ProcessState::Stopped;
            if let Some(mut child) = inner.child.take() {
                let _ = child.kill();
            }
            Err(anyhow!(
                "Server did not become healthy after {} attempts",
                max_attempts
            ))
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
                    if child.try_status().map(|s| s.is_some()).unwrap_or(true) {
                        break;
                    }
                    smol::Timer::after(Duration::from_millis(200)).await;
                }
            }

            let mut inner = this.inner.lock();
            inner.state = ProcessState::Stopped;
            inner.port = None;
            inner.current_model = None;
            inner.last_used = Instant::now();
            log::info!("Local MLX server stopped");
            Ok(())
        })
    }

    pub fn ensure_model(
        self: &Arc<Self>,
        server_binary: &str,
        server_args: &[String],
        model_name: &str,
        executor: &BackgroundExecutor,
    ) -> Task<Result<u16>> {
        let needs_restart = {
            let inner = self.inner.lock();
            inner.state == ProcessState::Running
                && inner.current_model.as_deref() != Some(model_name)
        };

        if needs_restart {
            let this = self.clone();
            let server_binary = server_binary.to_string();
            let server_args = server_args.to_vec();
            let model_name = model_name.to_string();
            let exec = executor.clone();

            executor.spawn(async move {
                let child_to_kill = {
                    let mut inner = this.inner.lock();
                    inner.state = ProcessState::Stopping;
                    inner.current_model = None;
                    inner.child.take()
                };

                if let Some(mut child) = child_to_kill {
                    log::info!("Restarting local MLX server for model change...");
                    let _ = child.kill();
                    smol::Timer::after(Duration::from_millis(500)).await;
                }

                {
                    let mut inner = this.inner.lock();
                    inner.state = ProcessState::Stopped;
                    inner.port = None;
                }

                this.start(&server_binary, &server_args, &model_name, &exec)
                    .await
            })
        } else if !self.is_running() && !self.is_starting() {
            self.start(server_binary, server_args, model_name, executor)
        } else if self.is_starting() {
            Task::ready(Err(anyhow!(
                "Local MLX server is still starting up. Please wait a moment and retry."
            )))
        } else {
            Task::ready(Ok(self.port().unwrap_or(0)))
        }
    }

    pub fn spawn_idle_watcher(
        self: &Arc<Self>,
        idle_timeout: Duration,
        executor: &BackgroundExecutor,
    ) -> Task<()> {
        let this = self.clone();

        executor.spawn(async move {
            loop {
                smol::Timer::after(Duration::from_secs(30)).await;

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
                        smol::Timer::after(Duration::from_secs(5)).await;
                        let _ = child.try_status();
                    }

                    let mut inner = this.inner.lock();
                    inner.state = ProcessState::Stopped;
                    inner.port = None;
                    inner.current_model = None;
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

fn resolve_binary(name: &str) -> String {
    // If it's an absolute or relative path, use it as-is
    if name.contains('/') {
        return name.to_string();
    }

    // Check common bin directories (macOS app bundle has limited PATH)
    let home = std::env::var("HOME").unwrap_or_default();
    let search_dirs = [
        format!("{}/.local/bin", home),
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
    ];

    for dir in &search_dirs {
        let path = PathBuf::from(dir).join(name);
        if path.exists() {
            log::info!("Resolved {} to {}", name, path.display());
            return path.to_string_lossy().to_string();
        }
    }

    // Fall back to the original name (hope it's in PATH)
    name.to_string()
}

fn find_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("failed to bind to find free port")?;
    Ok(listener.local_addr()?.port())
}
