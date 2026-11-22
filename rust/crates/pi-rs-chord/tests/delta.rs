use pi_rs_chord::{
    Result,
    delta::{Decoder, Encoder, Op, PendingOps, PendingTake, Segment, apply_immutable},
    json::{JsonString, JsonValue},
};
fn key(s: &str) -> Vec<Segment> {
    vec![Segment::Key(s.into())]
}
#[test]
fn utf16_truncation_can_split_astral_text_and_never_mutates_previous_revision() -> Result<()> {
    let before = JsonValue::parse(r#"{"s":"😀xy","a":[1,2]}"#)?;
    let after = apply_immutable(
        &before,
        &[
            Op::Truncate(key("s"), 1),
            Op::Append(key("s"), JsonString(vec![0xd800])),
            Op::Splice(key("a"), 1, 99, vec![JsonValue::Null]),
        ],
    )?;
    assert_eq!(after.to_json()?, r#"{"s":"\ude00xy\ud800","a":[1,null]}"#);
    assert_eq!(before.to_json()?, r#"{"s":"😀xy","a":[1,2]}"#);
    for s in ["__proto__", "constructor", "prototype"] {
        assert!(apply_immutable(&before, &[Op::Set(key(s), JsonValue::Null)]).is_err());
    }
    assert!(
        apply_immutable(
            &JsonValue::parse("[1]")?,
            &[Op::Set(vec![Segment::Index(5)], JsonValue::Null)]
        )
        .is_err()
    );
    assert!(Op::from_json(&JsonValue::parse(r#"["m",[],[0,0]]"#)?).is_err());
    Ok(())
}
#[test]
fn path_dictionary_interns_on_second_nonconsecutive_use_and_resets_at_base() -> Result<()> {
    let mut encoder = Encoder::default();
    let mut decoder = Decoder::default();
    let ops = vec![
        Op::Append(key("s"), "a".into()),
        Op::Append(key("s"), "b".into()),
        Op::Set(key("n"), JsonValue::Null),
        Op::Append(key("s"), "c".into()),
    ];
    let wire = encoder.encode(&ops)?;
    assert_eq!(
        JsonValue::Array(wire.clone()).to_json()?,
        r##"[["a",["s"],"a"],["a","b"],["s",["n"],null],["#",0,["s"]],["a",0,"c"]]"##
    );
    assert_eq!(decoder.decode(&wire)?, ops);
    let next = vec![Op::Append(key("s"), "d".into())];
    assert_eq!(decoder.decode(&encoder.encode(&next)?)?, next);
    assert!(
        decoder
            .decode(&[JsonValue::parse(r#"["a","missing previous"]"#)?])
            .is_err()
    );
    let base = vec![
        Op::Replace(JsonValue::parse(r#"{"s":"fresh"}"#)?),
        Op::Append(key("s"), "!".into()),
    ];
    assert_eq!(Decoder::default().decode(&encoder.encode(&base)?)?, base);
    Ok(())
}
#[test]
fn pending_folds_leaves_but_structural_edits_and_overflow_are_barriers() -> Result<()> {
    let mut queue = PendingOps::default();
    queue.push(&[Op::Append(key("s"), JsonString(vec![0xd83d]))]);
    queue.push(&[
        Op::Set(key("n"), JsonValue::Number(1.0)),
        Op::Append(key("s"), JsonString(vec![0xde00])),
    ]);
    assert_eq!(
        queue.take(),
        PendingTake::Ops(vec![
            Op::Append(key("s"), "😀".into()),
            Op::Set(key("n"), JsonValue::Number(1.0))
        ])
    );
    let many: Vec<_> = (0..10_001)
        .map(|i| Op::Set(vec![Segment::Index(i)], JsonValue::Null))
        .collect();
    queue.push(&many);
    assert_eq!(queue.size(), 10_001);
    queue.push(&[Op::Delete(key("x"))]);
    assert_eq!(queue.size(), 1);
    assert_eq!(queue.take(), PendingTake::Replace);
    queue.push(&many);
    queue.push(&[Op::Delete(key("x"))]);
    queue.push(&[Op::Replace(JsonValue::Null)]);
    assert_eq!(
        queue.take(),
        PendingTake::Ops(vec![Op::Replace(JsonValue::Null)])
    );
    Ok(())
}
#[test]
fn seeded_utf16_ops_and_wire_replay_match_an_independent_string_model() -> Result<()> {
    let mut state = 7u32;
    let mut model = Vec::<u16>::new();
    let mut value = JsonValue::parse(r#"{"s":""}"#)?;
    let mut encoder = Encoder::default();
    let mut decoder = Decoder::default();
    let mut queue = PendingOps::default();
    let mut slow = value.clone();
    for i in 0..10_000 {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let op = if state & 3 == 0 {
            let n = (state as usize >> 16) % 5;
            model.drain(..n.min(model.len()));
            Op::Truncate(key("s"), n)
        } else {
            let unit = (state >> 16) as u16;
            model.push(unit);
            Op::Append(key("s"), JsonString(vec![unit]))
        };
        queue.push(std::slice::from_ref(&op));
        let decoded = decoder.decode(&encoder.encode(&[op])?)?;
        value = apply_immutable(&value, &decoded)?;
        assert_eq!(
            value.get("s"),
            Some(&JsonValue::String(JsonString(model.clone())))
        );
        if i % 23 == 0 {
            if let PendingTake::Ops(ops) = queue.take() {
                slow = apply_immutable(&slow, &ops)?;
            }
            assert_eq!(slow, value);
        }
    }
    if let PendingTake::Ops(ops) = queue.take() {
        slow = apply_immutable(&slow, &ops)?;
    }
    assert_eq!(slow, value);
    Ok(())
}
