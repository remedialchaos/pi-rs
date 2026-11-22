use pi_rs_telemetry::*;
use std::{
    future::Future,
    pin::pin,
    task::{Context as TaskContext, Poll, Waker},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let carrier: TelemetryCarrier =
        serde_json::from_str(&std::env::args().nth(1).unwrap_or_else(|| "{}".into()))?;
    let telemetry = InMemoryTelemetryContext::new();
    let parent = telemetry.extract(Some(&carrier));
    let mut future = pin!(
        parent.start_span(SpanOptions::new("remote"), |_| std::future::ready(Ok::<
            _,
            String,
        >(
            ()
        )))
    );
    match future
        .as_mut()
        .poll(&mut TaskContext::from_waker(Waker::noop()))
    {
        Poll::Ready(Ok(())) => {}
        _ => return Err("ready callback failed to settle".into()),
    }
    print!("{}", serde_json::to_string(&telemetry.get_spans())?);
    Ok(())
}
