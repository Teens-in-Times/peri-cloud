use super::*;

#[test]
fn documented_qq_seed_and_independent_openssl_signature_verify_exact_body() {
    // Public Tencent bot-docs seed/public key; no account credentials. The body
    // signature is independently computed by Node/OpenSSL. The docs' published
    // body signature fails both OpenSSL and strict dalek verification.
    let key = signing_key("naOC0ocQE3shWLAfffVLB1rhYPG7").unwrap();
    let body = br#"{ "op": 0,"d": {}, "t": "GATEWAY_EVENT_NAME"}"#;
    assert_eq!(
        key.verifying_key().to_bytes(),
        [
            215, 195, 98, 254, 120, 174, 248, 31, 242, 50, 135, 180, 147, 98, 139, 93, 176, 42, 60,
            79, 227, 11, 33, 94, 77, 25, 96, 155, 93, 118, 103, 58
        ]
    );
    let signature="2eb9983ebb8bb209e78fd095942f58e442656656e7975d01e64f9023a84b7c964290fdd40e5500c33867ccfe9563b7e0b6bac0e1d42c13e787b304fd51f71102";
    verify(&key, "1725442341", body, signature).unwrap();
    assert!(matches!(
        verify(&key, "1725442342", body, signature),
        Err(QqError::Authentication)
    ));
    assert!(matches!(
        verify(
            &key,
            "1725442341",
            br#"{"op":0,"d":{},"t":"GATEWAY_EVENT_NAME"}"#,
            signature
        ),
        Err(QqError::Authentication)
    ));
}

#[test]
fn unsigned_url_challenge_cannot_sign_an_event_json() {
    let key = signing_key("fixture").unwrap();
    let signature = challenge_signature(&key, "fixture-challenge", "1725442341").unwrap();
    key.verifying_key()
        .verify_strict(b"1725442341fixture-challenge", &signature)
        .unwrap();
    assert!(matches!(
        challenge_signature(&key, r#"{"op":0,"t":"C2C_MESSAGE_CREATE"}"#, "1725442341"),
        Err(QqError::Event)
    ));
}

#[test]
fn malformed_signature_and_empty_secret_are_rejected() {
    assert!(matches!(signing_key(""), Err(QqError::Configuration)));
    let key = signing_key("fixture").unwrap();
    for value in ["0".repeat(126), "z".repeat(128), "0".repeat(128)] {
        assert!(matches!(
            verify(&key, "1", b"{}", &value),
            Err(QqError::Authentication)
        ));
    }
}

#[test]
fn duplicate_signature_headers_are_ambiguous() {
    let mut headers = HeaderMap::new();
    headers.append("x-signature-timestamp", "1".parse().unwrap());
    headers.append("x-signature-timestamp", "2".parse().unwrap());
    assert!(matches!(
        one_header(&headers, "x-signature-timestamp"),
        Err(QqError::Authentication)
    ));
}
