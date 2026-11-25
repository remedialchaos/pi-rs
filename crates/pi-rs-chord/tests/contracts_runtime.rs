use indexmap::IndexMap;
use pi_rs_chord::{
    contracts::{schema::*, *},
    data::args::*,
    json::JsonValue,
};
use std::sync::Arc;
fn j(source: &str) -> JsonValue {
    // INFALLIBLE: callers supply authored literal JSON fixtures.
    serde_json::from_str(source).unwrap_or_else(|e| unreachable!("invalid fixture: {e}"))
}
fn members() -> IndexMap<String, Member> {
    IndexMap::from([
        ("count".into(), Member::State(int())),
        ("add".into(), method(Some(int()), Some(str()))),
        ("clear".into(), method(None, None)),
    ])
}
#[test]
fn schema_builders_closed_objects_optionals_records_bounds_and_literals() -> pi_rs_chord::Result<()>
{
    let object = obj([("a", required(str())), ("b", opt(int()))]);
    assert_eq!(
        object,
        j(
            r#"{"type":"object","required":["a"],"properties":{"a":{"type":"string"},"b":{"type":"integer"}},"additionalProperties":false}"#
        )
    );
    assert_eq!(
        bounded(
            str(),
            Bounds {
                min: Some(1.0),
                max: Some(3.0)
            }
        )?,
        j(r#"{"type":"string","minLength":1,"maxLength":3}"#)
    );
    assert_eq!(
        one_of([j("3"), j("true"), j(r#""a""#)])?,
        j(
            r#"{"anyOf":[{"type":"number","const":3},{"type":"boolean","const":true},{"type":"string","const":"a"}]}"#
        )
    );
    assert!(snapshot_arguments(&map(bytes()), "map")?.bytes);
    assert!(snapshot_arguments(&j(r#"{"type":"object"}"#), "args").is_err());
    Ok(())
}
#[test]
fn declarations_materialize_owned_history_reserved_kinds_and_annotation_free_hashes()
-> pi_rs_chord::Result<()> {
    let first = Contract::local("counter", 1, vec![], members())?;
    assert_eq!(first.kind(), ContractKind::Local);
    assert_eq!(first.version(), 1);
    let expected = j(
        r#"{"kind":"local","params":[],"members":{"add":{"args":{"type":"integer"},"result":{"type":"string"}},"clear":{"args":{"type":"null"},"result":{"type":"null"}},"count":{"state":{"type":"integer"}}}}"#,
    );
    assert_eq!(first.fingerprint(), fingerprint_of(&expected)?);
    assert_eq!(first.member_names(None)?.methods, vec!["add", "clear"]);
    assert!(takes_no_args(
        &first.current().variants[LOCAL].methods["clear"]
    ));
    let mut bad = members();
    bad.insert("then".into(), Member::State(str()));
    assert!(Contract::local("bad", 1, vec![], bad).is_err());
    assert!(Contract::local("bad", 0, vec![], members()).is_err());
    assert!(
        Contract::local(
            "bad",
            1,
            vec![],
            IndexMap::from([("e".into(), Member::Event(bytes()))])
        )
        .is_err()
    );
    let removed = first.evolve(|e| {
        e.remove(LOCAL, "count", MemberKind::State)?;
        Ok(())
    })?;
    assert!(
        removed
            .evolve(|e| {
                e.put(LOCAL, "count", method(None, None), true)?;
                Ok(())
            })
            .is_err()
    );
    assert_eq!(first.member_names(None)?.state, vec!["count"]);
    Ok(())
}
#[test]
fn accepts_require_edit_history_not_schema_difference_and_exact_mapping_keys()
-> pi_rs_chord::Result<()> {
    let v1 = Contract::local("counter", 1, vec![], members())?;
    let v2 = v1.evolve(|e| {
        e.put(LOCAL, "count", Member::State(int()), false)?;
        e.method_args("add", str())?;
        Ok(())
    })?;
    assert_ne!(v1.fingerprint(), v2.fingerprint());
    assert!(check_accepts(&v2, &IndexMap::from([(1, VersionMappings::default())])).is_err());
    let mappings = VersionMappings {
        state: IndexMap::from([(
            (LOCAL.into(), "count".into()),
            ValueMapping::Map(Arc::new(Ok)),
        )]),
        methods: IndexMap::from([(
            "add".into(),
            MethodMapping::Adapt {
                args: Some(Arc::new(Ok)),
                result: None,
            },
        )]),
        ..Default::default()
    };
    let accepted = check_accepts(&v2, &IndexMap::from([(1, mappings.clone())]))?;
    assert_eq!(accepted[0].fingerprint, v1.fingerprint());
    let mut unnecessary = mappings.clone();
    unnecessary
        .events
        .insert((LOCAL.into(), "extra".into()), ValueMapping::Refuse);
    assert!(check_accepts(&v2, &IndexMap::from([(1, unnecessary)])).is_err());
    assert!(check_accepts(&v2, &IndexMap::from([(2, mappings)])).is_err());
    Ok(())
}
#[test]
fn dispatch_checks_bound_schema_maps_once_refuses_unknown_and_binds_fingerprints()
-> pi_rs_chord::Result<()> {
    let v1 = Contract::local("counter", 1, vec![], members())?;
    let v2 = v1.evolve(|e| {
        e.method_args("add", str())?;
        e.remove_method("clear")?;
        Ok(())
    })?;
    let table = IndexMap::from([(
        1,
        VersionMappings {
            methods: IndexMap::from([
                (
                    "add".into(),
                    MethodMapping::Adapt {
                        args: Some(Arc::new(|v| {
                            Ok(pi_rs_schema::Value::String(format!("{v:?}")))
                        })),
                        result: None,
                    },
                ),
                ("clear".into(), MethodMapping::Refuse),
            ]),
            ..Default::default()
        },
    )]);
    let provider = Provider::bind(v2.clone(), table)?;
    assert!(provider.binds(1, v1.fingerprint()));
    assert!(!provider.binds(1, "wrong"));
    assert!(provider.binds(2, v2.fingerprint()));
    let checked = provider.check_args("add", pi_rs_schema::Value::Number(42.0), 1)?;
    assert!(matches!(
        checked,
        CheckedArgs::Method {
            args: pi_rs_schema::Value::String(_),
            ..
        }
    ));
    assert!(
        provider
            .check_args("add", pi_rs_schema::Value::String("bad".into()), 1)
            .is_err()
    );
    assert!(
        provider
            .check_args("clear", pi_rs_schema::Value::Null, 1)
            .is_err()
    );
    assert!(
        provider
            .check_args("absent", pi_rs_schema::Value::Null, 2)
            .is_err()
    );
    assert!(
        provider
            .check_args("add", pi_rs_schema::Value::Null, 99)
            .is_err()
    );
    Ok(())
}
#[test]
fn byte_argument_dispatch_preserves_bytes_and_checks_bounds() -> pi_rs_chord::Result<()> {
    let contract = Contract::local(
        "blob",
        1,
        vec![],
        IndexMap::from([(
            "put".into(),
            method(
                Some(bounded(
                    bytes(),
                    Bounds {
                        min: Some(1.0),
                        max: Some(2.0),
                    },
                )?),
                None,
            ),
        )]),
    )?;
    let provider = Provider::bind(contract, IndexMap::new())?;
    assert!(matches!(
        provider.check_args("put", pi_rs_schema::Value::Bytes(vec![1, 2]), 1)?,
        CheckedArgs::Method {
            args: pi_rs_schema::Value::Bytes(_),
            ..
        }
    ));
    assert!(
        provider
            .check_args("put", pi_rs_schema::Value::Bytes(vec![]), 1)
            .is_err()
    );
    assert!(
        provider
            .check_args("put", pi_rs_schema::Value::Array(vec![]), 1)
            .is_err()
    );
    Ok(())
}

#[test]
fn remote_admission_is_separate_from_wire_hash_and_failed_edits_are_atomic()
-> pi_rs_chord::Result<()> {
    let host = HostKind::new("host", read_params(&[], "host.params")?)?;
    let remote = Contract::remote(
        "remote",
        1,
        vec![],
        IndexMap::from([("ready".into(), members()), ("busy".into(), members())]),
        Arc::new(move |_| host.address(j("{}"))),
    )?;
    let fingerprint = remote.fingerprint().to_owned();
    let remote = remote.with_admission(obj([("token", required(str()))]))?;
    assert_eq!(remote.fingerprint(), fingerprint);
    assert!(remote.admission().is_some());
    assert!(remote.clone().with_admission(bytes()).is_err());
    let next = remote.evolve(|editor| {
        assert!(
            editor
                .put("ready", "add", method(Some(str()), Some(str())), false)
                .is_err()
        );
        editor.method_args("add", int())?;
        Ok(())
    })?;
    assert_eq!(next.fingerprint(), fingerprint);
    assert_eq!(next.version(), 2);
    assert_eq!(remote.version(), 1);
    assert!(next.variants_at(2)?["ready"].methods["add"].args.normal == int());
    let local = Contract::local("local", 1, vec![], members())?;
    assert!(local.with_admission(nul()).is_err());
    Ok(())
}

#[tokio::test]
async fn removed_method_call_replacement_receives_old_validated_args_and_context()
-> pi_rs_chord::Result<()> {
    let old = Contract::local("counter", 1, vec![], members())?;
    let next = old.evolve(|editor| {
        editor.remove_method("add")?;
        Ok(())
    })?;
    let key = pi_rs_chord::context::ContextKey::<u32>::new("request");
    let used = key.clone();
    let mapping = MethodMapping::Call(Arc::new(move |args, context| {
        let used = used.clone();
        Box::pin(async move {
            assert_eq!(context.value(&used), Some(42));
            Ok(args)
        })
    }));
    let provider = Provider::bind(
        next,
        IndexMap::from([(
            1,
            VersionMappings {
                methods: IndexMap::from([("add".into(), mapping)]),
                ..Default::default()
            },
        )]),
    )?;
    assert!(
        provider
            .check_args("add", Value::String("not integer".into()), 1)
            .is_err()
    );
    let CheckedArgs::Call { args, call } = provider.check_args("add", Value::Number(7.0), 1)?
    else {
        return Err(pi_rs_chord::Error::Invalid("replacement missing".into()));
    };
    assert_eq!(
        call(
            args,
            pi_rs_chord::context::Context::background().with_value(&key, 42)
        )
        .await?,
        Value::Number(7.0)
    );
    assert_eq!(provider.accepted().len(), 1);
    assert_eq!(provider.contract().version(), 2);
    Ok(())
}
