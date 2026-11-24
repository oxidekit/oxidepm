use std::io::{BufReader, Cursor, Read as _};

use serde_json::json;

use crate::codes;
use crate::frame::{parse_frame, ErrorObject, Frame, FrameError};
use crate::limits::Limits;
use crate::messages::{method, InitializeResult};
use crate::reader::{FrameReader, LineAssembler, LineEvent, Read};

const PING: &str = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;

#[test]
fn unknown_fields_are_ignored() {
    let line = br#"{"jsonrpc":"2.0","id":7,"extra":[1,2],"result":{"protocol":1,"name":"n","version":"0.1.0","future":{"a":1}},"trace":"x"}"#;
    let frame = parse_frame(line).unwrap();
    let result: InitializeResult = frame.decode().unwrap();
    assert_eq!(
        result,
        InitializeResult {
            protocol: 1,
            name: "n".into(),
            version: "0.1.0".into()
        }
    );

    // An unknown method is still a well-formed request; answering it is the receiver's job.
    let req = parse_frame(br#"{"jsonrpc":"2.0","id":2,"method":"some.future.method","params":{"x":1},"_meta":{"trace":"t"}}"#).unwrap();
    assert_eq!(req.method(), Some("some.future.method"));
    // And an unknown notification parses too.
    assert!(matches!(
        parse_frame(br#"{"jsonrpc":"2.0","method":"later","params":{}}"#).unwrap(),
        Frame::Notification { .. }
    ));
}

#[test]
fn bounded_reader_discards_oversize_line_and_resyncs() {
    let max = 1000;
    let big = "x".repeat(max + 500);
    let stream = format!("{PING}\n{big}\n{PING}\n");
    // A tiny buffer so that the long line arrives in many chunks.
    let mut r = FrameReader::new(
        BufReader::with_capacity(7, Cursor::new(stream.into_bytes())),
        max,
    );
    assert!(matches!(
        r.read_frame().unwrap(),
        Read::Frame(Frame::Request { id: 1, .. })
    ));
    assert_eq!(
        r.read_frame().unwrap(),
        Read::Overflow {
            discarded: (max + 500) as u64
        }
    );
    // The stream is in step again: the next line is a good frame.
    assert!(matches!(
        r.read_frame().unwrap(),
        Read::Frame(Frame::Request { id: 1, .. })
    ));
    assert_eq!(r.read_frame().unwrap(), Read::Eof);
}

#[test]
fn bounded_reader_boundary_is_the_limit_not_one_more() {
    let mut a = LineAssembler::new(10);
    let (used, ev) = a.feed(b"0123456789\n");
    assert_eq!(
        (used, ev),
        (11, Some(LineEvent::Line(b"0123456789".to_vec())))
    );
    let (used, ev) = a.feed(b"0123456789A\nrest");
    assert_eq!(
        (used, ev),
        (12, Some(LineEvent::Overflow { discarded: 11 }))
    );
    // An overflow that spans chunks, with the limit crossed in the second one.
    assert_eq!(a.feed(b"01234"), (5, None));
    assert_eq!(a.feed(b"56789AB"), (7, None));
    assert_eq!(
        a.feed(b"CD\nok\n"),
        (3, Some(LineEvent::Overflow { discarded: 14 }))
    );
    assert_eq!(a.feed(b"ok\n"), (3, Some(LineEvent::Line(b"ok".to_vec()))));
}

#[test]
fn bounded_reader_never_holds_more_than_limit() {
    let max = Limits::PROTOCOL_1.max_line;
    let giant: u64 = 100 * 1024 * 1024;
    let tail = format!("\n{PING}\n");
    let stream = std::io::repeat(b'x')
        .take(giant)
        .chain(Cursor::new(tail.into_bytes()));
    let mut r = FrameReader::new(BufReader::with_capacity(8192, stream), max);

    assert_eq!(r.read_frame().unwrap(), Read::Overflow { discarded: giant });
    // The buffer capacity is a property of the reader, not of process memory (allocator retention
    // would make that flaky): it never grew past the limit plus a small constant.
    assert!(
        r.assembler().peak_capacity() <= max + 4096,
        "peak capacity {}",
        r.assembler().peak_capacity()
    );
    assert_eq!(r.assembler().buffered(), 0);
    assert!(matches!(
        r.read_frame().unwrap(),
        Read::Frame(Frame::Request { id: 1, .. })
    ));
}

#[test]
fn truncated_line_then_eof_is_reported() {
    let partial = br#"{"jsonrpc":"2.0","id":1,"met"#.to_vec();
    let n = partial.len();
    let mut r = FrameReader::new(Cursor::new(partial), 1000);
    assert_eq!(r.read_frame().unwrap(), Read::Truncated { bytes: n });
    assert_eq!(r.read_frame().unwrap(), Read::Eof);
    // A long line cut off by the end of input is also truncated, and nothing is kept.
    let mut r = FrameReader::new(Cursor::new(vec![b'x'; 5000]), 1000);
    assert_eq!(r.read_frame().unwrap(), Read::Truncated { bytes: 5000 });
    assert_eq!(r.assembler().buffered(), 0);
}

#[tokio::test]
async fn async_reader_matches_blocking_reader() {
    use crate::reader::AsyncFrameReader;
    use tokio::io::{AsyncReadExt, BufReader as TokioBufReader};

    let max = 4096;
    let giant: u64 = 10 * 1024 * 1024;
    let tail = format!("\n{PING}\n");
    let stream = tokio::io::repeat(b'y')
        .take(giant)
        .chain(Cursor::new(tail.into_bytes()));
    let mut r = AsyncFrameReader::new(TokioBufReader::with_capacity(8192, stream), max);
    assert_eq!(
        r.read_frame().await.unwrap(),
        Read::Overflow { discarded: giant }
    );
    assert!(r.assembler().peak_capacity() <= max + 4096);
    assert!(matches!(
        r.read_frame().await.unwrap(),
        Read::Frame(Frame::Request { id: 1, .. })
    ));
    assert_eq!(r.read_frame().await.unwrap(), Read::Eof);
}

#[test]
fn reader_rejects_invalid_utf8_nul_and_non_object() {
    let cases: Vec<(&[u8], FrameError)> = vec![
        (b"\xff\xfe\xfd", FrameError::InvalidUtf8),
        (
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"p\xc3\x28\"}",
            FrameError::InvalidUtf8,
        ),
        (
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"\0}",
            FrameError::Nul,
        ),
        (b"\0", FrameError::Nul),
        (b"[]", FrameError::Batch),
        (
            br#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
            FrameError::Batch,
        ),
        (b"\"ping\"", FrameError::NotObject),
        (b"42", FrameError::NotObject),
        (b"null", FrameError::NotObject),
        (b"true", FrameError::NotObject),
        (b"", FrameError::Empty),
        (b"   ", FrameError::Empty),
        (b"{}", FrameError::BadVersion),
        (
            br#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
            FrameError::BadVersion,
        ),
        (
            br#"{"jsonrpc":2.0,"id":1,"method":"ping"}"#,
            FrameError::BadVersion,
        ),
    ];
    for (line, want) in cases {
        assert_eq!(
            parse_frame(line).unwrap_err(),
            want,
            "{:?}",
            String::from_utf8_lossy(line)
        );
    }
    assert!(matches!(
        parse_frame(b"{\"jsonrpc\":\"2.0\",\"id\":").unwrap_err(),
        FrameError::NotJson(_)
    ));

    // Through the reader each of them is a violation and the stream stays in step.
    let stream = format!("\u{fffd}\n[]\n{PING}\n").into_bytes();
    let mut r = FrameReader::new(Cursor::new(stream), 1000);
    assert!(matches!(
        r.read_frame().unwrap(),
        Read::Violation(FrameError::BadVersion | FrameError::NotJson(_))
    ));
    assert_eq!(r.read_frame().unwrap(), Read::Violation(FrameError::Batch));
    assert!(matches!(r.read_frame().unwrap(), Read::Frame(_)));
}

#[test]
fn id_must_be_unsigned_integer() {
    for id in [
        "-1",
        "1.5",
        "1.0",
        "\"1\"",
        "null",
        "true",
        "[1]",
        "{}",
        "1e3",
        "18446744073709551616",
    ] {
        let line = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping","params":{{}}}}"#);
        assert_eq!(
            parse_frame(line.as_bytes()).unwrap_err(),
            FrameError::BadId,
            "request id {id}"
        );
        let line = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#);
        assert_eq!(
            parse_frame(line.as_bytes()).unwrap_err(),
            FrameError::BadId,
            "response id {id}"
        );
        let line =
            format!(r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32603,"message":"m"}}}}"#);
        assert_eq!(
            parse_frame(line.as_bytes()).unwrap_err(),
            FrameError::BadId,
            "error id {id}"
        );
    }
    // The largest unsigned 64-bit value is fine, and zero parses (numbering is the sender's job).
    for id in ["18446744073709551615", "0", "1"] {
        let line = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping","params":{{}}}}"#);
        assert!(
            matches!(parse_frame(line.as_bytes()).unwrap(), Frame::Request { .. }),
            "{id}"
        );
    }
    // A response and an error always have an id.
    assert_eq!(
        parse_frame(br#"{"jsonrpc":"2.0","result":{}}"#).unwrap_err(),
        FrameError::BadId
    );
    assert_eq!(
        parse_frame(br#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"m"}}"#).unwrap_err(),
        FrameError::BadId
    );
}

#[test]
fn frame_shape_rules() {
    let bad = |line: &str| parse_frame(line.as_bytes()).unwrap_err();
    // params and result are objects.
    assert_eq!(
        bad(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":[]}"#),
        FrameError::BadParams
    );
    assert_eq!(
        bad(r#"{"jsonrpc":"2.0","method":"event","params":"x"}"#),
        FrameError::BadParams
    );
    assert_eq!(
        bad(r#"{"jsonrpc":"2.0","id":1,"result":[]}"#),
        FrameError::BadParams
    );
    assert_eq!(
        bad(r#"{"jsonrpc":"2.0","id":1,"result":1}"#),
        FrameError::BadParams
    );
    // A response has a result or an error, not both, not neither; a method excludes both.
    assert!(matches!(
        bad(r#"{"jsonrpc":"2.0","id":1}"#),
        FrameError::BadShape(_)
    ));
    assert!(matches!(
        bad(r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":1,"message":"m"}}"#),
        FrameError::BadShape(_)
    ));
    assert!(matches!(
        bad(r#"{"jsonrpc":"2.0","id":1,"method":"ping","result":{}}"#),
        FrameError::BadShape(_)
    ));
    // The error object needs an integer code and a string message.
    assert!(matches!(
        bad(r#"{"jsonrpc":"2.0","id":1,"error":{"code":"x","message":"m"}}"#),
        FrameError::BadShape(_)
    ));
    assert!(matches!(
        bad(r#"{"jsonrpc":"2.0","id":1,"error":{"code":1.5,"message":"m"}}"#),
        FrameError::BadShape(_)
    ));
    assert!(matches!(
        bad(r#"{"jsonrpc":"2.0","id":1,"error":{"code":1}}"#),
        FrameError::BadShape(_)
    ));
    assert!(matches!(
        bad(r#"{"jsonrpc":"2.0","id":1,"error":"boom"}"#),
        FrameError::BadShape(_)
    ));
    // Method names are short and plain.
    for m in ["", "has space", "new\\nline", "a/b", "ü"] {
        let line = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{m}"}}"#);
        assert_eq!(bad(&line), FrameError::BadMethod, "{m:?}");
    }
    let long = "m".repeat(65);
    assert_eq!(
        bad(&format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{long}"}}"#)),
        FrameError::BadMethod
    );
    assert_eq!(
        bad(r#"{"jsonrpc":"2.0","id":1,"method":5}"#),
        FrameError::BadMethod
    );
    // Missing params is an empty object.
    let f = parse_frame(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
    assert!(matches!(f, Frame::Request { ref params, .. } if params.is_empty()));
}

#[test]
fn encode_is_one_line_and_limited() {
    let f = Frame::request(
        5,
        method::HOST_AUDIT_NOTE,
        &json!({"text": "line one\nline two\u{2028}end"}),
    )
    .unwrap();
    let bytes = f.encode();
    assert_eq!(
        bytes.iter().filter(|&&b| b == b'\n').count(),
        1,
        "the only newline is the terminator"
    );
    assert_eq!(parse_frame(&bytes[..bytes.len() - 1]).unwrap(), f);

    let size = bytes.len() - 1;
    assert!(f.encode_limited(size).is_ok());
    assert_eq!(
        f.encode_limited(size - 1).unwrap_err(),
        FrameError::TooLarge {
            size,
            max: size - 1
        }
    );
    // Not an object: refused at construction.
    assert_eq!(
        Frame::request(1, "ping", &json!([1])).unwrap_err(),
        FrameError::BadParams
    );
}

#[test]
fn error_objects_carry_the_stable_name() {
    let e = ErrorObject::with_name(codes::rpc::TIMEOUT, codes::E_ADAPTER_TIMEOUT, "too slow");
    let f = Frame::error(3, e.clone());
    let back = parse_frame(&f.encode()).unwrap();
    assert_eq!(back, f);
    let Frame::Error { error, .. } = back else {
        panic!()
    };
    assert_eq!(error.name(), Some("E_ADAPTER_TIMEOUT"));
    assert_eq!(ErrorObject::new(-32603, "x").name(), None);
}

#[test]
fn error_codes_are_unique_and_well_formed() {
    let mut names = codes::ALL_NAMES.to_vec();
    names.sort_unstable();
    let n = names.len();
    names.dedup();
    assert_eq!(names.len(), n, "duplicate error code name");
    for name in names {
        assert!(
            name.starts_with("E_") && name.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'),
            "{name}"
        );
    }
    for code in [
        codes::rpc::UNSUPPORTED_PROTOCOL,
        codes::rpc::PERMISSION_DENIED,
        codes::rpc::TIMEOUT,
        codes::rpc::DRY_RUN_UNSUPPORTED,
        codes::rpc::RATE_LIMITED,
        codes::rpc::BUSY,
    ] {
        assert!((-32099..=-32020).contains(&code));
        assert!(codes::ALL_NAMES.contains(&codes::name_for_rpc_code(code).unwrap()));
    }
    assert_eq!(codes::name_for_rpc_code(-32603), None);
}

#[test]
fn protocol_limits_match_the_wire_contract() {
    let l = Limits::PROTOCOL_1;
    assert_eq!(l.max_line, 262_144);
    assert_eq!(l.event_line, 16_384);
    assert_eq!((l.max_in_flight, l.event_queue), (32, 1024));
    assert_eq!(
        (l.invoke_output_frame, l.invoke_output_total),
        (65_536, 8 * 1024 * 1024)
    );
    assert_eq!((l.strikes_to_kill, l.strike_window_secs), (3, 60));
    assert_eq!((l.tombstones, l.tombstone_secs), (1024, 300));
    assert_eq!(l.plan_args_bytes, 131_072);
    // The init message tells the adapter exactly these three.
    let init = l.for_initialize();
    assert_eq!(
        (init.max_line, init.max_in_flight, init.event_queue),
        (262_144, 32, 1024)
    );
    // A plan whose arguments are at the limit still fits in one line.
    assert!(l.plan_args_bytes + 64 * 1024 < l.max_line);
}
