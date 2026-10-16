use crate::client::{send, validate_device, Error, Result};
use peri_acp_types::device_executor::ExecutorInfo;
pub use peri_process::wireguard::WireGuardEndpoint;
use reqwest::{Client, Method};
use std::net::SocketAddr;
use uuid::Uuid;

#[derive(Clone)]
pub enum ExecutorEndpoint {
    Loopback(SocketAddr),
    WireGuard(WireGuardEndpoint),
}

#[derive(Clone)]
pub(crate) struct Connection {
    pub http: Client,
    pub endpoint: SocketAddr,
    wireguard: Option<WireGuardEndpoint>,
}

impl Connection {
    pub fn kind(&self) -> &'static str {
        if self.wireguard.is_some() {
            "wireguard"
        } else {
            "loopback"
        }
    }
    pub async fn verify_route(&self) -> Result<()> {
        if let Some(wg) = &self.wireguard {
            wg.source_address(false)
                .await
                .map_err(|_| Error::Disconnected)?;
        }
        Ok(())
    }
}

impl ExecutorEndpoint {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Loopback(endpoint) if endpoint.ip().is_loopback() && endpoint.port() != 0 => {
                Ok(())
            }
            Self::WireGuard(wg) => wg.validate().map_err(|_| Error::NonLoopback),
            _ => Err(Error::NonLoopback),
        }
    }

    async fn connection(&self) -> Result<Connection> {
        self.validate()?;
        let mut builder = Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none());
        let (endpoint, wireguard) = match self {
            Self::Loopback(endpoint) => (*endpoint, None),
            Self::WireGuard(wg) => {
                let source = wg
                    .source_address(false)
                    .await
                    .map_err(|_| Error::Disconnected)?;
                builder = builder.local_address(source);
                (wg.endpoint, Some(wg.clone()))
            }
        };
        Ok(Connection {
            http: builder.build().map_err(|_| Error::Protocol)?,
            endpoint,
            wireguard,
        })
    }
}

pub(crate) async fn select(
    routes: &[ExecutorEndpoint],
    token: &str,
    expected: Uuid,
) -> Result<(Connection, ExecutorInfo)> {
    // Validate every candidate first. Configuration/authentication errors never downgrade.
    if routes.is_empty() || routes.len() > 8 {
        return Err(Error::Protocol);
    }
    for route in routes {
        route.validate()?;
    }
    for route in routes {
        let attempt = async {
            let connection = route.connection().await?;
            let info = send(
                &connection.http,
                connection.endpoint,
                token,
                Method::GET,
                "/v1/info",
                None,
            )
            .await?;
            validate_device(&info, expected)?;
            Ok((connection, info))
        }
        .await;
        match attempt {
            Err(Error::Disconnected) => continue,
            other => return other,
        }
    }
    Err(Error::Disconnected)
}
