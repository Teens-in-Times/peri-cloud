use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[tokio::test]
async fn public_literal_endpoints_are_accepted() {
    assert!(super::validate("https://93.184.216.34/mcp").await.is_ok());
    assert!(
        super::validate("https://[2606:2800:220:1:248:1893:25c8:1946]/mcp")
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn only_https_without_embedded_secrets_is_accepted() {
    for url in [
        "http://93.184.216.34/mcp",
        "https://user:pass@93.184.216.34/mcp",
        "https://93.184.216.34/mcp?token=secret",
        "https://93.184.216.34/mcp#fragment",
        "https://93.184.216.34:0/mcp",
    ] {
        assert!(
            super::validate(url).await.is_err(),
            "{url} must be rejected"
        );
    }
}

#[tokio::test]
async fn local_and_single_label_hosts_are_rejected_without_resolution() {
    for url in [
        "https://localhost/mcp",
        "https://service.localhost/mcp",
        "https://printer.local/mcp",
        "https://vault.internal/mcp",
        "https://intranet/mcp",
    ] {
        assert!(
            super::validate(url).await.is_err(),
            "{url} must be rejected"
        );
    }
}

#[tokio::test]
async fn non_public_ip_ranges_are_rejected() {
    for address in [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "192.0.2.1",
        "198.18.0.1",
        "224.0.0.1",
        "255.255.255.255",
        "0.0.0.0",
    ] {
        let url = format!("https://{address}/mcp");
        assert!(
            super::validate(&url).await.is_err(),
            "{url} must be rejected"
        );
    }
    for address in [
        "::1",
        "fc00::1",
        "fe80::1",
        "2001:db8::1",
        "::ffff:127.0.0.1",
    ] {
        let url = format!("https://[{address}]/mcp");
        assert!(
            super::validate(&url).await.is_err(),
            "{url} must be rejected"
        );
    }
}

#[test]
fn global_check_accepts_documentation_external_addresses() {
    assert!(super::global(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))));
    assert!(super::global(IpAddr::V6(Ipv6Addr::new(
        0x2606, 0x2800, 0x220, 1, 0x248, 0x1893, 0x25c8, 0x1946
    ))));
    assert!(!super::global(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
}
