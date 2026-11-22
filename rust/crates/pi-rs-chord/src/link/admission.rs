use super::{Body, FailedMember, MAX_SAFE_INTEGER, Message, invalid};
use crate::{Result, json::JsonValue};
fn number(n: u64) -> Result<()> {
    if n > MAX_SAFE_INTEGER {
        return Err(invalid("message: integer outside schema range"));
    }
    Ok(())
}
fn error(value: &JsonValue) -> Result<()> {
    let JsonValue::Object(fields) = value else {
        return Err(invalid("message: malformed wire error"));
    };
    let Some(JsonValue::String(kind)) = value.get("kind") else {
        return Err(invalid("message: wire error has no kind"));
    };
    let allowed: &[&str] = match kind.to_utf8()?.as_str() {
        "chord" => {
            if value.get("error").is_none() {
                return Err(invalid("message: chord envelope has no error"));
            }
            &["kind", "error"]
        }
        "plugin" => {
            if !matches!(value.get("code"), Some(JsonValue::String(_)))
                || !matches!(value.get("message"), Some(JsonValue::String(_)))
            {
                return Err(invalid("message: plugin error needs code and message"));
            }
            &["kind", "code", "message", "details"]
        }
        _ => return Err(invalid("message: unknown wire error kind")),
    };
    if fields.keys().any(|key| {
        key.to_utf8()
            .map_or(true, |s| !allowed.contains(&s.as_str()))
    }) {
        return Err(invalid("message: wire error has unknown field"));
    }
    Ok(())
}
fn failed(entries: &[FailedMember]) -> Result<()> {
    for each in entries {
        number(each.member)?;
        error(&each.error)?;
    }
    Ok(())
}
pub(super) fn check(message: &Message) -> Result<()> {
    match &message.body {
        Body::Hello {
            protocol, calls, ..
        } => {
            number(*protocol)?;
            if let Some(n) = calls {
                number(*n)?;
            }
        }
        Body::Hold {
            hold,
            version,
            limit,
            principal,
            ..
        } => {
            number(*hold)?;
            number(*version)?;
            if let Some(n) = limit {
                number(*n)?;
            }
            if let Some(p) = principal
                && (!matches!(p.as_ref(), JsonValue::Object(_))
                    || !matches!(p.get("kind"), Some(JsonValue::String(_))))
            {
                return Err(invalid("message: principal needs string kind"));
            }
        }
        Body::Release { hold } | Body::Retry { hold, .. } | Body::Loading { hold } => {
            number(*hold)?
        }
        Body::Call {
            call, hold, method, ..
        } => {
            number(*call)?;
            number(*hold)?;
            number(*method)?;
        }
        Body::Abort { call } | Body::Result { call, .. } => number(*call)?,
        Body::Error { call, error: wire } => {
            number(*call)?;
            error(wire)?;
        }
        Body::Gone { hold, error: wire } => {
            number(*hold)?;
            error(wire)?;
        }
        Body::Failed {
            hold,
            error: wire,
            retry_in,
            ..
        } => {
            number(*hold)?;
            error(wire)?;
            if let Some(n) = retry_in {
                number(*n)?;
            }
        }
        Body::Bound(holds) => {
            for h in holds {
                number(h.hold)?;
                number(h.variant)?;
                for (member, _) in &h.members {
                    number(*member)?;
                }
                failed(&h.failed)?;
            }
        }
        Body::State(holds) => {
            for h in holds {
                number(h.hold)?;
                for (member, _) in &h.members {
                    number(*member)?;
                }
                for member in &h.retired {
                    number(*member)?;
                }
                failed(&h.failed)?;
            }
        }
        Body::Ack { events: Some(n) } => number(*n)?,
        Body::Event(events) => {
            for e in events {
                number(e.hold)?;
                number(e.member)?;
                number(e.at)?;
            }
        }
        _ => {}
    }
    Ok(())
}
