use pi_rs_chord::{
    contracts::{hash64, variant_names},
    data::Validator,
    json::JsonValue,
};
use pi_rs_p2::api::v1::p2_contracts::lookup;
use serde_json::json;

// Upstream contractData() fingerprints plus a digest of each ordered variant,
// sorted member indexes and ordered change triples (which govern compatibility).
#[test]
fn every_exported_contract_matches_upstream_history() -> Result<(), Box<dyn std::error::Error>> {
    for line in include_str!("../src/api/v1/p2_contracts/parity.jsonl").lines() {
        type Expected = (String, Vec<(u64, String, String, String)>, Option<String>);
        let (name, versions, admission): Expected = serde_json::from_str(line)?;
        let contract = lookup(&name)?;
        assert_eq!(
            contract.admission().map(|a| &a.fingerprint),
            admission.as_ref(),
            "{name}"
        );
        assert_eq!(contract.versions().len(), versions.len(), "{name}");
        for (actual, (version, fingerprint, digest, schemas)) in
            contract.versions().iter().zip(versions)
        {
            assert_eq!(
                (actual.version, &actual.fingerprint),
                (version, &fingerprint),
                "{name}"
            );
            let mut layout = vec![];
            let mut snapshots = vec![];
            for (v, data) in &actual.variants {
                for snapshot in data
                    .state
                    .values()
                    .chain(data.methods.values().flat_map(|m| [&m.args, &m.result]))
                    .chain(data.events.values())
                    .chain(data.keyed.values().map(|k| &k.snapshot))
                    .chain(
                        data.queries
                            .values()
                            .flat_map(|q| std::iter::once(&q.item).chain(q.args.values())),
                    )
                {
                    snapshots.push(json!([snapshot.schema.to_serde()?, snapshot.bytes]));
                }
                let names = variant_names(data);
                let mut keyed: Vec<_> = data.keyed.keys().collect();
                let mut queries: Vec<_> = data.queries.keys().collect();
                keyed.sort();
                queries.sort();
                layout.push(json!([
                    v,
                    names.state,
                    names.methods,
                    names.events,
                    keyed,
                    queries
                ]));
            }
            assert_eq!(
                hash64(&serde_json::to_string(&snapshots)?),
                schemas,
                "{name} v{version} schemas"
            );
            let changes: Vec<_> = actual
                .changes
                .iter()
                .map(|c| json!([c.variant, c.member, format!("{:?}", c.side).to_lowercase()]))
                .collect();
            assert_eq!(
                hash64(&serde_json::to_string(&(layout, changes))?),
                digest,
                "{name} v{version}"
            );
        }
    }
    Ok(())
}

#[test]
fn chat_routes_to_its_session_and_checks_send_arguments() -> Result<(), Box<dyn std::error::Error>>
{
    let chat = lookup("p2.chat")?;
    let address = chat.host_address(&JsonValue::from_serde(
        json!({"session":"a|b","conversation":1}),
    ))?;
    assert_eq!(address.key, r"session|a\|b");
    assert_eq!(address.params.to_serde()?, json!({"id":"a|b"}));
    assert!(
        chat.host_address(&JsonValue::from_serde(
            json!({"session":1,"conversation":1})
        ))
        .is_err()
    );
    let send = &chat.current().variants["participant"].methods["send"];
    let validator = Validator::compile(&send.args.schema.to_serde()?)?;
    validator.check(&json!({"text":"ok","requestId":"r"}), "args")?;
    assert!(validator.check(&json!({"text":"ok"}), "args").is_err());
    assert!(
        validator
            .check(&json!({"text":"ok","requestId":"r","extra":true}), "args")
            .is_err()
    );
    assert_eq!(
        lookup("p2.access")?
            .host_address(&JsonValue::from_serde(json!({})))?
            .key,
        "server"
    );
    Ok(())
}
