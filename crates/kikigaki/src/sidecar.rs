use std::io::{BufRead, BufReader, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use kikigaki_core::config::{sidecar_endpoint, RemoteConfig};
use kikigaki_core::remote::{SidecarProcess, SidecarSpawner};

const SIDECAR_PATTERN: &str = "realtime_transcribe.py --input ws";
const PORT_RELEASE_TIMEOUT: Duration = Duration::from_secs(3);

pub struct MacSpawner;

impl SidecarSpawner for MacSpawner {
    fn spawn(&self, cfg: &RemoteConfig) -> anyhow::Result<Option<Box<dyn SidecarProcess>>> {
        if !cfg.spawn_sidecar {
            return Ok(None);
        }
        let (host, port) = sidecar_endpoint(&cfg.ws_url)?;
        let address = socket_address(&host, port)?;

        terminate_leftover_sidecars()?;
        wait_for_port_release(address)?;

        let port = port.to_string();
        let mut command = Command::new(&cfg.python);
        command
            .arg("scripts/realtime_transcribe.py")
            .args([
                "--input",
                "ws",
                "--ws-host",
                host.as_str(),
                "--ws-port",
                port.as_str(),
            ])
            .args(&cfg.extra_args)
            .current_dir(&cfg.hayamimi_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().with_context(|| {
            format!(
                "spawn hayamimi with {} in {}",
                cfg.python.display(),
                cfg.hayamimi_dir.display()
            )
        })?;
        if let Some(stdout) = child.stdout.take() {
            log_lines(stdout, "stdout");
        }
        if let Some(stderr) = child.stderr.take() {
            log_lines(stderr, "stderr");
        }
        tracing::info!(%address, "spawned hayamimi sidecar");
        Ok(Some(Box::new(MacProcess { child: Some(child) })))
    }
}

pub struct MacProcess {
    child: Option<Child>,
}

impl SidecarProcess for MacProcess {
    fn is_running(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                tracing::warn!(%status, "hayamimi sidecar exited");
                self.child.take();
                false
            }
            Err(error) => {
                tracing::warn!(%error, "failed to query hayamimi sidecar");
                false
            }
        }
    }

    fn kill(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        tracing::info!("stopping hayamimi sidecar");
        if let Err(error) = child.kill() {
            tracing::warn!(%error, "failed to kill hayamimi sidecar");
        }
        match child.wait() {
            Ok(status) => tracing::info!(%status, "hayamimi sidecar stopped"),
            Err(error) => tracing::warn!(%error, "failed waiting for hayamimi sidecar"),
        }
    }
}

impl Drop for MacProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

fn terminate_leftover_sidecars() -> anyhow::Result<()> {
    let output = Command::new("pgrep")
        .args(["-f", SIDECAR_PATTERN])
        .output()
        .context("find leftover hayamimi processes with pgrep")?;
    let count = if output.status.success() {
        String::from_utf8_lossy(&output.stdout).lines().count()
    } else if output.status.code() == Some(1) {
        0
    } else {
        bail!(
            "pgrep failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    };

    if count > 0 {
        let status = Command::new("pkill")
            .args(["-f", SIDECAR_PATTERN])
            .status()
            .context("terminate leftover hayamimi processes with pkill")?;
        if !status.success() && status.code() != Some(1) {
            bail!("pkill failed with {status}");
        }
    }
    tracing::info!(count, "killed leftover hayamimi processes");
    Ok(())
}

fn wait_for_port_release(address: SocketAddr) -> anyhow::Result<()> {
    let deadline = Instant::now() + PORT_RELEASE_TIMEOUT;
    while TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_ok() {
        if Instant::now() >= deadline {
            bail!("timed out waiting for {address} to become free");
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn socket_address(host: &str, port: u16) -> anyhow::Result<SocketAddr> {
    let ip = if host == "localhost" {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        host.parse()
            .with_context(|| format!("sidecar host is not an IP address: {host}"))?
    };
    Ok(SocketAddr::new(ip, port))
}

fn log_lines<R>(reader: R, stream: &'static str)
where
    R: Read + Send + 'static,
{
    thread::Builder::new()
        .name(format!("hayamimi-{stream}"))
        .spawn(move || {
            for line in BufReader::new(reader).lines() {
                match line {
                    Ok(line) => tracing::info!(target: "hayamimi", %stream, "{line}"),
                    Err(error) => {
                        tracing::warn!(target: "hayamimi", %stream, %error, "failed reading sidecar output");
                        break;
                    }
                }
            }
        })
        .expect("spawn hayamimi log reader");
}
