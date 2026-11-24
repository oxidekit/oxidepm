//! The golden frames of protocol 1. The host tests, the fixture adapter and the SDK read these
//! same files, so a change to one is a protocol change.

use std::path::PathBuf;

use oxidepm_adapter_proto::frame::{parse_frame, Frame};
use oxidepm_adapter_proto::messages::*;
use oxidepm_adapter_proto::reader::{FrameReader, Read};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// The typed object inside the frame must serialise back to exactly the params of the file.
fn typed<T: Serialize + DeserializeOwned>(frame: &Frame, original: &Value) {
    let value: T = frame.decode().expect("typed decode");
    let again = serde_json::to_value(&value).unwrap();
    let key = if frame.method().is_some() {
        "params"
    } else {
        "result"
    };
    assert_eq!(&again, original.get(key).unwrap(), "typed roundtrip");
}

fn check(name: &str, frame: &Frame, original: &Value) {
    match name {
        "initialize.request" => typed::<InitializeParams>(frame, original),
        "initialize.response" => typed::<InitializeResult>(frame, original),
        "initialized.notification" | "ping.request" | "ping.response" | "shutdown.request" => {
            assert_eq!(
                original.get("params").or(original.get("result")).unwrap(),
                &serde_json::json!({})
            );
        }
        "runtime.prepare.request" => typed::<PrepareParams>(frame, original),
        "runtime.prepare.response" => typed::<PrepareResult>(frame, original),
        "runtime.plan.request" => typed::<PlanParams>(frame, original),
        "runtime.plan.response" => typed::<PlanResult>(frame, original),
        "command.invoke.request" => typed::<InvokeParams>(frame, original),
        "command.invoke.response" => typed::<InvokeResult>(frame, original),
        "event.notification" => typed::<EventParams>(frame, original),
        "cancel.notification" => typed::<CancelParams>(frame, original),
        "host.request.request" => typed::<HostRequestParams>(frame, original),
        "host.request.response" => typed::<HostRequestResult>(frame, original),
        "host.audit_note.request" => typed::<AuditNoteParams>(frame, original),
        "invoke.output.notification" => typed::<InvokeOutputParams>(frame, original),
        "error.unsupported_protocol" | "error.permission_denied" => {
            assert!(matches!(frame, Frame::Error { .. }));
        }
        other => panic!("golden file {other} has no entry in the test table"),
    }
}

#[test]
fn golden_frames_roundtrip() {
    let mut seen = 0;
    let mut files: Vec<_> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    for path in files {
        let name = path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .strip_suffix(".json")
            .unwrap()
            .to_string();
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            bytes.ends_with(b"\n") && bytes.iter().filter(|&&b| b == b'\n').count() == 1,
            "{name}: one line"
        );
        let line = &bytes[..bytes.len() - 1];
        let original: Value = serde_json::from_slice(line).unwrap();

        let frame = parse_frame(line).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(frame.to_value(), original, "{name}: frame to value");
        let encoded = frame.encode();
        assert_eq!(
            parse_frame(&encoded[..encoded.len() - 1]).unwrap(),
            frame,
            "{name}: encode and parse"
        );
        check(&name, &frame, &original);

        // The same bytes come out of the bounded reader as that frame.
        let mut reader = FrameReader::new(std::io::Cursor::new(bytes), 256 * 1024);
        assert_eq!(
            reader.read_frame().unwrap(),
            Read::Frame(frame),
            "{name}: reader"
        );
        assert_eq!(reader.read_frame().unwrap(), Read::Eof);
        seen += 1;
    }
    assert_eq!(
        seen, 20,
        "a golden file was added or removed; update the table above"
    );
}
