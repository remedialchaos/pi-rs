//! SQLite telemetry v2. Fallible inherent reads preserve storage errors that the current telemetry trait cannot carry.
use crate::{
    Error, Result,
    sqlite::{Migration, SqliteDatabase, SqliteFuture, apply_migrations_in},
    telemetry_ingest::{TelemetryBatch, series_key, sorted_attributes},
};
use pi_rs_telemetry::{
    AttributeValue, MetricQuery, SpanAttributes, StoredEvent, StoredLink, StoredMetric, StoredSpan,
    StoredTrace, TracePage, TraceQuery,
};
use rusqlite::{
    Connection, OptionalExtension, Row, params, params_from_iter, types::Value as SqlValue,
};
use serde_json::{Value, json};
pub fn migrations() -> Vec<Migration> {
    vec![
    Migration{version:1,statements:vec![
        "CREATE TABLE telemetry_spans (id TEXT PRIMARY KEY, trace_id TEXT NOT NULL, parent_id TEXT, name TEXT, started_ms REAL, ended_ms REAL NOT NULL, attributes TEXT, events TEXT, links TEXT, status TEXT)".into(),
        "CREATE INDEX telemetry_spans_ended_ms ON telemetry_spans(ended_ms)".into(),"CREATE INDEX telemetry_spans_trace_started ON telemetry_spans(trace_id, started_ms)".into(),"CREATE INDEX telemetry_spans_started_ms ON telemetry_spans(started_ms)".into(),
        "CREATE TABLE telemetry_metrics (interval_ms INTEGER, series_key TEXT, name TEXT, kind TEXT, unit TEXT, attributes TEXT, count INTEGER, sum REAL, min REAL, max REAL, PRIMARY KEY(interval_ms, series_key))".into(),
    ]},
    Migration{version:2,statements:vec![
        "CREATE TABLE telemetry_spans_us (id TEXT PRIMARY KEY, trace_id TEXT NOT NULL, parent_id TEXT, name TEXT, started_us INTEGER, ended_us INTEGER NOT NULL, attributes TEXT, events TEXT, links TEXT, status TEXT)".into(),
        "INSERT INTO telemetry_spans_us SELECT id,trace_id,parent_id,name,CAST(round(started_ms*1000) AS INTEGER),CAST(round(ended_ms*1000) AS INTEGER),attributes,events,links,status FROM telemetry_spans".into(),
        "DROP TABLE telemetry_spans".into(),"ALTER TABLE telemetry_spans_us RENAME TO telemetry_spans".into(),
        "CREATE INDEX telemetry_spans_ended_us ON telemetry_spans(ended_us)".into(),"CREATE INDEX telemetry_spans_trace_started ON telemetry_spans(trace_id, started_us)".into(),"CREATE INDEX telemetry_spans_started_us ON telemetry_spans(started_us)".into(),
    ]},
]
}
pub fn migrate(db: &SqliteDatabase) -> SqliteFuture<()> {
    db.transaction(|db| apply_migrations_in(db, "telemetry_schema", &migrations()))
}
pub fn assert_store(db: &SqliteDatabase) -> SqliteFuture<()> {
    db.call(|db|{let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='telemetry_schema')",[],|r|r.get(0))?;if !exists{return Err(Error::Invalid("not a telemetry store: no telemetry_schema table".into()));}let version:u32=db.query_row("SELECT version FROM telemetry_schema WHERE singleton=1",[],|r|r.get(0)).optional()?.unwrap_or(0);if version>2{return Err(Error::Invalid(format!("telemetry store version {version} is newer than this reader's 2")));}
    if version<2{return Err(Error::Invalid(format!("telemetry store version {version}, not 2: a recorder (createSqliteTelemetry) migrates it")));}Ok(())})
}
fn micros(ms: f64) -> Result<i64> {
    let scaled = ms * 1000.0;
    if !scaled.is_finite() || scaled.abs() > 9_007_199_254_740_991.0 {
        return Err(Error::Invalid(
            "telemetry: span time is outside safe integer microseconds".into(),
        ));
    }
    Ok(if scaled.fract() == -0.5 {
        scaled.ceil() as i64
    } else {
        scaled.round() as i64
    })
}
pub fn write_batch(db: &SqliteDatabase, batch: TelemetryBatch, cutoff_ms: f64) -> SqliteFuture<()> {
    db.transaction(move|db|{
    for span in batch.spans{
        let events:Vec<_>=span.events.iter().map(|e|json!({"name":e.name,"time_ms":e.time_ms,"attributes":e.attributes})).collect();
        let links:Vec<_>=span.links.iter().map(|l|json!({"trace_id":l.trace_id,"span_id":l.span_id,"attributes":l.attributes})).collect();
        db.execute("INSERT OR IGNORE INTO telemetry_spans VALUES(?,?,?,?,?,?,?,?,?,?)",params![span.id,span.trace_id,span.parent_id,span.name,micros(span.started_ms)?,micros(span.ended_ms)?,crate::json::stringify(&serde_json::to_value(span.attributes)?)?,crate::json::stringify(&json!(events))?,crate::json::stringify(&json!(links))?,crate::json::stringify(&serde_json::to_value(span.status)?)?])?;
    }
    for metric in batch.metrics{let kind=serde_json::to_value(metric.kind)?.as_str().ok_or_else(||Error::Invalid("invalid metric kind".into()))?.to_string();db.execute("INSERT INTO telemetry_metrics VALUES(?,?,?,?,?,?,?,?,?,?) ON CONFLICT(interval_ms,series_key) DO UPDATE SET count=count+excluded.count,sum=sum+excluded.sum,min=min(min,excluded.min),max=max(max,excluded.max)",params![metric.interval_ms,series_key(&metric)?,metric.name,kind,metric.unit,crate::json::stringify(&serde_json::to_value(sorted_attributes(&metric.attributes))?)?,i64::try_from(metric.count).map_err(|_|Error::Invalid("telemetry: count exceeds SQLite integer".into()))?,metric.sum,metric.min,metric.max])?;}
    db.execute("DELETE FROM telemetry_spans WHERE ended_us<?",[cutoff_ms*1000.0])?;db.execute("DELETE FROM telemetry_metrics WHERE interval_ms<?",[cutoff_ms])?;Ok(())
})
}
const SPAN_ORDER: &str = "started_us,length(id),id";
fn span_row(row: &Row<'_>) -> Result<StoredSpan> {
    let events: Vec<Value> = serde_json::from_str(&row.get::<_, String>(7)?)?;
    let links: Vec<Value> = serde_json::from_str(&row.get::<_, String>(8)?)?;
    let events = events
        .into_iter()
        .map(|mut v| {
            if let Some(o) = v.as_object_mut()
                && let Some(time) = o.remove("time_ms")
            {
                o.insert("timeMs".into(), time);
            }
            serde_json::from_value::<StoredEvent>(v).map_err(Error::from)
        })
        .collect::<Result<_>>()?;
    let links = links
        .into_iter()
        .map(|mut v| {
            if let Some(o) = v.as_object_mut() {
                if let Some(id) = o.remove("trace_id") {
                    o.insert("traceId".into(), id);
                }
                if let Some(id) = o.remove("span_id") {
                    o.insert("spanId".into(), id);
                }
            }
            serde_json::from_value::<StoredLink>(v).map_err(Error::from)
        })
        .collect::<Result<_>>()?;
    Ok(StoredSpan {
        id: row.get(0)?,
        trace_id: row.get(1)?,
        parent_id: row.get(2)?,
        name: row.get(3)?,
        started_ms: row.get::<_, i64>(4)? as f64 / 1000.0,
        ended_ms: row.get::<_, i64>(5)? as f64 / 1000.0,
        attributes: serde_json::from_str(&row.get::<_, String>(6)?)?,
        events,
        links,
        status: serde_json::from_str(&row.get::<_, String>(9)?)?,
    })
}
fn trace_in(db: &Connection, id: &str) -> Result<Option<StoredTrace>> {
    let mut statement = db.prepare_cached(&format!(
        "SELECT * FROM telemetry_spans WHERE trace_id=? ORDER BY {SPAN_ORDER}"
    ))?;
    let mut rows = statement.query([id])?;
    let mut spans = Vec::new();
    while let Some(row) = rows.next()? {
        spans.push(span_row(row)?);
    }
    Ok((!spans.is_empty()).then(|| StoredTrace {
        trace_id: id.into(),
        spans,
    }))
}
pub fn get_trace(db: &SqliteDatabase, trace_id: String) -> SqliteFuture<Option<StoredTrace>> {
    db.call(move |db| trace_in(db, &trace_id))
}
fn attribute_sql(attributes: &SpanAttributes, column: &str, params: &mut Vec<SqlValue>) -> String {
    let mut conditions = Vec::new();
    for (key, value) in attributes {
        let (kind, value) = match value {
            AttributeValue::Number(n) => ("entry.type IN ('integer','real')", SqlValue::Real(*n)),
            AttributeValue::Boolean(b) => (
                if *b {
                    "entry.type='true'"
                } else {
                    "entry.type='false'"
                },
                SqlValue::Integer(i64::from(*b)),
            ),
            AttributeValue::String(s) => ("entry.type='text'", SqlValue::Text(s.clone())),
            _ => {
                conditions.push("0".into());
                continue;
            }
        };
        params.push(SqlValue::Text(key.clone()));
        params.push(value);
        conditions.push(format!("EXISTS(SELECT 1 FROM json_each({column}) entry WHERE entry.key=? AND {kind} AND entry.value=?)"));
    }
    if conditions.is_empty() {
        "1".into()
    } else {
        conditions.join(" AND ")
    }
}
pub fn find_traces(db: &SqliteDatabase, query: TraceQuery) -> SqliteFuture<TracePage> {
    db.transaction(move|db|{
    let limit=query.limit.unwrap_or(20);if limit==0||limit>=i64::MAX as usize{return Err(Error::Invalid("trace limit must be positive".into()));}let mut conditions=Vec::new();let mut params=Vec::new();
    if let Some(name)=query.name{conditions.push(format!("(SELECT name FROM telemetry_spans WHERE trace_id=heads.trace_id ORDER BY {SPAN_ORDER} LIMIT 1)=?"));params.push(SqlValue::Text(name));}
    if let Some(n)=query.start_min{conditions.push("started_us>=?".into());params.push(SqlValue::Real(n*1000.0));}
    if let Some(n)=query.start_max{conditions.push("started_us<=?".into());params.push(SqlValue::Real(n*1000.0));}
    if query.errors{conditions.push("EXISTS(SELECT 1 FROM telemetry_spans WHERE trace_id=heads.trace_id AND json_extract(status,'$.status')='error')".into());}
    if !query.attributes.is_empty(){conditions.push(format!("EXISTS(SELECT 1 FROM telemetry_spans candidate WHERE candidate.trace_id=heads.trace_id AND {})",attribute_sql(&query.attributes,"candidate.attributes",&mut params)));}
    if let Some(cursor)=query.cursor{let(time,id):(f64,String)=serde_json::from_str(&cursor)?;conditions.push("(started_us<? OR (started_us=? AND trace_id>?))".into());params.extend([SqlValue::Real(time),SqlValue::Real(time),SqlValue::Text(id)]);}
    params.push(SqlValue::Integer(limit as i64+1));let condition=if conditions.is_empty(){"1".into()}else{conditions.join(" AND ")};let sql=format!("WITH heads AS(SELECT trace_id,min(started_us) started_us FROM telemetry_spans GROUP BY trace_id) SELECT * FROM heads WHERE {condition} ORDER BY started_us DESC,trace_id LIMIT ?");
    let mut statement=db.prepare_cached(&sql)?;let mut heads=statement.query_map(params_from_iter(params),|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;let more=heads.len()>limit;if more{heads.pop();}
    let cursor=if more{heads.last().map(|(id,time)|crate::json::stringify(&json!([time,id]))).transpose()?}else{None};let mut traces=Vec::new();for(id,_)in heads{if let Some(trace)=trace_in(db,&id)?{traces.push(trace);}}Ok(TracePage{traces,cursor})
})
}
pub fn find_metrics(db: &SqliteDatabase, query: MetricQuery) -> SqliteFuture<Vec<StoredMetric>> {
    db.call(move|db|{
    let mut params=vec![SqlValue::Text(query.name)];let mut conditions=vec!["name=?".into()];if let Some(n)=query.start_min{conditions.push("interval_ms>=?".into());params.push(SqlValue::Real(n));}
    if let Some(n)=query.start_max{conditions.push("interval_ms<=?".into());params.push(SqlValue::Real(n));}conditions.push(attribute_sql(&query.attributes,"attributes",&mut params));
    let mut statement=db.prepare_cached(&format!("SELECT interval_ms,name,kind,unit,attributes,count,sum,min,max FROM telemetry_metrics WHERE {} ORDER BY interval_ms,series_key",conditions.join(" AND ")))?;let mut rows=statement.query(params_from_iter(params))?;let mut metrics=Vec::new();while let Some(row)=rows.next()?{metrics.push(StoredMetric{interval_ms:row.get(0)?,name:row.get(1)?,kind:serde_json::from_value(Value::String(row.get(2)?))?,unit:row.get(3)?,attributes:serde_json::from_str(&row.get::<_,String>(4)?)?,count:u64::try_from(row.get::<_,i64>(5)?).map_err(|_|Error::Invalid("telemetry: negative count".into()))?,sum:row.get(6)?,min:row.get(7)?,max:row.get(8)?});}Ok(metrics)
})
}
