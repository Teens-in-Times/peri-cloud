//! Device-owned reverse SSH. It carries executor traffic and an optional local
//! account portal; it never handles tool policy or cloud/account credentials.

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use clap::Args;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

#[derive(Args, Clone, Default)]
pub struct TunnelArgs {
    /// Existing SSH config alias. SSH selects its key and known-host record.
    #[arg(long, requires = "ssh_reverse_listen")]
    pub ssh_host: Option<String>,
    /// Cloud-side loopback endpoint forwarded to this executor.
    #[arg(long, requires = "ssh_host")]
    pub ssh_reverse_listen: Option<SocketAddr>,
    /// Optional device-side loopback address for the cloud account portal.
    #[arg(long, requires_all = ["ssh_host", "cloud_remote"])]
    pub cloud_listen: Option<SocketAddr>,
    /// Cloud-side loopback address of the cloud account service.
    #[arg(long, requires_all = ["ssh_host", "cloud_listen"])]
    pub cloud_remote: Option<SocketAddr>,
}

impl TunnelArgs {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(host) = &self.ssh_host {
            anyhow::ensure!(
                !host.is_empty()
                    && host.len() <= 128
                    && !host.starts_with('-')
                    && host
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c)),
                "SSH host must be a configured alias"
            );
        }
        for address in [
            self.ssh_reverse_listen,
            self.cloud_listen,
            self.cloud_remote,
        ]
        .into_iter()
        .flatten()
        {
            anyhow::ensure!(
                address.ip().is_loopback() && address.port() != 0,
                "SSH forwards must use nonzero loopback endpoints"
            );
        }
        Ok(())
    }

    fn command(&self, executor: SocketAddr) -> Command {
        let mut command = Command::new("ssh");
        command.args([
            "-N",
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
        ]);
        if let Some(remote) = self.ssh_reverse_listen {
            command.arg("-R").arg(format!("{remote}:{executor}"));
        }
        if let (Some(local), Some(remote)) = (self.cloud_listen, self.cloud_remote) {
            command.arg("-L").arg(format!("{local}:{remote}"));
        }
        command.arg(self.ssh_host.as_ref().expect("configured SSH host"));
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        command
    }

    pub async fn supervise(self, executor: SocketAddr, stop: CancellationToken) {
        if self.ssh_host.is_none() {
            return;
        }
        loop {
            if stop.is_cancelled() {
                return;
            }
            match self.command(executor).spawn() {
                Ok(mut child) => {
                    // A spawned SSH process is not a readiness receipt. The
                    // cloud authenticates the executor before selecting it.
                    tracing::info!(component = "device_ssh", "SSH connection process started");
                    tokio::select! {
                        biased;
                        _ = stop.cancelled() => {
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                            return;
                        }
                        result = child.wait() => {
                            tracing::warn!(component = "device_ssh", exited = result.is_ok(), "SSH connection ended; retrying");
                        }
                    }
                }
                Err(_) => tracing::warn!(component = "device_ssh", "SSH could not start; retrying"),
            }
            tokio::select! {
                biased;
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(3)) => (),
            }
        }
    }
}

#[cfg(test)]
#[path = "tunnel_test.rs"]
mod tests;
