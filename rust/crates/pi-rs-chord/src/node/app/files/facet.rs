//! Native Host bindings for versioned files and live folder views.
use super::*;
use crate::{
    contracts::{Contract, Member, schema as s},
    data::ChordError,
    host::{Facet, Members, ServeOptions, define_facet},
};

const MAX_PAGE: usize = 1000;
const METHODS: [&str; 18] = [
    "put",
    "begin",
    "append",
    "uploaded",
    "finish",
    "cancel",
    "read",
    "file",
    "get",
    "byKey",
    "byRef",
    "versions",
    "list",
    "mkdir",
    "move",
    "copy",
    "remove",
    "removePath",
];

fn folder_schema() -> JsonValue {
    s::obj([
        ("space", s::required(s::str())),
        ("path", s::required(s::str())),
        ("created", s::required(s::int())),
    ])
}
fn entry_schema() -> Result<JsonValue> {
    let lit = |value: &str| s::lit(JsonValue::String(value.into())).map(s::required);
    Ok(s::union([
        s::obj([
            ("kind", lit("folder")?),
            ("folder", s::required(folder_schema())),
        ]),
        s::obj([("kind", lit("file")?), ("file", s::required(file_schema()))]),
    ]))
}
fn file_schema() -> JsonValue {
    s::obj([
        ("id", s::required(s::str())),
        ("space", s::required(s::str())),
        ("path", s::required(s::str())),
        ("version", s::required(s::int())),
        ("size", s::required(s::int())),
        ("mime", s::required(s::str())),
        ("source", s::required(s::str())),
        ("ref", s::opt(s::str())),
        ("key", s::opt(s::str())),
        ("created", s::required(s::int())),
        ("meta", s::opt(s::unknown())),
    ])
}

/// The host-wide Files contract. Native methods carry bytes as arrays of octets;
/// `summary` is an observable map indexed by space.
pub fn files_contract() -> Result<Contract> {
    let obj = |keys: &[&'static str]| s::obj(keys.iter().map(|&key| (key, s::required(s::str()))));
    let int = |min: f64, max| {
        s::bounded(
            s::int(),
            s::Bounds {
                min: Some(min),
                max,
            },
        )
    };
    let octets = s::arr(int(0., Some(255.))?);
    let piece = s::bounded(
        octets,
        s::Bounds {
            min: None,
            max: Some(CHUNK as f64),
        },
    )?;
    let write = || -> Result<Vec<_>> {
        Ok(vec![
            ("space", s::required(s::str())),
            ("path", s::required(s::str())),
            ("mime", s::required(s::str())),
            ("source", s::required(s::str())),
            (
                "ref",
                s::opt(s::bounded(
                    s::str(),
                    s::Bounds {
                        min: Some(1.),
                        max: None,
                    },
                )?),
            ),
            (
                "key",
                s::opt(s::bounded(
                    s::str(),
                    s::Bounds {
                        min: Some(1.),
                        max: None,
                    },
                )?),
            ),
            ("created", s::opt(int(0., None)?)),
            ("meta", s::opt(s::unknown())),
        ])
    };
    let page = |key, item| -> Result<Member> {
        Ok(s::method(
            Some(s::obj([
                ("space", s::required(s::str())),
                (key, s::required(s::str())),
                ("after", s::opt(s::str())),
                ("limit", s::opt(int(1., Some(MAX_PAGE as f64))?)),
            ])),
            Some(s::obj([
                ("items", s::required(s::arr(item))),
                ("next", s::opt(s::str())),
            ])),
        ))
    };
    let file = file_schema();
    let maybe_file = s::union([file.clone(), s::nul()]);
    let summary = s::obj([
        ("files", s::required(s::int())),
        ("versions", s::required(s::int())),
        ("bytes", s::required(s::int())),
        ("newest", s::required(s::arr(file.clone()))),
    ]);
    let mut put = write()?;
    put.push(("bytes", s::required(piece.clone())));
    let lit = |value: &str| s::lit(JsonValue::String(value.into())).map(s::required);
    let begin = s::union([
        s::obj([("kind", lit("upload")?), ("upload", s::required(s::str()))]),
        s::obj([
            ("kind", lit("exists")?),
            ("file", s::required(file.clone())),
        ]),
    ]);
    Contract::local(
        OWNER,
        2,
        vec![],
        IndexMap::from([
            ("summary".into(), Member::State(s::map(summary))),
            (
                "put".into(),
                s::method(Some(s::obj(put)), Some(file.clone())),
            ),
            (
                "begin".into(),
                s::method(Some(s::obj(write()?)), Some(begin)),
            ),
            (
                "append".into(),
                s::method(
                    Some(s::obj([
                        ("upload", s::required(s::str())),
                        ("offset", s::required(int(0., None)?)),
                        ("bytes", s::required(piece.clone())),
                    ])),
                    None,
                ),
            ),
            ("uploaded".into(), s::method(Some(s::str()), Some(s::int()))),
            (
                "finish".into(),
                s::method(Some(s::str()), Some(file.clone())),
            ),
            ("cancel".into(), s::method(Some(s::str()), None)),
            (
                "read".into(),
                s::method(
                    Some(s::obj([
                        ("id", s::required(s::str())),
                        ("offset", s::required(int(0., None)?)),
                        ("length", s::required(int(0., Some(CHUNK as f64))?)),
                    ])),
                    Some(piece),
                ),
            ),
            (
                "file".into(),
                s::method(Some(s::str()), Some(maybe_file.clone())),
            ),
            (
                "get".into(),
                s::method(
                    Some(s::obj([
                        ("space", s::required(s::str())),
                        ("path", s::required(s::str())),
                        ("version", s::opt(int(1., None)?)),
                    ])),
                    Some(maybe_file.clone()),
                ),
            ),
            (
                "byKey".into(),
                s::method(Some(obj(&["space", "key"])), Some(maybe_file)),
            ),
            ("byRef".into(), page("ref", file.clone())?),
            ("versions".into(), page("path", file.clone())?),
            ("list".into(), page("folder", entry_schema()?)?),
            (
                "mkdir".into(),
                s::method(Some(obj(&["space", "path"])), Some(folder_schema())),
            ),
            (
                "move".into(),
                s::method(Some(obj(&["space", "from", "to"])), None),
            ),
            (
                "copy".into(),
                s::method(Some(obj(&["id", "space", "path"])), Some(file)),
            ),
            ("remove".into(), s::method(Some(s::str()), None)),
            (
                "removePath".into(),
                s::method(Some(obj(&["space", "path"])), None),
            ),
            ("removeSpace".into(), s::method(Some(s::str()), None)),
        ]),
    )
}

/// The live view of one folder in the host's Files service.
pub fn files_folder_contract() -> Result<Contract> {
    Contract::local(
        "chord.app.files.folder",
        1,
        vec![("space".into(), s::str()), ("folder".into(), s::str())],
        IndexMap::from([(
            "listing".into(),
            Member::State(s::obj([
                ("entries", s::required(s::arr(entry_schema()?))),
                ("more", s::required(s::bool())),
            ])),
        )]),
    )
}

impl Files {
    /// Registers both contracts against this host's file store.
    pub fn facet(&self) -> Result<Facet> {
        let files = self.clone();
        let contract = Arc::new(files_contract()?);
        let folder = Arc::new(files_folder_contract()?);
        Ok(define_facet(move |host| {
            let storage = files.clone();
            host.serve(
                contract.clone(),
                move |_, _, this| {
                    let mut members = Members::new();
                    members.state("summary", this.follow_source(storage.0.summary.clone())?)?;
                    for method in METHODS.into_iter().chain(["removeSpace"]) {
                        let storage = storage.clone();
                        members.method(method, move |args, _| {
                            let storage = storage.clone();
                            Box::pin(async move {
                                Ok(JsonValue::from_serde(
                                    storage.invoke(method, args.to_serde()?).await?,
                                ))
                            })
                        })?;
                    }
                    Ok(members)
                },
                ServeOptions::default(),
            )?;
            let storage = files.clone();
            host.serve(
                folder.clone(),
                move |params, _, this| {
                    let params = params.to_serde()?;
                    let space = text(&params["space"])?.to_owned();
                    let folder = text(&params["folder"])?.to_owned();
                    let storage = storage.clone();
                    let mut members = Members::new();
                    members.state(
                        "listing",
                        this.follow(move |target, ctx| {
                            Box::pin(async move {
                                let view =
                                    storage.watch_folder(&space, &folder, target, &ctx.context)?;
                                Ok(Box::new(move || drop(view)) as Box<dyn FnOnce() + Send>)
                            })
                        })?,
                    )?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }))
    }

    async fn invoke(&self, method: &str, args: Value) -> Result<Value> {
        let s = |name| text(&args[name]);
        let n = |name| {
            args[name]
                .as_u64()
                .ok_or_else(|| invalid("expected an unsigned integer"))
        };
        Ok(match method {
            "put" => serde_json::to_value(
                self.put(
                    serde_json::from_value(args.clone())?,
                    &serde_json::from_value::<Vec<u8>>(args["bytes"].clone())?,
                )
                .await?,
            )?,
            "begin" => {
                let write: FileWrite = serde_json::from_value(args)?;
                check_write(&write)?;
                let _line = self.0.line.lock().await;
                if let Some(file) = write
                    .key
                    .as_deref()
                    .and_then(|key| self.by_key(&write.space, key))
                {
                    json!({"kind":"exists","file":file})
                } else {
                    check_destination(
                        &self.0.index.lock().unwrap_or_else(|e| e.into_inner()),
                        &write,
                    )?;
                    json!({"kind":"upload","upload":self.begin(write)?})
                }
            }
            "append" => {
                self.append(
                    s("upload")?,
                    n("offset")?,
                    &serde_json::from_value::<Vec<u8>>(args["bytes"].clone())?,
                )
                .await?;
                Value::Null
            }
            "uploaded" => json!(self.uploaded(text(&args)?)?),
            "finish" => serde_json::to_value(self.finish(text(&args)?).await?)?,
            "cancel" => {
                self.cancel(text(&args)?)?;
                Value::Null
            }
            "read" => json!(self.read(
                s("id")?,
                n("offset")?,
                usize::try_from(n("length")?).map_err(|_| invalid("length out of range"))?
            )?),
            "file" => serde_json::to_value(self.file(text(&args)?))?,
            "get" => serde_json::to_value(self.get(
                s("space")?,
                s("path")?,
                args.get("version").map(|_| n("version")).transpose()?,
            )?)?,
            "byKey" => {
                let space = s("space")?;
                check_space(space)?;
                serde_json::to_value(self.by_key(space, s("key")?))?
            }
            "byRef" => {
                let space = s("space")?;
                check_space(space)?;
                let reference = s("ref")?;
                let mut versions = self
                    .0
                    .index
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .files
                    .values()
                    .filter(|r| {
                        r.info.space == space && r.info.reference.as_deref() == Some(reference)
                    })
                    .map(|r| r.info.clone())
                    .collect::<Vec<_>>();
                versions.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
                paginate(versions, &args, |f| (f.created, f.id.clone()))?
            }
            "versions" => paginate(self.versions(s("space")?, s("path")?)?, &args, |f| {
                std::cmp::Reverse(f.version)
            })?,
            "list" => paginate(
                self.list(s("space")?, s("folder")?)?,
                &args,
                |entry| match entry {
                    FolderEntry::Folder { folder } => (0, folder.path.clone()),
                    FolderEntry::File { file } => (1, file.path.clone()),
                },
            )?,
            "mkdir" => serde_json::to_value(self.make_folder(s("space")?, s("path")?).await?)?,
            "move" => {
                self.move_path(s("space")?, s("from")?, s("to")?).await?;
                Value::Null
            }
            "copy" => {
                let id = s("id")?;
                let old = self.file(id).ok_or_else(|| invalid("file not found"))?;
                let mut write = FileWrite::new(s("space")?, s("path")?);
                write.mime = old.mime;
                write.source = old.source;
                write.reference = old.reference;
                write.meta = old.meta;
                serde_json::to_value(self.copy(id, write).await?)?
            }
            "remove" => {
                self.remove_id(text(&args)?).await?;
                Value::Null
            }
            "removePath" => {
                self.remove(s("space")?, s("path")?, None).await?;
                Value::Null
            }
            "removeSpace" => {
                self.remove_space(text(&args)?).await?;
                Value::Null
            }
            _ => return Err(invalid("unknown Files method")),
        })
    }
}

fn invalid(message: &str) -> Error {
    ChordError::invalid("args", message, None).into()
}
fn text(value: &Value) -> Result<&str> {
    value.as_str().ok_or_else(|| invalid("expected text"))
}

/// Continue strictly after the last returned key even if that entry has been deleted.
fn paginate<T: Serialize, K: Ord + Serialize + serde::de::DeserializeOwned>(
    items: Vec<T>,
    args: &Value,
    key: impl Fn(&T) -> K,
) -> Result<Value> {
    let after = args
        .get("after")
        .map(|after| {
            serde_json::from_str::<K>(text(after)?).map_err(|_| invalid("invalid paging cursor"))
        })
        .transpose()?;
    let limit = args.get("limit").map_or(Ok(100), |limit| {
        limit
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (1..=MAX_PAGE).contains(n))
            .ok_or_else(|| invalid("invalid page size"))
    })?;
    let mut remaining = items
        .into_iter()
        .filter(|item| after.as_ref().is_none_or(|after| key(item) > *after));
    let items = remaining.by_ref().take(limit).collect::<Vec<_>>();
    if let (Some(_), Some(last)) = (remaining.next(), items.last()) {
        Ok(json!({"items":items,"next":serde_json::to_string(&key(last))?}))
    } else {
        Ok(json!({"items":items}))
    }
}
