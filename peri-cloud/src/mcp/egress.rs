//! Egress policy for account-configured MCP endpoints.
//!
//! Only public HTTPS origins are allowed. This prevents an account from using
//! the cloud host as a probe into loopback, WireGuard, or link-local services.
//! DNS is re-resolved here at configuration time; the MCP client resolves again
//! when connecting, so this is an admission check, not a pinned route.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use url::{Host, Url};

use super::McpConfigError;

const MAX_URL_BYTES: usize = 2048;
const MAX_HOST_BYTES: usize = 253;

pub(crate) async fn validate(raw: &str) -> Result<String, McpConfigError> {
    let url = shape(raw)?;
    match url.host() {
        Some(Host::Ipv4(address)) => ensure_global(IpAddr::V4(address))?,
        Some(Host::Ipv6(address)) => ensure_global(IpAddr::V6(address))?,
        Some(Host::Domain(domain)) => {
            let port = url
                .port_or_known_default()
                .ok_or(McpConfigError::Url("URL 缺少可用的端口"))?;
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((domain, port))
                .await
                .map_err(|_| McpConfigError::Url("域名解析失败"))?
                .collect();
            if addrs.is_empty() {
                return Err(McpConfigError::Url("域名没有可用地址"));
            }
            for address in addrs {
                ensure_global(address.ip())?;
            }
        }
        None => return Err(McpConfigError::Url("URL 缺少主机名")),
    }
    Ok(url.to_string())
}

fn shape(raw: &str) -> Result<Url, McpConfigError> {
    if raw.len() > MAX_URL_BYTES {
        return Err(McpConfigError::Url("URL 过长"));
    }
    let url = Url::parse(raw).map_err(|_| McpConfigError::Url("无法解析 URL"))?;
    if url.scheme() != "https" {
        return Err(McpConfigError::Url("只接受 https:// 端点"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(McpConfigError::Url("URL 不能携带用户名或密码"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(McpConfigError::Url("URL 不能携带查询参数或片段"));
    }
    if url.port() == Some(0) {
        return Err(McpConfigError::Url("端口无效"));
    }
    let host = url
        .host_str()
        .ok_or(McpConfigError::Url("URL 缺少主机名"))?;
    if host.is_empty() || host.len() > MAX_HOST_BYTES {
        return Err(McpConfigError::Url("主机名无效"));
    }
    if let Some(Host::Domain(domain)) = url.host() {
        let domain = domain.to_ascii_lowercase();
        if !domain.contains('.')
            || domain == "localhost"
            || domain.ends_with(".localhost")
            || domain.ends_with(".local")
            || domain.ends_with(".internal")
            || domain.ends_with(".home.arpa")
        {
            return Err(McpConfigError::Url("不接受本机、内网或单标签主机名"));
        }
    }
    Ok(url)
}

fn ensure_global(address: IpAddr) -> Result<(), McpConfigError> {
    if global(address) {
        Ok(())
    } else {
        Err(McpConfigError::Url("不接受回环、内网、链路本地或保留地址"))
    }
}

pub(crate) fn global(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => global_v4(address),
        IpAddr::V6(address) => global_v6(address),
    }
}

fn global_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        || octets[0] >= 240)
}

fn global_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return global_v4(mapped);
    }
    let segments = ip.segments();
    // ::/96 IPv4-compatible, ULA, link-local, site-local, documentation,
    // Teredo, 6to4 and NAT64 can all encode a non-public destination.
    if segments[0] == 0
        && segments[1] == 0
        && segments[2] == 0
        && segments[3] == 0
        && segments[4] == 0
    {
        return false;
    }
    if (segments[0] & 0xfe00) == 0xfc00 {
        return false;
    }
    if (segments[0] & 0xffc0) == 0xfe80 || (segments[0] & 0xffc0) == 0xfec0 {
        return false;
    }
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return false;
    }
    if segments[0] == 0x2001 && segments[1] == 0x0000 {
        return false;
    }
    if segments[0] == 0x2002 {
        return false;
    }
    if segments[0] == 0x0064 && segments[1] == 0xff9b {
        return false;
    }
    true
}

#[cfg(test)]
#[path = "egress_test.rs"]
mod tests;
