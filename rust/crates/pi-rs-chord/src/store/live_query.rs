//! Store-step read preparation and batched watchable adoption.
use super::{ColumnType, ReadPlan, ReadRange, StoreEngine, StoredRead, table::invalid};
use crate::{
    Result,
    context::{Context, lock},
    delta::Revision,
    json::JsonValue,
    sqlite::SqliteFuture,
    state::{Batch, Source},
};
use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use std::sync::{Arc, Mutex, Weak};
type Decode = Arc<dyn Fn(&StoredRead, &Context) -> Result<JsonValue> + Send + Sync>;
#[derive(Clone, Debug)]
pub enum LiveShape {
    Rows { range: ReadRange, paged: bool },
    Count,
}
struct Target {
    plan: ReadPlan,
    context: Context,
    decode: Decode,
}
struct State {
    generation: u64,
    target: Option<Arc<Target>>,
    rows: Vec<(StoredRead, Revision)>,
    closed: bool,
}
pub(crate) struct LiveInner {
    source: Source,
    shape: LiveShape,
    state: Mutex<State>,
}
#[derive(Clone)]
pub struct LiveQuery(Arc<LiveInner>);
enum Run {
    Rows(Vec<StoredRead>, bool),
    Count(i64),
}
pub(crate) struct LiveRun {
    live: Arc<LiveInner>,
    generation: u64,
    target: Arc<Target>,
    result: Result<Run>,
}
impl LiveQuery {
    pub(crate) fn new(batch: Batch, shape: LiveShape) -> Result<Self> {
        if let LiveShape::Rows { range, .. } = &shape
            && (range.limit == 0 || range.limit >= 9_007_199_254_740_991)
        {
            return Err(invalid("live query needs a bounded positive limit"));
        }
        Ok(Self(Arc::new(LiveInner {
            source: Source::framed(batch, "live query", |_| {}),
            shape,
            state: Mutex::new(State {
                generation: 0,
                target: None,
                rows: vec![],
                closed: false,
            }),
        })))
    }
    pub fn source(&self) -> Source {
        self.0.source.clone()
    }
    pub fn value(&self) -> Result<Revision> {
        self.0.source.value()
    }
    pub(crate) fn weak(&self) -> Weak<LiveInner> {
        Arc::downgrade(&self.0)
    }
    pub fn read(&self, engine: &StoreEngine, plan: ReadPlan, context: Context) -> SqliteFuture<()> {
        let definitions = plan.clone();
        self.read_mapped(engine, plan, context, move |row, _| {
            decode_row(&definitions, row)
        })
    }
    pub fn read_mapped(
        &self,
        engine: &StoreEngine,
        plan: ReadPlan,
        context: Context,
        decode: impl Fn(&StoredRead, &Context) -> Result<JsonValue> + Send + Sync + 'static,
    ) -> SqliteFuture<()> {
        {
            let mut state = lock(&self.0.state);
            if !state.closed {
                state.generation = state.generation.wrapping_add(1);
                state.target = Some(Arc::new(Target {
                    plan,
                    context,
                    decode: Arc::new(decode),
                }));
                state.rows.clear();
            }
        }
        let live = self.0.clone();
        let engine = engine.clone();
        engine.database().clone().call(move |c| {
            if let Some(run) = prepare_one(c, live) {
                engine.adopt_live(vec![run])?;
            }
            Ok(())
        })
    }
    pub fn fail(&self, error: impl Into<String>, context: &Context) -> Result<()> {
        let mut state = lock(&self.0.state);
        state.generation = state.generation.wrapping_add(1);
        state.target = None;
        state.rows.clear();
        drop(state);
        self.0.source.fail(error, context)
    }
    pub fn close(&self, error: impl Into<String>) {
        let mut state = lock(&self.0.state);
        state.closed = true;
        state.target = None;
        state.rows.clear();
        drop(state);
        self.0.source.detach(error);
    }
}
fn decode_row(plan: &ReadPlan, row: &StoredRead) -> Result<JsonValue> {
    let mut output = serde_json::Map::new();
    for (name, value) in row {
        if name.ends_with("$v") {
            continue;
        }
        let (definition, local) = if let Some(joined) = &plan.joined
            && let Some(n) = name.strip_prefix(&format!("{}:", joined.set.name))
        {
            (&plan.bindings.get(&joined.set.name)?.definition, n)
        } else {
            (&plan.query.definition, name.as_str())
        };
        let column = definition
            .columns
            .get(local)
            .ok_or_else(|| invalid("unreadable projected column"))?;
        let value = match value {
            SqlValue::Null => continue,
            SqlValue::Integer(n) => {
                if column.kind == ColumnType::Bool {
                    serde_json::Value::Bool(*n == 1)
                } else {
                    serde_json::Value::from(*n)
                }
            }
            SqlValue::Real(n) => serde_json::Value::from(*n),
            SqlValue::Text(text) => {
                if column.kind == ColumnType::Json {
                    let Some(SqlValue::Integer(version)) = row.get(&format!("{name}$v")) else {
                        return Err(invalid("unreadable projected JSON version"));
                    };
                    if let Some(json) = definition.json.get(local) {
                        json.read(text, *version)?.to_serde()?
                    } else if Some(*version) == column.version {
                        serde_json::from_str(text)?
                    } else {
                        return Err(crate::data::ChordError::new(
                            "unavailable",
                            "JSON cell has no migration binding",
                        )
                        .into());
                    }
                } else if column.host.is_some() {
                    let host = definition
                        .hosts
                        .get(local)
                        .ok_or_else(|| invalid("host column needs runtime kind binding"))?;
                    let address = host.address_of_key(text)?;
                    serde_json::json!({"kind":address.kind,"params":address.params.to_serde()?,"key":address.key})
                } else {
                    serde_json::Value::String(text.clone())
                }
            }
            SqlValue::Blob(_) => return Err(invalid("blob in declared table")),
        };
        output.insert(name.clone(), value);
    }
    Ok(JsonValue::from_serde(serde_json::Value::Object(output)))
}
fn prepare_one(c: &Connection, live: Arc<LiveInner>) -> Option<LiveRun> {
    let state = lock(&live.state);
    if state.closed {
        return None;
    }
    let target = state.target.clone()?;
    let generation = state.generation;
    drop(state);
    let result = (|| {
        target.plan.check(c)?;
        match &live.shape {
            LiveShape::Count => {
                let sql = target.plan.count()?;
                Ok(Run::Count(c.query_row(
                    &sql.sql,
                    params_from_iter(sql.params),
                    |r| r.get(0),
                )?))
            }
            LiveShape::Rows { range, .. } => {
                let mut fetched = range.clone();
                fetched.limit = range.limit + 1;
                let mut rows = target.plan.read(c, &fetched)?;
                let more = rows.len() > range.limit;
                rows.truncate(range.limit);
                Ok(Run::Rows(rows, more))
            }
        }
    })();
    Some(LiveRun {
        live,
        generation,
        target,
        result,
    })
}
pub(crate) fn prepare(c: &Connection, observers: &Mutex<Vec<Weak<LiveInner>>>) -> Vec<LiveRun> {
    let lives = {
        let mut observers = lock(observers);
        observers.retain(|w| w.strong_count() > 0);
        observers
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>()
    };
    lives
        .into_iter()
        .filter_map(|live| prepare_one(c, live))
        .collect()
}
pub(crate) fn adopt(runs: Vec<LiveRun>) -> Result<()> {
    for run in runs {
        let state = lock(&run.live.state);
        if state.closed || state.generation != run.generation {
            continue;
        }
        let previous = state.rows.clone();
        drop(state);
        let mut cached = None;
        let context = &run.target.context;
        let result = match run.result {
            Err(error) => Err(error),
            Ok(Run::Count(n)) => {
                Revision::from_json(JsonValue::Number(n as f64)).map_err(crate::Error::from)
            }
            Ok(Run::Rows(rows, more)) => {
                let mut decoded = vec![];
                let mut next = vec![];
                let mut failure = None;
                for row in rows {
                    let revision = if let Some((_, revision)) =
                        previous.iter().find(|(before, _)| *before == row)
                    {
                        Ok(revision.clone())
                    } else {
                        (run.target.decode)(&row, context)
                            .and_then(|v| Revision::from_json(v).map_err(crate::Error::from))
                    };
                    match revision {
                        Ok(revision) => {
                            decoded.push(revision.clone());
                            next.push((row, revision));
                        }
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                }
                if let Some(error) = failure {
                    Err(error)
                } else {
                    cached = Some(next);
                    let rows = Revision::array(decoded);
                    if matches!(run.live.shape, LiveShape::Rows { paged: true, .. }) {
                        Revision::from_json(JsonValue::from_serde(
                            serde_json::json!({"rows":rows.to_json().to_serde()?,"more":more}),
                        ))
                        .map_err(crate::Error::from)
                    } else {
                        Ok(rows)
                    }
                }
            }
        };
        let mut state = lock(&run.live.state);
        // Decoders are user callbacks and can replace or close this target.
        if state.closed || state.generation != run.generation {
            continue;
        }
        if let Some(rows) = cached {
            state.rows = rows;
        }
        drop(state);
        match result {
            Ok(revision) => run.live.source.set_revision(revision, context)?,
            Err(error) => run.live.source.fail(error.to_string(), context)?,
        }
    }
    Ok(())
}
pub(crate) fn close_all(observers: &Mutex<Vec<Weak<LiveInner>>>) {
    for live in lock(observers).iter().filter_map(Weak::upgrade) {
        LiveQuery(live).close("store closed");
    }
}
