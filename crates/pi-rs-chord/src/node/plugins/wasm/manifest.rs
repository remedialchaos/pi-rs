use super::*;
mod members;
impl Guest {
    pub(super) fn facet(self, declarations: &str, sources: Sources) -> Result<Facet> {
        let declarations = match serde_json::from_str(declarations)? {
            SetupDeclaration::Types(types) => Manifest {
                types,
                ..Manifest::default()
            },
            SetupDeclaration::Facet(manifest) => manifest,
        };
        // Validate the complete declaration set before any facet can be staged.
        let mut types = Vec::new();
        for declaration in declarations.types {
            let mut members = IndexMap::new();
            for (name, member) in &declaration.members {
                let schema = JsonValue::from_serde(member.schema.clone());
                let kind = match member.kind.as_str() {
                    "state" | "document" => Member::State(schema),
                    "event" => Member::Event(schema),
                    "keyed" => Member::Keyed { schema, key: None },
                    "method" | "job" | "store" => crate::contracts::schema::method(
                        member.args.clone().map(JsonValue::from_serde),
                        member.result.clone().map(JsonValue::from_serde),
                    ),
                    _ => return Err(failure("unknown member kind")),
                };
                members.insert(name.clone(), kind);
            }
            let params: Vec<_> = declaration
                .params
                .iter()
                .map(|(name, schema)| (name.clone(), JsonValue::from_serde(schema.clone())))
                .collect();
            let contract = Arc::new(if let Some(remote) = &declaration.remote {
                let fields = remote
                    .host_params
                    .iter()
                    .map(|field| {
                        params
                            .iter()
                            .find(|(name, _)| name == field)
                            .cloned()
                            .ok_or_else(|| failure(format!("unknown host parameter {field}")))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let kind = crate::contracts::HostKind::new(
                    &remote.host_kind,
                    crate::contracts::read_params(&fields, "host")?,
                )?;
                Contract::remote(
                    &declaration.name,
                    u64::from(declaration.version),
                    params,
                    IndexMap::from([(remote.variant.clone(), members)]),
                    Arc::new(move |params| {
                        let fields = kind
                            .params
                            .fields
                            .iter()
                            .map(|field| {
                                Ok((
                                    field.as_str().into(),
                                    params
                                        .get(field)
                                        .cloned()
                                        .ok_or_else(|| failure("missing address parameter"))?,
                                ))
                            })
                            .collect::<Result<_>>()?;
                        kind.address(JsonValue::Object(fields))
                    }),
                )?
            } else {
                Contract::local(
                    &declaration.name,
                    u64::from(declaration.version),
                    params,
                    members,
                )?
            });
            types.push((contract, declaration));
        }
        let guest = Arc::new(tokio::sync::Mutex::new(self));
        Ok(define_facet(move |setup| {
            use crate::registries::{RegistryProvider, define_registry};
            for registry in &declarations.registries {
                setup.serve_registry(
                    define_registry(&registry.name, registry.version)?,
                    RegistryProvider::<JsonValue, JsonValue>::new(|entries, _| {
                        Ok(JsonValue::Array(
                            entries.iter().map(|entry| entry.value.clone()).collect(),
                        ))
                    }),
                )?;
            }
            for contribution in &declarations.contributions {
                setup.contribute(
                    define_registry(&contribution.name, contribution.version)?,
                    JsonValue::from_serde(contribution.value.clone()),
                )?;
            }
            if !declarations.routes.is_empty() {
                use crate::node::http::{HttpContribution, RouteOptions};
                use axum::response::IntoResponse;
                let mut http = HttpContribution::new("");
                for route in &declarations.routes {
                    let guest = guest.clone();
                    let handler = route.handler.clone();
                    let mut options = RouteOptions::new(move |request| {
                        let guest = guest.clone();
                        let handler = handler.clone();
                        Box::pin(async move {
                            let body = request.body(1024 * 1024).await?;
                            let body = if body.is_empty() {
                                serde_json::Value::Null
                            } else {
                                serde_json::from_slice(&body)?
                            };
                            let args = JsonValue::from_serde(
                                serde_json::json!({"method":request.method.as_str(),"path":request.path,"query":request.query,"params":request.params,"body":body}),
                            );
                            let result =
                                invoke(&guest, "$http", &handler, args, &request.principal).await?;
                            Ok(axum::Json(result).into_response())
                        })
                    });
                    options.public = route.public;
                    http.route(&route.pattern, options).map_err(failure)?;
                }
                setup.contribute_http(http)?;
            }
            for (contract, declaration) in &types {
                let sources = sources.clone();
                let guest = guest.clone();
                let declaration = declaration.clone();
                setup.serve(
                    contract.clone(),
                    move |params, principal, object| {
                        declaration.members(&guest, &sources, params, principal, object)
                    },
                    ServeOptions::default(),
                )?;
            }
            Ok(())
        }))
    }
}
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeDeclaration {
    name: String,
    version: u32,
    #[serde(default)]
    params: IndexMap<String, serde_json::Value>,
    remote: Option<RemoteDeclaration>,
    members: IndexMap<String, MemberDeclaration>,
}
#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteDeclaration {
    host_kind: String,
    #[serde(default)]
    host_params: Vec<String>,
    variant: String,
}
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberDeclaration {
    kind: String,
    #[serde(default)]
    schema: serde_json::Value,
    #[serde(default)]
    persist: bool,
    document: Option<String>,
    concurrency: Option<usize>,
    resume: Option<JobResume>,
    initial: Option<serde_json::Value>,
    args: Option<serde_json::Value>,
    result: Option<serde_json::Value>,
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum JobResume {
    Rerun,
    Abandon,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum SetupDeclaration {
    Types(Vec<TypeDeclaration>),
    Facet(Manifest),
}
#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Manifest {
    types: Vec<TypeDeclaration>,
    routes: Vec<Route>,
    registries: Vec<Registry>,
    contributions: Vec<Contribution>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    pattern: String,
    handler: String,
    #[serde(default)]
    public: bool,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    name: String,
    version: u64,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Contribution {
    name: String,
    version: u64,
    value: serde_json::Value,
}
