use super::*;
use serde_json::json;

#[test]
fn foreground_wait_uses_the_device_platform_and_millisecond_contract() {
    assert_eq!(
        parse_foreground_timeout_for_platform(&json!({"timeout": 1}), true),
        (5_000, None)
    );
    assert_eq!(
        parse_foreground_timeout_for_platform(&json!({"timeout": 1}), false),
        (1, None)
    );
    for windows in [false, true] {
        assert_eq!(
            parse_foreground_timeout_for_platform(&json!({}), windows),
            (15_000, None)
        );
        assert_eq!(
            parse_foreground_timeout_for_platform(&json!({"timeout":0}), windows),
            (120_000, Some(0))
        );
        assert_eq!(
            parse_foreground_timeout_for_platform(&json!({"timeout":999_999}), windows),
            (120_000, Some(999_999))
        );
    }
}
