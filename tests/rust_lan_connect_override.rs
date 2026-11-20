//! LAN connect-override derivation on the Manager host (GUI onboarding path).
//! Root cause dist/live-20261120-real-01: the model origin resolves publicly
//! to a dead address while the Manager host's resolver serves the live LAN
//! address. The Manager must stage that LAN address as connect_addr so the
//! Linux worker can reach the endpoint the user validated.
use agentranch::core::provision::derive_connect_addr;

#[test]
fn private_manager_resolution_derives_override() {
    assert_eq!(
        derive_connect_addr("https://boapi.uno:18317", &["172.28.8.128".parse().unwrap()]),
        Some("172.28.8.128:18317".to_owned())
    );
    assert_eq!(
        derive_connect_addr("https://internal.example", &["10.1.2.3".parse().unwrap()]),
        Some("10.1.2.3:443".to_owned())
    );
}

#[test]
fn public_resolution_and_ip_literals_never_derive_override() {
    assert_eq!(derive_connect_addr("https://boapi.uno:18317", &["182.139.39.10".parse().unwrap()]), None);
    assert_eq!(derive_connect_addr("https://boapi.uno:18317", &[]), None);
    // An IP-literal origin never involves resolution.
    assert_eq!(derive_connect_addr("https://172.28.8.128:18317", &["172.28.8.128".parse().unwrap()]), None);
}
