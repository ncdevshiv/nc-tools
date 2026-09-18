// Protocol negotiation tests: the server must mirror the SDK's initialize
// behavior (echo the requested supported revision; otherwise answer with the
// latest) and must only emit structuredContent on revisions that define it.
use nct_mcp::{
    pick_protocol_version, structured_output_supported, LATEST_PROTOCOL_VERSION,
    SUPPORTED_PROTOCOL_VERSIONS,
};

#[test]
fn echoes_every_supported_requested_version() {
    for v in SUPPORTED_PROTOCOL_VERSIONS {
        assert_eq!(pick_protocol_version(v), v, "must echo supported {v}");
    }
}

#[test]
fn unknown_version_falls_back_to_latest() {
    assert_eq!(pick_protocol_version("2030-01-01"), LATEST_PROTOCOL_VERSION);
    assert_eq!(pick_protocol_version(""), LATEST_PROTOCOL_VERSION);
    assert_eq!(
        pick_protocol_version("2024-11-05 "),
        LATEST_PROTOCOL_VERSION
    );
}

#[test]
fn latest_is_the_first_supported_entry() {
    assert_eq!(LATEST_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS[0]);
}

#[test]
fn structured_output_only_on_2025_06_18_and_later() {
    assert!(structured_output_supported("2025-11-25"));
    assert!(structured_output_supported("2025-06-18"));
    assert!(!structured_output_supported("2025-03-26"));
    assert!(!structured_output_supported("2024-11-05"));
    assert!(!structured_output_supported("2024-10-07"));
}
