use super::*;
impl TypeDeclaration {
    pub(super) fn members(
        &self,
        guest: &Arc<tokio::sync::Mutex<Guest>>,
        sources: &Sources,
        params: &JsonValue,
        principal: Option<&serde_json::Value>,
        self_: &mut crate::host::ObjectSelf,
    ) -> Result<Members> {
        let object = serde_json::to_string(
            &serde_json::json!({"type":self.name,"params":params.to_serde()?,"principal":principal}),
        )?;
        let mut members = self.remote.as_ref().map_or_else(Members::new, |remote| {
            Members::variant(remote.variant.clone())
        });
        for (name, member) in &self.members {
            if member.kind == "store" {
                let document = member
                    .document
                    .as_deref()
                    .ok_or_else(|| failure("store callback requires a document"))?;
                if self
                    .members
                    .get(document)
                    .is_none_or(|decl| decl.kind != "document")
                {
                    return Err(failure(format!("unknown store document {document}")));
                }
                let schema = member
                    .result
                    .clone()
                    .map(JsonValue::from_serde)
                    .unwrap_or_else(crate::contracts::schema::nul);
                let result_schema = Arc::new(crate::data::Validator::compile(&schema.to_serde()?)?);
                let key = (object.clone(), document.to_owned());
                let object = object.clone();
                let sources = sources.clone();
                let name = name.clone();
                let guest = guest.clone();
                members.method(name.clone(), move |args, ctx| {
                    let result_schema = result_schema.clone();
                    let document = sources
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&key)
                        .cloned();
                    let (guest, object, name) = (guest.clone(), object.clone(), name.clone());
                    Box::pin(async move {
                        let Some(Publisher::Document(document)) = document else {
                            return Err(failure("store document is not live"));
                        };
                        // Acquire the guest before the document line, the same order used by publish.
                        let mut guest = guest.lock_owned().await;
                        document
                            .change(&ctx.context, move |draft| {
                                Box::pin(async move {
                                    let args = JsonValue::Object(IndexMap::from([
                                        ("args".into(), args),
                                        ("value".into(), draft.value()?.to_json()),
                                    ]));
                                    let updated = guest
                                        .call(&object, &name, args, &ctx.principal, false)
                                        .await?;
                                    let next = updated.get("value").ok_or_else(|| {
                                        failure("store callback must return value")
                                    })?;
                                    let result = updated.get("result").ok_or_else(|| {
                                        failure("store callback must return result")
                                    })?;
                                    result_schema
                                        .check(&result.to_serde()?, "store callback result")?;
                                    draft.apply(crate::delta::Op::Replace(next.clone()))?;
                                    Ok(result.clone())
                                })
                            })
                            .await
                    })
                })?;
            } else if member.kind == "job" {
                use crate::jobs::{JobImplementation, JobOptions, JobResult, define_job};
                let mut options = JobOptions::new(
                    u64::from(self.version),
                    member
                        .args
                        .clone()
                        .map(JsonValue::from_serde)
                        .unwrap_or_else(crate::contracts::schema::nul),
                    member
                        .result
                        .clone()
                        .map(JsonValue::from_serde)
                        .unwrap_or_else(crate::contracts::schema::nul),
                );
                options.concurrency = member.concurrency;
                options.persist = member.resume.map(|resume| match resume {
                    JobResume::Rerun => crate::jobs::Resume::Rerun,
                    JobResume::Abandon => crate::jobs::Resume::Abandon,
                });
                let def = define_job(name, options)?;
                let guest = guest.clone();
                let object = object.clone();
                let method = name.clone();
                let principal = principal.cloned();
                let binding = self_.job(
                    def,
                    JobImplementation::new(move |args, _| {
                        let (guest, object, method, principal) = (
                            guest.clone(),
                            object.clone(),
                            method.clone(),
                            principal.clone(),
                        );
                        Box::pin(
                            async move { invoke(&guest, &object, &method, args, &principal).await },
                        )
                    }),
                )?;
                members.method(name, move |args, ctx| {
                    let binding = binding.clone();
                    Box::pin(async move {
                        let run = binding.get()?.start(args, None, &ctx.context).await?;
                        match run.outcome(&ctx.context).await?.outcome {
                            JobResult::Done { result } => Ok(result),
                            JobResult::Failed { error } => {
                                Err(failure(serde_json::to_string(&error)?))
                            }
                        }
                    })
                })?;
            } else if member.kind != "method" {
                let initial = JsonValue::from_serde(
                    member.initial.clone().unwrap_or(serde_json::Value::Null),
                );
                let readable = match member.kind.as_str() {
                    "document" => {
                        let def = crate::documents::Definition::new(
                            name,
                            JsonValue::from_serde(member.schema.clone()),
                            u64::from(self.version),
                            move || Ok(initial.clone()),
                        )?
                        .persisted(member.persist, |_, _, _| false);
                        let doc = self_.state(Arc::new(def))?;
                        members.state(name, doc.source()?)?;
                        Publisher::Document(doc)
                    }
                    "state" => {
                        let source = self_.writable(initial)?;
                        members.state(name, source.clone())?;
                        Publisher::State(source)
                    }
                    "event" => {
                        let event = EventChannel::new();
                        members.event(name, event.clone())?;
                        Publisher::Event(event)
                    }
                    "keyed" => {
                        let keyed = Keyed {
                            batch: self_.batch(),
                            initial,
                            entries: Arc::default(),
                        };
                        let resolve = keyed.clone();
                        members.keyed(name, move |key| resolve.source(&key))?;
                        Publisher::Keyed(keyed)
                    }
                    _ => return Err(failure("unknown readable kind")),
                };
                let key = (object.clone(), name.clone());
                let opened = sources.clone();
                let closed = sources.clone();
                self_.open(
                    move |_| {
                        Box::pin(async move {
                            opened
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(key.clone(), readable);
                            Ok(key)
                        })
                    },
                    move |key, _| {
                        Box::pin(async move {
                            closed
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(key.as_ref());
                            Ok(())
                        })
                    },
                );
            } else {
                let (guest, object, name) = (guest.clone(), object.clone(), name.clone());
                members.method(name.clone(), move |args, ctx| {
                    let (guest, object, name) = (guest.clone(), object.clone(), name.clone());
                    Box::pin(
                        async move { invoke(&guest, &object, &name, args, &ctx.principal).await },
                    )
                })?;
            }
        }
        Ok(members)
    }
}
