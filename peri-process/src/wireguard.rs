//! Explicit, OS-verified WireGuard transport. This does not change tool permissions.
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireGuardEndpoint {
    pub interface: String,
    pub endpoint: SocketAddr,
}

impl WireGuardEndpoint {
    pub fn validate(&self) -> io::Result<()> {
        if self.interface.is_empty()
            || self.interface.len() > 63
            || self.interface.starts_with('-')
            || !self
                .interface
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
            || self.endpoint.port() == 0
            || !private(self.endpoint.ip())
        {
            return Err(invalid());
        }
        Ok(())
    }

    /// Verify the interface kind and selected route, returning its source address.
    pub async fn source_address(&self, listener: bool) -> io::Result<IpAddr> {
        self.validate()?;
        #[cfg(target_os = "linux")]
        {
            let link = output(Command::new("ip").args([
                "-j",
                "-d",
                "link",
                "show",
                "dev",
                &self.interface,
            ]))
            .await?;
            if link[0]["linkinfo"]["info_kind"] != "wireguard" {
                return Err(invalid());
            }
            let addresses =
                output(Command::new("ip").args(["-j", "address", "show", "dev", &self.interface]))
                    .await?;
            if listener {
                let own = addresses[0]["addr_info"].as_array().ok_or_else(invalid)?;
                if own.iter().any(|a| {
                    a["local"].as_str().and_then(|s| s.parse::<IpAddr>().ok())
                        == Some(self.endpoint.ip())
                }) {
                    return Ok(self.endpoint.ip());
                }
                return Err(invalid());
            }
            let peer = self.endpoint.ip().to_string();
            let route = output(Command::new("ip").args(["-j", "route", "get", &peer])).await?;
            if route[0]["dev"].as_str() != Some(self.interface.as_str()) {
                return Err(invalid());
            }
            let source = route[0]["prefsrc"]
                .as_str()
                .or_else(|| route[0]["src"].as_str())
                .ok_or_else(invalid)?;
            let source: IpAddr = source.parse().map_err(|_| invalid())?;
            if !private(source) {
                return Err(invalid());
            }
            Ok(source)
        }
        #[cfg(windows)]
        {
            // Both substitutions have been validated above; no credential enters this script.
            let script = format!(
                "$ErrorActionPreference='Stop'; $a=Get-NetAdapter -Name '{}'; if ($a.InterfaceDescription -notlike '*WireGuard*' -or $a.Status -ne 'Up') {{throw 'interface'}}; ",
                self.interface
            );
            let script = if listener {
                format!("{script}$p=@(Get-NetIPAddress -InterfaceIndex $a.ifIndex | Where-Object IPAddress -eq '{}'); if ($p.Count -ne 1) {{throw 'address'}}; @{{source=$p[0].IPAddress}} | ConvertTo-Json -Compress", self.endpoint.ip())
            } else {
                format!("{script}$p=Find-NetRoute -RemoteIPAddress '{}' | Where-Object {{ $_.IPAddress }} | Select-Object -First 1; if ($p.InterfaceIndex -ne $a.ifIndex) {{throw 'route'}}; @{{source=$p.IPAddress}} | ConvertTo-Json -Compress", self.endpoint.ip())
            };
            let mut command = Command::new("powershell.exe");
            command.creation_flags(0x08000000);
            let value =
                output(command.args(["-NoProfile", "-NonInteractive", "-Command", &script]))
                    .await?;
            let source: IpAddr = value["source"]
                .as_str()
                .ok_or_else(invalid)?
                .parse()
                .map_err(|_| invalid())?;
            if !private(source) {
                return Err(invalid());
            }
            Ok(source)
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = listener;
            Err(invalid())
        }
    }
}

fn private(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => ip.is_private(),
        IpAddr::V6(ip) => (ip.segments()[0] & 0xfe00) == 0xfc00,
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "WireGuard interface, address or route is invalid",
    )
}

async fn output(command: &mut Command) -> io::Result<Value> {
    command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(8), command.output())
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "WireGuard verification timed out")
        })??;
    if !result.status.success() || result.stdout.len() > 65536 {
        return Err(invalid());
    }
    serde_json::from_slice(&result.stdout).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_public_wildcard_loopback_and_shell_input() {
        for endpoint in [
            "0.0.0.0:42",
            "127.0.0.1:42",
            "8.8.8.8:42",
            "10.77.0.1:0",
            "[::]:42",
            "[fe80::1]:42",
        ] {
            assert!(WireGuardEndpoint {
                interface: "wg77".into(),
                endpoint: endpoint.parse().unwrap()
            }
            .validate()
            .is_err());
        }
        for interface in ["", "-wg", "wg;exit", "wg'", "$(whoami)"] {
            assert!(WireGuardEndpoint {
                interface: interface.into(),
                endpoint: "10.77.0.1:42".parse().unwrap()
            }
            .validate()
            .is_err());
        }
        for endpoint in ["10.77.0.1:42", "[fd77::1]:42"] {
            assert!(WireGuardEndpoint {
                interface: "wg77".into(),
                endpoint: endpoint.parse().unwrap()
            }
            .validate()
            .is_ok());
        }
    }
    #[tokio::test]
    async fn missing_interface_is_not_a_trusted_private_network() {
        let endpoint = WireGuardEndpoint {
            interface: "peri-does-not-exist".into(),
            endpoint: "10.77.0.1:42".parse().unwrap(),
        };
        assert!(endpoint.source_address(false).await.is_err());
        assert!(endpoint.source_address(true).await.is_err());
    }
}
