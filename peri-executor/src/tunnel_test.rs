use super::*;
use crate::Args as ExecutorArgs;
use clap::Parser;

#[test]
fn test_tunnel_cli_requires_complete_endpoint_pairs() {
    for extra in [
        vec!["--ssh-host", "fixture"],
        vec!["--ssh-reverse-listen", "127.0.0.1:34567"],
        vec!["--cloud-listen", "127.0.0.1:45678"],
    ] {
        let mut arguments = vec!["peri-executor", "--state-dir", "fixture"];
        arguments.extend(extra);
        let error = ExecutorArgs::try_parse_from(arguments).err().unwrap();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }
}

#[test]
fn test_tunnel_rejects_public_ports_and_ssh_option_injection() {
    let mut args = TunnelArgs {
        ssh_host: Some("fixture".into()),
        ..Default::default()
    };
    args.ssh_reverse_listen = Some("0.0.0.0:34567".parse().unwrap());
    assert!(args
        .validate()
        .unwrap_err()
        .to_string()
        .contains("loopback"));
    args.ssh_reverse_listen = Some("127.0.0.1:0".parse().unwrap());
    assert!(args.validate().unwrap_err().to_string().contains("nonzero"));
    args.ssh_reverse_listen = Some("127.0.0.1:34567".parse().unwrap());
    args.ssh_host = Some("-oProxyCommand=fixture".into());
    assert!(args
        .validate()
        .unwrap_err()
        .to_string()
        .contains("configured alias"));
}

#[test]
fn test_tunnel_uses_strict_ssh_and_both_loopback_forwards() {
    let parsed = ExecutorArgs::try_parse_from([
        "peri-executor",
        "--state-dir",
        "fixture",
        "--ssh-host",
        "fixture",
        "--ssh-reverse-listen",
        "127.0.0.1:34567",
        "--cloud-listen",
        "127.0.0.1:45678",
        "--cloud-remote",
        "127.0.0.1:56789",
    ])
    .unwrap();
    parsed.tunnel.validate().unwrap();
    let command = parsed.tunnel.command("127.0.0.1:42371".parse().unwrap());
    let args: Vec<_> = command
        .as_std()
        .get_args()
        .map(|s| s.to_str().unwrap())
        .collect();
    assert!(args.contains(&"StrictHostKeyChecking=yes"));
    assert!(args.contains(&"BatchMode=yes"));
    assert!(args.contains(&"127.0.0.1:34567:127.0.0.1:42371"));
    assert!(args.contains(&"127.0.0.1:45678:127.0.0.1:56789"));
    assert_eq!(args.last(), Some(&"fixture"));
}
