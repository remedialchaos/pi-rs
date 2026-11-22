use pi_rs_chord::{Result, contracts::*, json::JsonValue};

#[test]
fn schema_snapshots_sort_keys_drop_only_annotations_and_reject_open_arguments() -> Result<()> {
    let source = JsonValue::parse(
        r#"{"description":"ignored","type":"object","additionalProperties":false,"required":["z","a"],"properties":{"z":{"type":"bytes","minLength":0},"a":{"type":"string","enum":["z","a"],"default":{"title":"kept","b":2,"a":1}}}}"#,
    )?;
    let snapshot = snapshot_arguments(&source, "call.args")?;
    assert!(snapshot.bytes);
    assert!(snapshot.schema.get("description").is_some());
    assert!(snapshot.normal.get("description").is_none());
    assert_eq!(
        snapshot.normal.to_json()?,
        r#"{"additionalProperties":false,"properties":{"a":{"default":{"a":1,"b":2,"title":"kept"},"enum":["a","z"],"type":"string"},"z":{"minLength":0,"type":"bytes"}},"required":["a","z"],"type":"object"}"#
    );
    for (schema, problem) in [
        (r#"{"type":"object"}"#, "argument objects must be closed"),
        (r#"{"type":"bytes","pattern":"x"}"#, "unsupported keyword"),
        (r#"{"type":"bytes","minLength":0.5}"#, "bytes bounds"),
        (r#"{"type":["string","null"]}"#, "unsupported type"),
        (r#"{"oneOf":[]}"#, "unsupported keyword"),
    ] {
        let error = snapshot_arguments(&JsonValue::parse(schema)?, "call.args")
            .err()
            .ok_or_else(|| pi_rs_chord::Error::Invalid("schema was accepted".into()))?;
        assert!(error.to_string().contains(problem), "{error}");
    }
    assert!(
        snapshot_arguments(
            &JsonValue::parse(r#"{"type":"object","patternProperties":{".*":{"type":"number"}}}"#)?,
            "record"
        )
        .is_ok()
    );
    Ok(())
}

#[test]
fn contract_fingerprints_and_host_keys_preserve_identity_order_and_ignore_forged_keys() -> Result<()>
{
    assert_eq!(hash64(""), "cbf29ce4811c9dc5");
    assert_eq!(hash64("hello"), "6e750d184f9f2cab");
    let params = read_params(
        &[
            ("id".into(), JsonValue::parse(r#"{"type":"string"}"#)?),
            ("n".into(), JsonValue::parse(r#"{"type":"integer"}"#)?),
            ("enabled".into(), JsonValue::parse(r#"{"type":"boolean"}"#)?),
        ],
        "session.params",
    )?;
    let kind = HostKind::new("session", params)?;
    let address = kind.address(JsonValue::parse(r#"{"id":"a|b\\c","n":7,"enabled":true}"#)?)?;
    assert_eq!(address.key, r#"session|a\|b\\c|7|true"#);
    assert_eq!(kind.address_of_key(&address.key)?, address);
    let wire = JsonValue::parse(
        r#"{"kind":"session","params":{"id":"a|b\\c","n":7,"enabled":true},"key":"forged"}"#,
    )?;
    assert_eq!(kind.key_of_address(&wire)?, Some(address.key));
    assert!(
        kind.key_of_address(&JsonValue::parse(
            r#"{"kind":"session","params":{"id":"a","n":0.5,"enabled":true}}"#
        )?)?
        .is_none()
    );
    assert!(
        read_params(
            &[(
                "x".into(),
                JsonValue::parse(r#"{"type":"string","default":"no"}"#)?
            )],
            "x"
        )
        .is_err()
    );
    assert!(HostKind::new("client", read_params(&[], "x")?).is_err());
    assert!(check_name("My:Object", "name").is_err());
    Ok(())
}
