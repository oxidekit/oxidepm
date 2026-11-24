//! `docs/ADAPTER-PROTOCOL.md` shows frames; each one must be a golden frame, byte for byte as JSON,
//! so the document cannot drift from the files the host tests read.

use std::path::PathBuf;

use serde_json::Value;

#[test]
fn frames_in_the_protocol_document_are_golden_frames() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let doc = match std::fs::read_to_string(root.join("../../docs/ADAPTER-PROTOCOL.md")) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("skipped: the document is not next to this crate ({e})");
            return;
        }
    };
    let golden: Vec<Value> = std::fs::read_dir(root.join("tests/golden"))
        .unwrap()
        .map(|entry| {
            serde_json::from_slice(&std::fs::read(entry.unwrap().path()).unwrap()).unwrap()
        })
        .collect();

    let mut shown = 0;
    for line in doc.lines().filter(|l| l.starts_with("{\"jsonrpc\"")) {
        let frame: Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON: {line}: {e}"));
        assert!(
            golden.contains(&frame),
            "the document shows a frame that is not a golden frame: {line}"
        );
        shown += 1;
    }
    assert!(shown >= 2, "the document shows the handshake frames");
}
