use pi_rs_chord::{delta::*, json::JsonValue};
fn j(text: &str) -> JsonValue {
    // INFALLIBLE: every caller supplies a literal or formatted JSON fixture authored in this test.
    serde_json::from_str(text).unwrap_or_else(|e| unreachable!("invalid fixture: {e}"))
}
fn p(keys: &[&str]) -> Path {
    keys.iter().map(|s| Segment::Key((*s).into())).collect()
}
fn tuples(ops: &[Op]) -> JsonValue {
    JsonValue::Array(ops.iter().map(Op::to_json).collect())
}
#[test]
fn draft_folds_introduced_subtrees_and_preserves_delete_readd_order()
-> Result<(), Box<dyn std::error::Error>> {
    let tracker = Tracker::new(j(r#"{"first":1,"second":2,"rows":[]}"#))?;
    let mut change = tracker.begin_change();
    change.delete(&p(&["first"]))?;
    change.set(&p(&["first"]), j("1"))?;
    change.splice(&p(&["rows"]), 0, 0, vec![j(r#"{"text":"new"}"#)])?;
    change.append_text(
        &[
            Segment::Key("rows".into()),
            Segment::Index(0),
            Segment::Key("text".into()),
        ],
        " block".into(),
    )?;
    let prepared = change.prepare()?;
    assert_eq!(
        tuples(&prepared.ops),
        j(r#"[["d",["first"]],["s",["first"],1],["p",["rows"],0,0,[{"text":"new block"}]]]"#)
    );
    assert_eq!(
        serde_json::to_string(&prepared.value.to_json())?,
        r#"{"second":2,"rows":[{"text":"new block"}],"first":1}"#
    );
    assert_eq!(
        apply(prepared.base.to_json(), &prepared.ops)?,
        prepared.value.to_json()
    );
    let tracker = Tracker::new(j(r#"{"label":"x"}"#))?;
    let mut change = tracker.begin_change();
    change.set(&p(&["2"]), j("2"))?;
    change.set(&p(&["1"]), j("1"))?;
    let JsonValue::Object(view) = change.value()?.to_json() else {
        return Err("object fixture".into());
    };
    assert_eq!(
        view.keys().cloned().collect::<Vec<_>>(),
        vec!["1".into(), "2".into(), "label".into()]
    );
    Ok(())
}
#[test]
fn draft_moves_then_edits_retained_descendants_using_final_paths()
-> Result<(), Box<dyn std::error::Error>> {
    let tracker = Tracker::new(j(r#"{"rows":[{"n":1},{"n":2},{"n":3},{"n":4}]}"#))?;
    let mut change = tracker.begin_change();
    change.permute(&p(&["rows"]), vec![2, 0, 3, 1])?;
    change.splice(&p(&["rows"]), 2, 1, vec![j(r#"{"n":9}"#)])?;
    change.set(
        &[
            Segment::Key("rows".into()),
            Segment::Index(0),
            Segment::Key("n".into()),
        ],
        j("7"),
    )?;
    let prepared = change.prepare()?;
    assert_eq!(
        tuples(&prepared.ops),
        j(
            r#"[["p",["rows"],3,1,[]],["m",["rows"],[2,0,1]],["p",["rows"],2,0,[{"n":9}]],["s",["rows",0,"n"],7]]"#
        )
    );
    assert_eq!(
        apply(prepared.base.to_json(), &prepared.ops)?,
        j(r#"{"rows":[{"n":7},{"n":1},{"n":9},{"n":2}]}"#)
    );
    Ok(())
}
#[test]
fn draft_dense_region_suppresses_nested_operations_and_reserved_paths_fold_safely()
-> Result<(), Box<dyn std::error::Error>> {
    let tracker = Tracker::new(j(&format!(
        r#"{{"rows":[{}]}}"#,
        (0..700)
            .map(|n| format!(r#"{{"n":{n}}}"#))
            .collect::<Vec<_>>()
            .join(",")
    )))?;
    let mut change = tracker.begin_change();
    for i in 200..500 {
        change.set(
            &[
                Segment::Key("rows".into()),
                Segment::Index(i),
                Segment::Key("n".into()),
            ],
            j("-1"),
        )?;
    }
    change.set(
        &[
            Segment::Key("rows".into()),
            Segment::Index(0),
            Segment::Key("n".into()),
        ],
        j("-2"),
    )?;
    let prepared = change.prepare()?;
    assert_eq!(prepared.ops.len(), 2);
    assert!(
        matches!(&prepared.ops[0],Op::Splice(path,200,300,items) if *path==p(&["rows"])&&items.len()==300)
    );
    assert_eq!(
        apply(prepared.base.to_json(), &prepared.ops)?,
        prepared.value.to_json()
    );
    let tracker = Tracker::new(j(r#"{"safe":{"__proto__":{"n":1}},"other":0}"#))?;
    let mut change = tracker.begin_change();
    change.set(&p(&["safe", "__proto__", "n"]), j("2"))?;
    let prepared = change.prepare()?;
    assert_eq!(
        tuples(&prepared.ops),
        j(r#"[["s",["safe"],{"__proto__":{"n":2}}]]"#)
    );
    assert_eq!(
        apply(prepared.base.to_json(), &prepared.ops)?,
        prepared.value.to_json()
    );
    Ok(())
}
#[test]
fn draft_replay_survives_repeated_mixed_array_edits_and_reused_allocation_addresses()
-> Result<(), Box<dyn std::error::Error>> {
    let tracker = Tracker::new(j(r#"{"rows":[{"n":0},{"n":1},{"n":2}],"text":""}"#))?;
    let mut seed = 193u64;
    for _ in 0..150 {
        let mut change = tracker.begin_change();
        for step in 0..30 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let len = change
                .value()?
                .at(&p(&["rows"]))?
                .elements()
                .ok_or("array fixture")?
                .len();
            match (seed >> 32) % 5 {
                0 => change.splice(
                    &p(&["rows"]),
                    len / 2,
                    0,
                    vec![j(&format!(r#"{{"n":{step}}}"#))],
                )?,
                1 if len > 0 => change.splice(&p(&["rows"]), len / 2, 1, vec![])?,
                2 if len > 0 => change.set(
                    &[
                        Segment::Key("rows".into()),
                        Segment::Index(len / 2),
                        Segment::Key("n".into()),
                    ],
                    j("9"),
                )?,
                3 => change.permute(&p(&["rows"]), (0..len).rev().collect())?,
                _ => change.append_text(&p(&["text"]), "x".into())?,
            }
        }
        let expected = change.value()?.to_json();
        let prepared = change.prepare()?;
        assert_eq!(apply(prepared.base.to_json(), &prepared.ops)?, expected);
        assert_eq!(prepared.value.to_json(), expected);
        tracker.adopt(&prepared)?;
    }
    Ok(())
}
#[test]
fn settled_change_releases_base_but_prepared_keeps_immutable_snapshots()
-> Result<(), Box<dyn std::error::Error>> {
    let tracker = Tracker::new(j(r#"{"large":[1,2,3]}"#))?;
    let weak = tracker.value().downgrade();
    let mut change = tracker.begin_change();
    change.set(&p(&["large"]), j("[]"))?;
    let prepared = change.prepare()?;
    tracker.adopt(&prepared)?;
    assert!(weak.upgrade().is_some());
    drop(prepared);
    assert!(weak.upgrade().is_none());
    assert!(change.value().is_err());
    let mut abandoned = tracker.begin_change();
    assert!(abandoned.apply(Op::Replace(j("null"))).is_err());
    abandoned.abort();
    let weak = tracker.value().downgrade();
    drop(tracker);
    assert!(weak.upgrade().is_none());
    Ok(())
}
