//! Provider dispatch: lazy per-version validation and direct old/current mappings.
use crate::{Error, Result, context::lock, contracts::*, data::ChordError};
use indexmap::IndexMap;
pub use pi_rs_schema::Value;
use std::sync::Mutex;
struct Entry {
    schema: Snapshot,
    mapping: Option<MethodMapping>,
    validator: Mutex<Option<pi_rs_schema::Validator>>,
}
pub struct Provider {
    contract: Contract,
    accepted: Vec<AcceptedVersion>,
    accepts: AcceptsTable,
    table: IndexMap<u64, IndexMap<String, Entry>>,
}
pub enum CheckedArgs {
    Method {
        args: Value,
        result: Option<ValueAdapter>,
    },
    Call {
        args: Value,
        call: CallReplacement,
    },
}
impl Provider {
    pub fn contract(&self) -> &Contract {
        &self.contract
    }
    pub fn accepted(&self) -> &[AcceptedVersion] {
        &self.accepted
    }
    pub fn accepts(&self) -> &AcceptsTable {
        &self.accepts
    }
    pub fn bind(contract: Contract, accepts: AcceptsTable) -> Result<Self> {
        let accepted = check_accepts(&contract, &accepts)?;
        let mut table = IndexMap::new();
        for version in contract.versions() {
            if version.version != contract.version() && !accepts.contains_key(&version.version) {
                continue;
            }
            let mappings = accepts.get(&version.version);
            let mut entries = IndexMap::new();
            for variant in version.variants.values() {
                for (member, method) in &variant.methods {
                    entries.insert(
                        member.clone(),
                        Entry {
                            schema: method.args.clone(),
                            mapping: mappings.and_then(|m| m.methods.get(member)).cloned(),
                            validator: Mutex::new(None),
                        },
                    );
                }
            }
            table.insert(version.version, entries);
        }
        Ok(Self {
            contract,
            accepted,
            accepts,
            table,
        })
    }
    pub fn binds(&self, version: u64, fingerprint: &str) -> bool {
        if version == self.contract.version() {
            fingerprint == self.contract.fingerprint()
        } else {
            self.accepted
                .iter()
                .any(|v| v.version == version && v.fingerprint == fingerprint)
        }
    }
    pub fn check_args(&self, method: &str, raw: Value, bound_version: u64) -> Result<CheckedArgs> {
        let name = self.contract.name();
        let methods = self.table.get(&bound_version).ok_or_else(|| {
            ChordError::new(
                "contract_mismatch",
                format!("{name} v{bound_version} is not accepted"),
            )
        })?;
        let entry = methods.get(method).ok_or_else(|| {
            ChordError::invalid(
                "member",
                format!("{name} v{bound_version} has no method \"{method}\""),
                None,
            )
        })?;
        if matches!(entry.mapping, Some(MethodMapping::Refuse)) {
            return Err(ChordError::new(
                "contract_mismatch",
                format!("{name} v{bound_version} refuses \"{method}\""),
            )
            .into());
        }
        let mut validator = lock(&entry.validator);
        if validator.is_none() {
            *validator = Some(
                pi_rs_schema::compile(&entry.schema.schema.to_serde()?)
                    .map_err(|e| Error::Invalid(e.to_string()))?,
            );
        }
        if let Some(validator) = validator.as_ref()
            && !validator.check(&raw)
        {
            let errors = validator.errors(&raw);
            let first = errors
                .first()
                .ok_or_else(|| Error::Invalid("validator rejected without explanation".into()))?;
            let path = if first.path.is_empty() {
                "/"
            } else {
                &first.path
            };
            return Err(ChordError::invalid(
                "args",
                format!(
                    "args{}: {}",
                    if path == "/" { "" } else { path },
                    first.message
                ),
                Some(serde_json::json!({"path":path})),
            )
            .into());
        }
        drop(validator);
        Ok(match &entry.mapping {
            Some(MethodMapping::Call(call)) => CheckedArgs::Call {
                args: raw,
                call: call.clone(),
            },
            Some(MethodMapping::Adapt { args, result }) => CheckedArgs::Method {
                args: match args {
                    Some(map) => map(raw)?,
                    None => raw,
                },
                result: result.clone(),
            },
            _ => CheckedArgs::Method {
                args: raw,
                result: None,
            },
        })
    }
}
