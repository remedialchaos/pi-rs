use pi_rs_chord::{
    Result,
    json::JsonValue,
    link::{
        self, Body, BoundEntry, FailedMember, Limits, Message, Part, Payload, Reader, StateEntry,
        WireEvent, Writer,
    },
};
#[test]
fn payload_segments_borrow_received_bytes_and_reserve_markers_only_for_byte_schemas() -> Result<()>
{
    use indexmap::IndexMap;
    use pi_rs_chord::json::JsonString;
    use pi_rs_chord::link::{LinkValue, parse_payload, payload_part};
    let bytes = [0, 1, 255];
    let mut object = IndexMap::new();
    object.insert(JsonString::from("__proto__"), LinkValue::Bytes(&bytes));
    let value = LinkValue::Object(object);
    let (json, segments) = payload_part(&value, true)?;
    assert_eq!(json, r#"{"__proto__":{"$bytes":0}}"#);
    let segments =
        segments.ok_or_else(|| pi_rs_chord::Error::Invalid("missing segments".into()))?;
    assert!(std::ptr::eq(segments[0].as_ptr(), bytes.as_ptr()));
    let decoded = parse_payload(&JsonValue::parse(&json)?, Some(&segments))?;
    let LinkValue::Object(map) = decoded else {
        return Err(pi_rs_chord::Error::Invalid("not an object".into()));
    };
    assert!(
        matches!(map.get(&JsonString::from("__proto__")),Some(LinkValue::Bytes(b)) if std::ptr::eq(b.as_ptr(),bytes.as_ptr()))
    );
    let marker = LinkValue::Json(JsonValue::parse(r#"{"nested":{"$bytes":0}}"#)?);
    assert!(payload_part(&marker, true).is_err());
    assert_eq!(
        payload_part(&marker, false)?.0,
        r#"{"nested":{"$bytes":0}}"#
    );
    for bad in [
        r#"{"$bytes":-1}"#,
        r#"{"$bytes":0.5}"#,
        r#"{"$bytes":2}"#,
        r#"{"$bytes":0,"x":1}"#,
    ] {
        assert!(parse_payload(&JsonValue::parse(bad)?, Some(&segments)).is_err());
    }
    assert!(matches!(
        parse_payload(&JsonValue::parse(r#"{"$bytes":0}"#)?, None)?,
        LinkValue::Json(_)
    ));
    Ok(())
}

fn part(s: &str) -> Part {
    Part::Bytes(s.as_bytes().to_vec())
}
#[test]
fn binary_all_nineteen_layouts_roundtrip_and_reject_trailing_or_truncated_bytes() -> Result<()> {
    let oracle: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("fixtures/link-binary-upstream.json"))?;
    let error = JsonValue::parse(r#"{"kind":"plugin","code":"x","message":"failed"}"#)?;
    let bodies = vec![
        Body::Hello {
            protocol: 1,
            incarnation: [7; 16],
            calls: Some(256),
        },
        Body::Hold {
            hold: 300,
            type_name: "app.notes".into(),
            version: 2,
            fingerprint: [9; 8],
            params: part("{}"),
            address_kind: "server".into(),
            address_params: Box::new(JsonValue::parse("{}")?),
            principal: Some(Box::new(JsonValue::parse(r#"{"kind":"guest"}"#)?)),
            incarnation: Some([4; 16]),
            chain: Some(vec![[3; 16]]),
            limit: Some(99),
        },
        Body::Release { hold: 3 },
        Body::Call {
            call: 3,
            hold: 0,
            method: 1,
            args: Some(Payload {
                part: part(r#"{"$bytes":0}"#),
                segments: Some(vec![vec![0, 1, 255], vec![]]),
            }),
        },
        Body::Abort { call: 3 },
        Body::Bound(vec![BoundEntry {
            hold: 1,
            variant: 2,
            members: vec![(3, part("[1]"))],
            failed: vec![FailedMember {
                member: 0,
                error: error.clone(),
            }],
        }]),
        Body::State(vec![StateEntry {
            hold: 2,
            members: vec![(1, part("[]"))],
            failed: vec![FailedMember {
                member: 0,
                error: error.clone(),
            }],
            retired: vec![1, 3],
        }]),
        Body::Failed {
            hold: 4,
            cause: 2,
            error: error.clone(),
            retry_in: Some(60_000),
        },
        Body::Result {
            call: 1,
            value: Some(Payload {
                part: Part::Deflated(vec![3, 0]),
                segments: None,
            }),
        },
        Body::Error {
            call: 1,
            error: error.clone(),
        },
        Body::Gone { hold: 1, error },
        Body::Retry { hold: 1, hops: 16 },
        Body::Loading { hold: 1 },
        Body::Ack { events: Some(4000) },
        Body::Closed,
        Body::Ping,
        Body::Pong,
        Body::Revoke(JsonValue::Null),
        Body::Event(vec![WireEvent {
            hold: 300,
            member: 2,
            at: 1_762_819_200_123,
            value: part("null"),
        }]),
    ];
    for (kind, body) in bodies.into_iter().enumerate() {
        let mut message = Message::new(body);
        if kind != 0 && kind != 15 && kind != 16 && kind != 17 {
            message.origin = Some(300);
        }
        if kind == 1 || kind == 3 {
            message.trace = Some(JsonValue::parse(r#"{"traceparent":"valid"}"#)?);
        }
        if kind == 3 {
            message.old = true;
        }
        let encoded = link::encode(&message)?;
        let hex = encoded
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            oracle[kind]["hex"].as_str(),
            Some(hex.as_str()),
            "upstream binary layout {kind}"
        );
        assert_eq!(encoded[0] & 31, kind as u8);
        assert_eq!(link::decode(&encoded)?, message);
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(link::decode(&trailing).is_err());
        if encoded.len() > 1 {
            assert!(link::decode(&encoded[..encoded.len() - 1]).is_err());
        }
    }
    assert_eq!(
        link::encode(&Message::new(Body::Retry {
            hold: 300,
            hops: 16
        }))?,
        vec![11, 172, 2, 16]
    );
    assert_eq!(
        link::decode(&[45, 1])?,
        Message::new(Body::Ack { events: Some(1) })
    );
    for bytes in [&[0xff][..], &[0x2f], &[0x8f, 1], &[3, 0, 0, 0, 2]] {
        assert!(link::decode(bytes).is_err());
    }
    Ok(())
}
#[test]
fn leb128_accepts_nonminimal_five_byte_slots_and_bounds_safe_integers() -> Result<()> {
    assert_eq!(Reader::new(&[0x81, 0x80, 0x80, 0x80, 0]).varint()?, 1);
    let mut writer = Writer::default();
    writer.varint(link::MAX_SAFE_INTEGER)?;
    assert_eq!(Reader::new(&writer.bytes).varint()?, link::MAX_SAFE_INTEGER);
    assert!(
        Writer::default()
            .varint(link::MAX_SAFE_INTEGER + 1)
            .is_err()
    );
    assert!(Reader::new(&[255; 9]).varint().is_err());
    let long = "界".repeat(4096);
    let mut writer = Writer::default();
    writer.string(&long)?;
    assert!(writer.bytes[..4].iter().all(|b| b & 128 != 0));
    assert_eq!(Reader::new(&writer.bytes).string()?, long);
    assert!(Reader::new(&[1, 255]).string().is_err());
    Ok(())
}
#[test]
fn raw_deflate_part_threshold_bombs_zero_limit_and_corruption_are_enforced() -> Result<()> {
    let text = format!("\"{}\"", "x".repeat(100_000));
    let part = Part::origin(text.clone(), true)?;
    assert!(matches!(part, Part::Deflated(_)));
    let Part::Deflated(compressed) = &part else {
        return Err(pi_rs_chord::Error::Invalid(
            "expected compressed part".into(),
        ));
    };
    for count in [1, 2, 5] {
        assert!(
            Part::Deflated(compressed[..compressed.len() - count].to_vec())
                .open(Limits::for_link(true))
                .is_err()
        );
    }
    let limit_error = Part::Bytes(vec![1, 2]).open(Limits {
        part: 1,
        decoded: 99,
    });
    assert!(
        matches!(limit_error,Err(pi_rs_chord::Error::Chord(ref e)) if e.message=="a part of 2 bytes is over the wire limit of 1 bytes")
    );
    assert_eq!(
        part.open(Limits {
            part: 1000,
            decoded: text.len()
        })?,
        text.as_bytes()
    );
    assert!(
        part.open(Limits {
            part: 1000,
            decoded: 20
        })
        .is_err()
    );
    assert!(
        part.open(Limits {
            part: 0,
            decoded: 100_002
        })
        .is_err()
    );
    assert!(
        Part::Deflated(vec![255, 255, 255])
            .open(Limits::for_link(true))
            .is_err()
    );
    assert!(
        Part::Deflated(vec![3])
            .open(Limits::for_link(true))
            .is_err()
    );
    assert!(
        Part::Bytes(vec![1])
            .open(Limits {
                part: 1,
                decoded: 0
            })
            .is_err()
    );
    assert_eq!(
        Part::Deflated(vec![3, 0]).open(Limits {
            part: 2,
            decoded: 0
        })?,
        Vec::<u8>::new()
    );
    assert!(matches!(
        Part::origin("a".repeat(16_384), true)?,
        Part::Bytes(_)
    ));
    assert!(matches!(Part::origin("a".repeat(10), true)?, Part::Text(_)));
    Ok(())
}
