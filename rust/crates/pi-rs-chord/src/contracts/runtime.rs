//! Owned declarations, materialized versions and edit-driven compatibility history.
use super::*;
use std::{collections::HashSet, sync::Arc};
pub const LOCAL: &str = "members";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractKind {
    Local,
    Remote,
}
impl ContractKind {
    fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemberKind {
    State,
    Method,
    Keyed,
    Event,
    Queries,
}
impl MemberKind {
    fn name(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Method => "method",
            Self::Keyed => "keyed",
            Self::Event => "event",
            Self::Queries => "queries",
        }
    }
}
#[derive(Clone, Debug)]
pub enum Member {
    State(JsonValue),
    Method {
        args: JsonValue,
        result: JsonValue,
    },
    Keyed {
        schema: JsonValue,
        key: Option<HostKind>,
    },
    Event(JsonValue),
    Queries {
        item: JsonValue,
        args: IndexMap<String, JsonValue>,
    },
}
impl Member {
    fn kind(&self) -> MemberKind {
        match self {
            Self::State(_) => MemberKind::State,
            Self::Method { .. } => MemberKind::Method,
            Self::Keyed { .. } => MemberKind::Keyed,
            Self::Event(_) => MemberKind::Event,
            Self::Queries { .. } => MemberKind::Queries,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct MethodData {
    pub args: Snapshot,
    pub result: Snapshot,
}
#[derive(Clone, Debug, PartialEq)]
pub struct KeyedSnapshot {
    pub snapshot: Snapshot,
    pub key: Option<HostKind>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct QueriesData {
    pub item: Snapshot,
    pub args: IndexMap<String, Snapshot>,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VariantData {
    pub state: IndexMap<String, Snapshot>,
    pub methods: IndexMap<String, MethodData>,
    pub keyed: IndexMap<String, KeyedSnapshot>,
    pub events: IndexMap<String, Snapshot>,
    pub queries: IndexMap<String, QueriesData>,
}
impl VariantData {
    fn has(&self, name: &str) -> bool {
        self.state.contains_key(name)
            || self.methods.contains_key(name)
            || self.keyed.contains_key(name)
            || self.events.contains_key(name)
            || self.queries.contains_key(name)
    }
    fn contains(&self, name: &str, kind: MemberKind) -> bool {
        match kind {
            MemberKind::State => self.state.contains_key(name),
            MemberKind::Method => self.methods.contains_key(name),
            MemberKind::Keyed => self.keyed.contains_key(name),
            MemberKind::Event => self.events.contains_key(name),
            MemberKind::Queries => self.queries.contains_key(name),
        }
    }
    fn insert(&mut self, name: &str, member: Member, path: &str) -> Result<()> {
        match member {
            Member::State(schema) => {
                self.state
                    .insert(name.into(), snapshot_schema(&schema, path)?);
            }
            Member::Method { args, result } => {
                self.methods.insert(
                    name.into(),
                    MethodData {
                        args: snapshot_arguments(&args, &format!("{path}.args"))?,
                        result: snapshot_schema(&result, &format!("{path}.result"))?,
                    },
                );
            }
            Member::Keyed { schema, key } => {
                self.keyed.insert(
                    name.into(),
                    KeyedSnapshot {
                        snapshot: snapshot_schema(&schema, path)?,
                        key,
                    },
                );
            }
            Member::Event(schema) => {
                let snapshot = snapshot_schema(&schema, path)?;
                if snapshot.bytes {
                    return Err(refused(
                        path,
                        "an event's values are JSON only (no bytes())",
                    ));
                }
                self.events.insert(name.into(), snapshot);
            }
            Member::Queries { item, args } => {
                self.queries.insert(
                    name.into(),
                    QueriesData {
                        item: snapshot_schema(&item, &format!("{path}.item"))?,
                        args: args
                            .into_iter()
                            .map(|(query, schema)| {
                                Ok((
                                    query.clone(),
                                    snapshot_arguments(&schema, &format!("{path}.args.{query}"))?,
                                ))
                            })
                            .collect::<Result<_>>()?,
                    },
                );
            }
        }
        Ok(())
    }
    fn remove(&mut self, name: &str, kind: MemberKind) -> bool {
        match kind {
            MemberKind::State => self.state.shift_remove(name).is_some(),
            MemberKind::Method => self.methods.shift_remove(name).is_some(),
            MemberKind::Keyed => self.keyed.shift_remove(name).is_some(),
            MemberKind::Event => self.events.shift_remove(name).is_some(),
            MemberKind::Queries => false,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeSide {
    State,
    Args,
    Result,
    Keyed,
    Event,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Change {
    pub variant: String,
    pub member: String,
    pub side: ChangeSide,
}
#[derive(Clone, Debug)]
pub struct VersionData {
    pub version: u64,
    pub fingerprint: String,
    pub variants: IndexMap<String, VariantData>,
    pub changes: Vec<Change>,
}
pub type HostAddress = Arc<dyn Fn(&JsonValue) -> Result<Address> + Send + Sync>;
#[derive(Clone, Debug)]
pub struct AdmissionData {
    pub schema: Snapshot,
    pub fingerprint: String,
}
#[derive(Clone)]
pub struct Contract {
    kind: ContractKind,
    name: String,
    params: ParamsData,
    host_address: Option<HostAddress>,
    versions: Vec<Arc<VersionData>>,
    current: Arc<VersionData>,
    kinds: IndexMap<String, MemberKind>,
    admission: Option<AdmissionData>,
}
fn sorted_dict(entries: impl IntoIterator<Item = (String, JsonValue)>) -> JsonValue {
    let mut entries: Vec<_> = entries.into_iter().collect();
    entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    object(entries.into_iter().map(|(key, value)| (key.into(), value)))
}
fn variant_normal(variant: &VariantData) -> JsonValue {
    sorted_dict(
        variant
            .state
            .iter()
            .map(|(member, snapshot)| {
                (
                    member.clone(),
                    object([("state".into(), snapshot.normal.clone())]),
                )
            })
            .chain(variant.methods.iter().map(|(member, method)| {
                (
                    member.clone(),
                    object([
                        ("args".into(), method.args.normal.clone()),
                        ("result".into(), method.result.normal.clone()),
                    ]),
                )
            }))
            .chain(variant.keyed.iter().map(|(member, keyed)| {
                let mut fields = vec![("keyed".into(), keyed.snapshot.normal.clone())];
                if let Some(key) = &keyed.key {
                    fields.push(("key".into(), JsonValue::String(key.kind.as_str().into())));
                }
                (member.clone(), object(fields))
            }))
            .chain(variant.events.iter().map(|(member, snapshot)| {
                (
                    member.clone(),
                    object([("event".into(), snapshot.normal.clone())]),
                )
            }))
            .chain(variant.queries.iter().map(|(member, queries)| {
                (
                    member.clone(),
                    object([
                        ("queries".into(), queries.item.normal.clone()),
                        (
                            "args".into(),
                            sorted_dict(
                                queries
                                    .args
                                    .iter()
                                    .map(|(query, schema)| (query.clone(), schema.normal.clone())),
                            ),
                        ),
                    ]),
                )
            })),
    )
}
pub fn materialize(
    kind: ContractKind,
    params: &ParamsData,
    version: u64,
    variants: IndexMap<String, VariantData>,
    changes: Vec<Change>,
) -> Result<VersionData> {
    let mut normal = vec![
        ("kind".into(), JsonValue::String(kind.name().into())),
        ("params".into(), params.normal.clone()),
    ];
    normal.push(match kind {
        ContractKind::Local => (
            "members".into(),
            variant_normal(
                variants
                    .get(LOCAL)
                    .ok_or_else(|| refused("contract", "missing local variant"))?,
            ),
        ),
        ContractKind::Remote => (
            "variants".into(),
            sorted_dict(
                variants
                    .iter()
                    .map(|(name, variant)| (name.clone(), variant_normal(variant))),
            ),
        ),
    });
    Ok(VersionData {
        version,
        fingerprint: fingerprint_of(&object(normal))?,
        variants,
        changes,
    })
}
fn check_members(
    variants: &IndexMap<String, VariantData>,
    kinds: &mut IndexMap<String, MemberKind>,
    base: &str,
) -> Result<()> {
    let mut signatures: IndexMap<String, (JsonValue, JsonValue)> = IndexMap::new();
    for (variant, data) in variants {
        for (kind, names) in [
            (MemberKind::State, data.state.keys().collect::<Vec<_>>()),
            (MemberKind::Method, data.methods.keys().collect()),
            (MemberKind::Keyed, data.keyed.keys().collect()),
            (MemberKind::Event, data.events.keys().collect()),
            (MemberKind::Queries, data.queries.keys().collect()),
        ] {
            for member in names {
                let path = format!("{base}.{variant}.{member}");
                if ["then", "release", "variant", "state", "retry"].contains(&member.as_str()) {
                    return Err(refused(
                        &path,
                        format!("\"{member}\" is reserved: a reference has it"),
                    ));
                }
                if let Some(previous) = kinds.get(member)
                    && previous != &kind
                {
                    return Err(refused(
                        &path,
                        format!(
                            "\"{member}\" is a {} member; its kind never changes",
                            previous.name()
                        ),
                    ));
                }
                kinds.insert(member.clone(), kind);
            }
        }
        for (member, method) in &data.methods {
            let signature = (method.args.normal.clone(), method.result.normal.clone());
            if let Some(old) = signatures.get(member)
                && old != &signature
            {
                return Err(refused(
                    &format!("{base}.{variant}.{member}"),
                    "a method has the same signature in every variant",
                ));
            }
            signatures.insert(member.clone(), signature);
        }
    }
    Ok(())
}
impl Contract {
    pub fn local(
        name: &str,
        version: u64,
        params: Vec<(String, JsonValue)>,
        members: IndexMap<String, Member>,
    ) -> Result<Self> {
        Self::declare(
            ContractKind::Local,
            name,
            version,
            params,
            IndexMap::from([(LOCAL.into(), members)]),
            None,
        )
    }
    pub fn remote(
        name: &str,
        version: u64,
        params: Vec<(String, JsonValue)>,
        variants: IndexMap<String, IndexMap<String, Member>>,
        host_address: HostAddress,
    ) -> Result<Self> {
        Self::declare(
            ContractKind::Remote,
            name,
            version,
            params,
            variants,
            Some(host_address),
        )
    }
    fn declare(
        kind: ContractKind,
        name: &str,
        version: u64,
        params: Vec<(String, JsonValue)>,
        declared: IndexMap<String, IndexMap<String, Member>>,
        host_address: Option<HostAddress>,
    ) -> Result<Self> {
        check_name(name, "A contract name")?;
        if version == 0 || version > 9_007_199_254_740_991 {
            return Err(refused(
                &format!("{name}.version"),
                "expected a positive integer",
            ));
        }
        if declared.is_empty() {
            return Err(refused(
                &format!("{name}.variants"),
                "at least one variant is required",
            ));
        }
        let mut variants = IndexMap::new();
        for (variant, members) in declared {
            let mut data = VariantData::default();
            for (member, value) in members {
                if kind == ContractKind::Remote
                    && matches!(value, Member::Keyed { .. } | Member::Queries { .. })
                {
                    return Err(refused(
                        &format!("{name}.{variant}.{member}"),
                        "keyed and queries members are only in local contracts",
                    ));
                }
                data.insert(&member, value, &format!("{name}.{variant}.{member}"))?;
            }
            variants.insert(variant, data);
        }
        let params = read_params(&params, &format!("{name}.params"))?;
        let mut kinds = IndexMap::new();
        check_members(&variants, &mut kinds, name)?;
        let current = Arc::new(materialize(kind, &params, version, variants, Vec::new())?);
        Ok(Self {
            kind,
            name: name.into(),
            params,
            host_address,
            versions: vec![current.clone()],
            current,
            kinds,
            admission: None,
        })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn kind(&self) -> ContractKind {
        self.kind
    }
    pub fn version(&self) -> u64 {
        self.current.version
    }
    pub fn fingerprint(&self) -> &str {
        &self.current.fingerprint
    }
    pub fn current(&self) -> &VersionData {
        &self.current
    }
    pub fn versions(&self) -> &[Arc<VersionData>] {
        &self.versions
    }
    pub fn params(&self) -> &ParamsData {
        &self.params
    }
    pub fn admission(&self) -> Option<&AdmissionData> {
        self.admission.as_ref()
    }
    /// Admission is a separate boundary and does not participate in the contract's wire fingerprint.
    pub fn with_admission(mut self, schema: JsonValue) -> Result<Self> {
        if self.kind != ContractKind::Remote {
            return Err(refused(self.name(), "admission is remote only"));
        }
        self.admission = Some(admission_data(schema)?);
        Ok(self)
    }
    pub fn host_address(&self, params: &JsonValue) -> Result<Address> {
        let host = self
            .host_address
            .as_ref()
            .ok_or_else(|| refused(self.name(), "a local contract has no host address"))?;
        check_params(&self.params, params, "params")?;
        host(params)
    }
    pub fn variants_at(&self, version: u64) -> Result<&IndexMap<String, VariantData>> {
        self.versions
            .iter()
            .find(|v| v.version == version)
            .map(|v| &v.variants)
            .ok_or_else(|| refused(self.name(), "unknown version"))
    }
    pub fn variant_order(&self) -> Vec<String> {
        let mut names: Vec<_> = self.current.variants.keys().cloned().collect();
        names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        names
    }
    pub fn member_names(&self, variant: Option<&str>) -> Result<MemberNames> {
        let name = variant.unwrap_or(LOCAL);
        let variant = self
            .current
            .variants
            .get(name)
            .ok_or_else(|| refused(self.name(), format!("no variant \"{name}\"")))?;
        Ok(MemberNames {
            state: variant.state.keys().cloned().collect(),
            methods: variant.methods.keys().cloned().collect(),
            events: variant.events.keys().cloned().collect(),
        })
    }
    /// A variant's state, method, and event names in canonical Chord wire order (sorted by UTF-16).
    pub fn variant_member_names(&self, variant: Option<&str>) -> Result<MemberNames> {
        let name = variant.unwrap_or(LOCAL);
        let variant = self
            .current
            .variants
            .get(name)
            .ok_or_else(|| refused(self.name(), format!("no variant \"{name}\"")))?;
        Ok(variant_names(variant))
    }
    pub fn evolve(&self, edit: impl FnOnce(&mut Editor) -> Result<()>) -> Result<Self> {
        let mut editor = Editor {
            base: self.clone(),
            variants: self.current.variants.clone(),
            kinds: self.kinds.clone(),
            changes: Vec::new(),
            admission: self.admission.clone(),
        };
        edit(&mut editor)?;
        let version = self
            .version()
            .checked_add(1)
            .filter(|v| *v <= 9_007_199_254_740_991)
            .ok_or_else(|| refused(self.name(), "version overflow"))?;
        let current = Arc::new(materialize(
            self.kind,
            &self.params,
            version,
            editor.variants,
            editor.changes,
        )?);
        let mut next = self.clone();
        next.versions.push(current.clone());
        next.current = current;
        next.kinds = editor.kinds;
        next.admission = editor.admission;
        Ok(next)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberNames {
    pub state: Vec<String>,
    pub methods: Vec<String>,
    pub events: Vec<String>,
}
pub fn variant_names(variant: &VariantData) -> MemberNames {
    let mut names = MemberNames {
        state: variant.state.keys().cloned().collect(),
        methods: variant.methods.keys().cloned().collect(),
        events: variant.events.keys().cloned().collect(),
    };
    for list in [&mut names.state, &mut names.methods, &mut names.events] {
        list.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    }
    names
}
pub fn takes_no_args(method: &MethodData) -> bool {
    matches!(&method.args.normal,JsonValue::Object(map) if map.len()==1 && method.args.normal.get("type")==Some(&JsonValue::String("null".into())))
}
pub struct Editor {
    base: Contract,
    variants: IndexMap<String, VariantData>,
    kinds: IndexMap<String, MemberKind>,
    changes: Vec<Change>,
    admission: Option<AdmissionData>,
}
impl Editor {
    fn variant(&mut self, variant: &str) -> Result<&mut VariantData> {
        self.variants
            .get_mut(variant)
            .ok_or_else(|| refused(variant, "unknown variant (the variant set never changes)"))
    }
    pub fn put(
        &mut self,
        variant: &str,
        member: &str,
        value: Member,
        add: bool,
    ) -> Result<&mut Self> {
        if self.base.kind == ContractKind::Remote
            && matches!(value, Member::Keyed { .. } | Member::Queries { .. })
        {
            return Err(refused(member, "only in local contracts"));
        }
        let kind = value.kind();
        if kind == MemberKind::Queries {
            return Err(refused(member, "queries members have no evolution edits"));
        }
        let mut variants = self.variants.clone();
        let target = variants
            .get_mut(variant)
            .ok_or_else(|| refused(variant, "unknown variant (the variant set never changes)"))?;
        if add && target.has(member) {
            return Err(refused(member, "already declared"));
        }
        if !add && !target.contains(member, kind) {
            return Err(refused(member, format!("no such {} member", kind.name())));
        }
        target.insert(member, value, member)?;
        let mut kinds = self.kinds.clone();
        check_members(&variants, &mut kinds, self.base.name())?;
        let sides: &[_] = match kind {
            MemberKind::State => &[ChangeSide::State],
            MemberKind::Method => &[ChangeSide::Args, ChangeSide::Result],
            MemberKind::Keyed => &[ChangeSide::Keyed],
            MemberKind::Event => &[ChangeSide::Event],
            MemberKind::Queries => {
                return Err(refused(member, "queries members have no evolution edits"));
            }
        };
        for side in sides {
            self.changes.push(Change {
                variant: variant.into(),
                member: member.into(),
                side: *side,
            });
        }
        self.variants = variants;
        self.kinds = kinds;
        Ok(self)
    }
    pub fn remove(&mut self, variant: &str, member: &str, kind: MemberKind) -> Result<&mut Self> {
        if kind == MemberKind::Queries {
            return Err(refused(member, "queries members have no evolution edits"));
        }
        if !self.variant(variant)?.remove(member, kind) {
            return Err(refused(member, format!("no such {} member", kind.name())));
        }
        Ok(self)
    }
    pub fn remove_method(&mut self, member: &str) -> Result<&mut Self> {
        let mut found = false;
        for variant in self.variants.values_mut() {
            found |= variant.methods.shift_remove(member).is_some();
        }
        if !found {
            return Err(refused(member, "no method"));
        }
        Ok(self)
    }
    pub fn method_args(&mut self, member: &str, args: JsonValue) -> Result<&mut Self> {
        self.method_sides(member, Some(args), None)
    }
    pub fn method_result(&mut self, member: &str, result: JsonValue) -> Result<&mut Self> {
        self.method_sides(member, None, Some(result))
    }
    pub fn method(
        &mut self,
        member: &str,
        args: JsonValue,
        result: JsonValue,
    ) -> Result<&mut Self> {
        self.method_sides(member, Some(args), Some(result))
    }
    fn method_sides(
        &mut self,
        member: &str,
        args: Option<JsonValue>,
        result: Option<JsonValue>,
    ) -> Result<&mut Self> {
        let args = args
            .as_ref()
            .map(|s| snapshot_arguments(s, &format!("{member}.args")))
            .transpose()?;
        let result = result
            .as_ref()
            .map(|s| snapshot_schema(s, &format!("{member}.result")))
            .transpose()?;
        let mut found = false;
        for (variant, data) in &mut self.variants {
            if let Some(method) = data.methods.get_mut(member) {
                found = true;
                if let Some(args) = &args {
                    method.args = args.clone();
                    self.changes.push(Change {
                        variant: variant.clone(),
                        member: member.into(),
                        side: ChangeSide::Args,
                    });
                }
                if let Some(result) = &result {
                    method.result = result.clone();
                    self.changes.push(Change {
                        variant: variant.clone(),
                        member: member.into(),
                        side: ChangeSide::Result,
                    });
                }
            }
        }
        if !found {
            return Err(refused(member, "no method"));
        }
        Ok(self)
    }
    pub fn admission(&mut self, schema: JsonValue) -> Result<&mut Self> {
        if self.base.kind != ContractKind::Remote {
            return Err(refused(self.base.name(), "admission is remote only"));
        }
        self.admission = Some(admission_data(schema)?);
        Ok(self)
    }
}
fn admission_data(schema: JsonValue) -> Result<AdmissionData> {
    let schema = snapshot_schema(&schema, "admission")?;
    if schema.bytes {
        return Err(refused(
            "admission",
            "an admission's value is JSON only (no bytes())",
        ));
    }
    let fingerprint = fingerprint_of(&schema.normal)?;
    Ok(AdmissionData {
        schema,
        fingerprint,
    })
}

pub type ValueAdapter =
    Arc<dyn Fn(pi_rs_schema::Value) -> Result<pi_rs_schema::Value> + Send + Sync>;
pub type CallReplacement = Arc<
    dyn Fn(
            pi_rs_schema::Value,
            crate::context::Context,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<pi_rs_schema::Value>> + Send>,
        > + Send
        + Sync,
>;
#[derive(Clone)]
pub enum ValueMapping {
    Map(ValueAdapter),
    Refuse,
}
#[derive(Clone)]
pub enum MethodMapping {
    Refuse,
    Call(CallReplacement),
    Adapt {
        args: Option<ValueAdapter>,
        result: Option<ValueAdapter>,
    },
}
#[derive(Clone, Default)]
pub struct VersionMappings {
    pub state: IndexMap<(String, String), ValueMapping>,
    pub methods: IndexMap<String, MethodMapping>,
    pub keyed: IndexMap<String, ValueMapping>,
    pub events: IndexMap<(String, String), ValueMapping>,
}
pub type AcceptsTable = IndexMap<u64, VersionMappings>;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedVersion {
    pub version: u64,
    pub fingerprint: String,
}
#[derive(Clone, Copy)]
enum Need {
    Changed,
    Removed,
    Rekeyed,
}
fn check_value_mapping(mapping: &ValueMapping, need: Need, state: bool, path: &str) -> Result<()> {
    match (mapping, need) {
        (ValueMapping::Map(_), Need::Changed | Need::Removed) if state => Ok(()),
        (ValueMapping::Map(_), Need::Changed) => Ok(()),
        (ValueMapping::Refuse, Need::Changed) if state => Err(refused(path, "expected a function")),
        (ValueMapping::Refuse, _) => Ok(()),
        _ => Err(refused(
            path,
            "expected refusal (the key kind changed or member was removed)",
        )),
    }
}
pub fn check_accepts(contract: &Contract, accepts: &AcceptsTable) -> Result<Vec<AcceptedVersion>> {
    let mut accepted = Vec::new();
    for (version, mappings) in accepts {
        let path = format!("{} accepts[{version}]", contract.name());
        let index = contract
            .versions
            .iter()
            .position(|v| v.version == *version)
            .filter(|index| *index + 1 < contract.versions.len())
            .ok_or_else(|| refused(&path, "not an older version"))?;
        let old = &contract.versions[index];
        let current = contract.current();
        let changes: HashSet<_> = contract.versions[index + 1..]
            .iter()
            .flat_map(|v| v.changes.iter().cloned())
            .collect();
        let changed = |variant: &str, member: &str, side| {
            changes.contains(&Change {
                variant: variant.into(),
                member: member.into(),
                side,
            })
        };
        let mut states = IndexMap::new();
        let mut events = IndexMap::new();
        let mut keyed = IndexMap::new();
        let mut methods: IndexMap<String, (bool, bool, bool)> = IndexMap::new();
        for (variant, before) in &old.variants {
            let now = current
                .variants
                .get(variant)
                .ok_or_else(|| refused(&path, "variant missing from history"))?;
            for member in before.state.keys() {
                let need = if !now.state.contains_key(member) {
                    Some(Need::Removed)
                } else if changed(variant, member, ChangeSide::State) {
                    Some(Need::Changed)
                } else {
                    None
                };
                if let Some(need) = need {
                    states.insert((variant.clone(), member.clone()), need);
                }
            }
            for member in before.events.keys() {
                if now.events.contains_key(member) && changed(variant, member, ChangeSide::Event) {
                    events.insert((variant.clone(), member.clone()), Need::Changed);
                }
            }
            for (member, before) in &before.keyed {
                let need = match now.keyed.get(member) {
                    None => Some(Need::Removed),
                    Some(now)
                        if before.key.as_ref().map(|k| &k.kind)
                            != now.key.as_ref().map(|k| &k.kind) =>
                    {
                        Some(Need::Rekeyed)
                    }
                    _ if changed(variant, member, ChangeSide::Keyed) => Some(Need::Changed),
                    _ => None,
                };
                if let Some(need) = need {
                    keyed.insert(member.clone(), need);
                }
            }
            for member in before.methods.keys() {
                let need = methods.entry(member.clone()).or_default();
                need.0 |= !now.methods.contains_key(member);
                need.1 |= changed(variant, member, ChangeSide::Args);
                need.2 |= changed(variant, member, ChangeSide::Result);
            }
        }
        methods.retain(|_, (removed, args, result)| *removed || *args || *result);
        for (section, needed, provided, is_state) in [
            ("state", &states, &mappings.state, true),
            ("events", &events, &mappings.events, false),
        ] {
            for key in provided.keys() {
                if !needed.contains_key(key) {
                    return Err(refused(
                        &format!("{path}.{section}.{}.{}", key.0, key.1),
                        "unnecessary mapping",
                    ));
                }
            }
            for (key, need) in needed {
                let at = if contract.kind == ContractKind::Local {
                    format!("{path}.{section}.{}", key.1)
                } else {
                    format!("{path}.{section}.{}.{}", key.0, key.1)
                };
                let mapping = provided
                    .get(key)
                    .ok_or_else(|| refused(&at, "missing mapping"))?;
                check_value_mapping(mapping, *need, is_state, &at)?;
            }
        }
        for key in mappings.keyed.keys() {
            if !keyed.contains_key(key) {
                return Err(refused(
                    &format!("{path}.keyed.{key}"),
                    "unnecessary mapping",
                ));
            }
        }
        for (member, need) in &keyed {
            let at = format!("{path}.keyed.{member}");
            let mapping = mappings
                .keyed
                .get(member)
                .ok_or_else(|| refused(&at, "missing mapping"))?;
            check_value_mapping(mapping, *need, false, &at)?;
        }
        for member in mappings.methods.keys() {
            if !methods.contains_key(member) {
                return Err(refused(
                    &format!("{path}.methods.{member}"),
                    "unnecessary mapping",
                ));
            }
        }
        for (member, (removed, args, result)) in methods {
            let at = format!("{path}.methods.{member}");
            let mapping = mappings
                .methods
                .get(&member)
                .ok_or_else(|| refused(&at, "missing mapping"))?;
            match mapping {
                MethodMapping::Refuse | MethodMapping::Call(_) => {}
                MethodMapping::Adapt {
                    args: arg_map,
                    result: result_map,
                } => {
                    if removed {
                        return Err(refused(&at, "expected { call } or \"refuse\""));
                    }
                    for (side, needed, present) in [
                        ("args", args, arg_map.is_some()),
                        ("result", result, result_map.is_some()),
                    ] {
                        if needed != present {
                            return Err(refused(
                                &format!("{at}.{side}"),
                                if needed {
                                    "missing mapping"
                                } else {
                                    "unnecessary mapping"
                                },
                            ));
                        }
                    }
                }
            }
        }
        accepted.push(AcceptedVersion {
            version: *version,
            fingerprint: old.fingerprint.clone(),
        });
    }
    Ok(accepted)
}
