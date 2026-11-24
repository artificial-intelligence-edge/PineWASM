//! A program without lane declarations serializes to the pinned golden bytes.
//! The witness is the serialized CSIL artifact pinned by SHA-256 in the golden
//! bridge fixture; it is read out of that file so the golden bytes exist once.

use pinewasm_compiler::Program;
use sha2::{Digest, Sha256};

fn golden_artifact() -> &'static str {
    let source = include_str!("artifact_import_bridge.rs");
    let open = "ARTIFACT_BYTES: &[u8] = br#\"";
    let start = source.find(open).expect("the golden artifact literal") + open.len();
    let end = start + source[start..].find("\"#;").expect("end of the literal");
    &source[start..end]
}

#[test]
fn a_program_without_declarations_serializes_to_the_pinned_bytes() {
    let golden = golden_artifact();
    let digest: String = Sha256::digest(golden.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        digest,
        "de3f773dbaa49aa457ce08d30b5b94bfcc4596c9cd8c6d3dc433d7dcd60b8a05"
    );
    assert!(!golden.contains("\"lane\"") && !golden.contains("\"field_lanes\""));

    let program: Program = serde_json::from_str(golden).expect("the golden artifact decodes");
    assert!(program.series_slots.iter().all(|slot| slot.lane.is_none()));
    assert!(program
        .object_types
        .iter()
        .all(|decl| decl.field_lanes.is_empty()));
    assert_eq!(program.to_json(), golden);
}
