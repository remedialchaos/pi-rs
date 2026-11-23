//! Shared p2 plugin wire declarations, recorded from upstream Chord's contractData.
//! Schema indexes are local to each declaration. Chord snapshots and fingerprints
//! each version as it is built, including edits that leave the wire shape identical.

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    contracts::{Contract, HostKind, Member, MemberKind, read_params, schema},
    json::JsonValue,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Route {
    Server,
    Session,
}
impl Route {
    fn host(&self) -> Result<HostKind> {
        let (name, params) = match self {
            Self::Server => ("server", vec![]),
            Self::Session => ("session", vec![("id".into(), schema::str())]),
        };
        HostKind::new(name, read_params(&params, name)?)
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum WireMember {
    State(usize),
    Event(usize),
    Method(usize, usize),
    Keyed(usize, Option<Route>),
    Queries(usize, Vec<(String, usize)>),
}
type Members = Vec<(String, WireMember)>;
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum RemovedKind {
    State,
    Methods,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Edit {
    Put(String, String, WireMember, bool),
    Remove(String, String, RemovedKind),
    Args(String, usize),
    Result(String, usize),
    Method(String, usize, usize),
}
#[derive(Deserialize)]
struct Declaration {
    version: u64,
    params: Vec<(String, usize)>,
    route: Option<Route>,
    variants: Vec<(String, Members)>,
    history: Vec<Vec<Edit>>,
    admission: Option<usize>,
    schemas: Vec<Value>,
}
include!(concat!(env!("OUT_DIR"), "/p2_contract_index.rs"));

/// Builds the named declaration and its upstream compatibility history.
pub fn lookup(name: &str) -> Result<Contract> {
    let source = declaration_source(name)
        .ok_or_else(|| Error::Invalid(format!("unknown p2 contract: {name}")))?;
    let declaration: Declaration =
        serde_json::from_str(source).map_err(|e| Error::Invalid(format!("{name}: {e}")))?;
    let schema = |i: usize| {
        declaration
            .schemas
            .get(i)
            .cloned()
            .map(JsonValue::from_serde)
            .ok_or_else(|| Error::Invalid(format!("{name}: unknown schema {i}")))
    };
    let member = |wire: WireMember| -> Result<Member> {
        Ok(match wire {
            WireMember::State(i) => Member::State(schema(i)?),
            WireMember::Event(i) => Member::Event(schema(i)?),
            WireMember::Method(a, r) => Member::Method {
                args: schema(a)?,
                result: schema(r)?,
            },
            WireMember::Keyed(i, key) => Member::Keyed {
                schema: schema(i)?,
                key: key.map(|k| k.host()).transpose()?,
            },
            WireMember::Queries(i, args) => Member::Queries {
                item: schema(i)?,
                args: args
                    .into_iter()
                    .map(|(k, i)| Ok((k, schema(i)?)))
                    .collect::<Result<_>>()?,
            },
        })
    };
    let params = declaration
        .params
        .into_iter()
        .map(|(k, i)| Ok((k, schema(i)?)))
        .collect::<Result<_>>()?;
    let mut variants = IndexMap::new();
    for (v, members) in declaration.variants {
        variants.insert(
            v,
            members
                .into_iter()
                .map(|(k, m)| Ok((k, member(m)?)))
                .collect::<Result<_>>()?,
        );
    }
    let mut contract = if let Some(route) = declaration.route {
        let host = route.host()?;
        Contract::remote(
            name,
            declaration.version,
            params,
            variants,
            Arc::new(move |params| {
                let fields = match route {
                    Route::Server => IndexMap::new(),
                    Route::Session => IndexMap::from([(
                        "id".into(),
                        params
                            .get("session")
                            .cloned()
                            .ok_or_else(|| Error::Invalid("missing session".into()))?,
                    )]),
                };
                host.address(JsonValue::Object(fields))
            }),
        )?
    } else {
        Contract::local(
            name,
            declaration.version,
            params,
            variants
                .shift_remove("members")
                .ok_or_else(|| Error::Invalid(format!("{name}: missing members")))?,
        )?
    };
    for edits in declaration.history {
        contract = contract.evolve(|editor| {
            for edit in edits {
                match edit {
                    Edit::Put(v, k, m, add) => editor.put(&v, &k, member(m)?, add)?,
                    Edit::Remove(v, k, kind) => editor.remove(
                        &v,
                        &k,
                        match kind {
                            RemovedKind::State => MemberKind::State,
                            RemovedKind::Methods => MemberKind::Method,
                        },
                    )?,
                    Edit::Args(k, i) => editor.method_args(&k, schema(i)?)?,
                    Edit::Result(k, i) => editor.method_result(&k, schema(i)?)?,
                    Edit::Method(k, a, r) => editor.method(&k, schema(a)?, schema(r)?)?,
                };
            }
            Ok(())
        })?;
    }
    if let Some(i) = declaration.admission {
        contract = contract.with_admission(schema(i)?)?;
    }
    Ok(contract)
}
