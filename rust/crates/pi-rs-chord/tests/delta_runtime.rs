use indexmap::IndexMap;
use pi_rs_chord::{
    delta::*,
    json::{JsonString, JsonValue},
};
fn j(s: &str) -> JsonValue {
    // INFALLIBLE: callers supply authored literal JSON fixtures.
    serde_json::from_str(s).unwrap_or_else(|e| unreachable!("invalid fixture: {e}"))
}
fn p(names: &[&str]) -> Path {
    names.iter().map(|s| Segment::Key((*s).into())).collect()
}
#[test]
fn diff_canonical_sets_deletes_utf16_overlap_and_array_gaps()
-> Result<(), Box<dyn std::error::Error>> {
    for (before, after, expected) in [
        (
            r#"{"keep":1,"change":1,"remove":true}"#,
            r#"{"keep":1,"change":2,"add":3}"#,
            r#"[["s",["change"],2],["s",["add"],3],["d",["remove"]]]"#,
        ),
        (
            r#"{"text":"hello"}"#,
            r#"{"text":"hello world"}"#,
            r#"[["a",["text"]," world"]]"#,
        ),
        (
            r#"{"text":"abcdefgh"}"#,
            r#"{"text":"defghxyz"}"#,
            r#"[["t",["text"],3],["a",["text"],"xyz"]]"#,
        ),
        (
            r#"{"values":[1,2,3]}"#,
            r#"{"values":[2,3,4]}"#,
            r#"[["p",["values"],0,1,[]],["p",["values"],2,0,[4]]]"#,
        ),
        (
            r#"{"values":[1,2,3]}"#,
            r#"{"values":[3,1,2]}"#,
            r#"[["m",["values"],[2,0,1]]]"#,
        ),
        (
            r#"{"values":[{"id":1,"label":"one"},{"id":2,"label":"two"}]}"#,
            r#"{"values":[{"id":1,"label":"one"},{"id":2,"label":"changed"}]}"#,
            r#"[["s",["values",1,"label"],"changed"]]"#,
        ),
        (
            r#"{"__proto__":1,"x":1}"#,
            r#"{"__proto__":1,"x":2}"#,
            r#"[["r",{"__proto__":1,"x":2}]]"#,
        ),
    ] {
        let (before, after) = (j(before), j(after));
        let ops = diff_revisions(&before, &after)?;
        assert_eq!(
            JsonValue::Array(ops.iter().map(Op::to_json).collect()),
            j(expected)
        );
        assert_eq!(apply(before, &ops)?, after);
    }
    Ok(())
}
#[test]
fn diff_bounds_4096_operations_and_large_payload_cost_with_root_replacement()
-> Result<(), Box<dyn std::error::Error>> {
    let before = JsonValue::Object(
        (0..20_000)
            .map(|i| (JsonString::from(format!("field{i}")), j("0")))
            .collect(),
    );
    let after = JsonValue::Object(
        (0..20_000)
            .map(|i| (JsonString::from(format!("field{i}")), j("1")))
            .collect(),
    );
    assert_eq!(diff_revisions(&before, &after)?, vec![Op::Replace(after)]);
    let before = j(r#"{"values":[]}"#);
    let after = JsonValue::Object(IndexMap::from([(
        "values".into(),
        JsonValue::Array(vec![j("1"); 40_000]),
    )]));
    assert_eq!(diff_revisions(&before, &after)?, vec![Op::Replace(after)]);
    Ok(())
}
#[test]
fn tracker_adopts_only_owned_current_prepared_and_revokes_competitors()
-> Result<(), Box<dyn std::error::Error>> {
    let tracker = Tracker::new(j(r#"{"changed":{"n":1},"retained":{"n":2}}"#))?;
    let base = tracker.value();
    let mut winner = tracker.begin_change();
    let mut loser = tracker.begin_change();
    winner.set(&p(&["changed", "n"]), j("3"))?;
    let prepared = winner.prepare()?;
    assert!(!prepared.value.ptr_eq(&base));
    assert!(
        prepared
            .value
            .at(&p(&["retained"]))?
            .ptr_eq(base.at(&p(&["retained"]))?)
    );
    assert_eq!(
        base.to_json(),
        j(r#"{"changed":{"n":1},"retained":{"n":2}}"#)
    );
    tracker.adopt(&prepared)?;
    assert_eq!(tracker.revision(), 1);
    assert!(tracker.value().ptr_eq(&prepared.value));
    assert!(loser.set(&p(&["changed", "n"]), j("9")).is_err());
    assert!(tracker.adopt(&prepared).is_err());
    let foreign = Tracker::new(j("{}"))?;
    let other = foreign.prepare_replace(j("{}"))?;
    assert!(tracker.adopt(&other).is_err());
    let aborted = tracker.prepare_replace(j("{}"))?;
    aborted.abort();
    assert!(tracker.adopt(&aborted).is_err());
    Ok(())
}
#[test]
fn tracker_noop_and_append_text_keep_base_identity_and_settled_drafts_unusable()
-> Result<(), Box<dyn std::error::Error>> {
    let tracker = Tracker::new(j(r#"{"text":"hello","x":1}"#))?;
    let base = tracker.value();
    let noop = tracker.prepare_replace(base.to_json())?;
    assert!(noop.ops.is_empty());
    assert!(noop.value.ptr_eq(&base));
    tracker.adopt(&noop)?;
    let mut change = tracker.begin_change();
    change.append_text(&p(&["text"]), " world".into())?;
    change.append_text(&p(&["text"]), "!".into())?;
    let prepared = change.prepare()?;
    assert_eq!(
        prepared.ops,
        vec![Op::Append(p(&["text"]), " world!".into())]
    );
    assert!(change.set(&p(&["x"]), j("2")).is_err());
    change.abort();
    assert!(tracker.adopt(&prepared).is_err());
    assert_eq!(tracker.value().to_json(), base.to_json());
    Ok(())
}
#[test]
fn shared_revision_permutations_sparse_edits_and_cached_validation()
-> Result<(), Box<dyn std::error::Error>> {
    let tracker = Tracker::new(j(r#"{"values":[{"n":1},{"n":2},{"n":3}]}"#))?;
    let base = tracker.value();
    let mut change = tracker.begin_change();
    change.permute(&p(&["values"]), vec![2, 0, 1])?;
    let prepared = change.prepare()?;
    assert_eq!(
        prepared.ops,
        vec![Op::Permute(p(&["values"]), vec![2, 0, 1])]
    );
    let mut validator = JsonRevisionValidator::default();
    assert!(validator.validate(&base).is_ok());
    assert!(validator.validate(&prepared.value).is_ok());
    assert!(Revision::from_json(JsonValue::Number(f64::NAN)).is_err());
    assert_eq!(
        apply(base.to_json(), &prepared.ops)?,
        prepared.value.to_json()
    );
    Ok(())
}
#[test]
fn seeded_diff_replay_matches_independent_generated_revisions()
-> Result<(), Box<dyn std::error::Error>> {
    let mut seed = 1234567u64;
    for _ in 0..1200 {
        let mut arrays = Vec::new();
        for _ in 0..2 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let len = (seed >> 32) % 20;
            let mut a = Vec::new();
            for _ in 0..len {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                a.push(JsonValue::Number(((seed >> 32) % 10) as f64));
            }
            arrays.push(JsonValue::Array(a));
        }
        let ops = diff_revisions(&arrays[0], &arrays[1])?;
        assert_eq!(apply(arrays[0].clone(), &ops)?, arrays[1]);
    }
    Ok(())
}
