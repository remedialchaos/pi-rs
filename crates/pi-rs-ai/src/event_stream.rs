use std::pin::Pin;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::task::{Context, Poll};
use std::time::Instant;

use futures_util::Stream;
use tokio::sync::{mpsc, watch};

use crate::error::{Error, Result};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, FinalMessage, PartialMessage, StopReason, unix_millis,
};

/// Single-consumer event stream; the terminal result is available without consuming events.
pub struct AssistantMessageEventStream {
    events: mpsc::UnboundedReceiver<AssistantMessageEvent>,
    final_message: watch::Receiver<Option<FinalMessage>>,
}

pub struct AssistantMessageEventSender {
    events: mpsc::UnboundedSender<AssistantMessageEvent>,
    final_message: watch::Sender<Option<FinalMessage>>,
    created: u64,
    started: Instant,
}

impl AssistantMessageEventStream {
    pub fn channel() -> (AssistantMessageEventSender, Self) {
        let (send, events) = mpsc::unbounded_channel();
        let (complete, final_message) = watch::channel(None);
        (
            AssistantMessageEventSender {
                events: send,
                final_message: complete,
                created: unix_millis(),
                started: Instant::now(),
            },
            Self {
                events,
                final_message,
            },
        )
    }

    pub async fn result(&self) -> Result<FinalMessage> {
        self.final_message
            .clone()
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|value| value.clone())
            .ok_or(Error::MissingTerminalEvent)
    }
}

impl Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.events.poll_recv(cx)
    }
}

impl AssistantMessageEventSender {
    pub fn send(&self, event: AssistantMessageEvent) {
        match event {
            AssistantMessageEvent::Done { message, .. }
            | AssistantMessageEvent::Error { error: message, .. } => {
                self.publish(message);
            }
            event if self.final_message.borrow().is_none() => {
                let _ = self.events.send(event);
            }
            _ => {}
        }
    }

    fn publish(&self, mut message: FinalMessage) {
        if self.final_message.borrow().is_some() {
            return;
        }
        if message.duration_ms.is_none() && message.timestamp >= self.created {
            let millis = (self.started.elapsed().as_micros() + 500) / 1000;
            Arc::make_mut(&mut message).duration_ms =
                Some(u64::try_from(millis).unwrap_or(u64::MAX));
        }
        let reason = message.stop_reason;
        let event = if reason.failed() {
            AssistantMessageEvent::Error {
                reason,
                error: message.clone(),
            }
        } else {
            AssistantMessageEvent::Done {
                reason,
                message: message.clone(),
            }
        };
        self.final_message.send_replace(Some(message));
        let _ = self.events.send(event);
    }

    pub fn finish(self, message: AssistantMessage) {
        self.publish(Arc::new(message));
    }
}

pub(crate) fn read_partial(message: &PartialMessage) -> RwLockReadGuard<'_, AssistantMessage> {
    message.read().unwrap_or_else(|e| e.into_inner())
}
pub(crate) fn write_partial(message: &PartialMessage) -> RwLockWriteGuard<'_, AssistantMessage> {
    message.write().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn live(message: AssistantMessage) -> PartialMessage {
    Arc::new(RwLock::new(message))
}

pub(crate) fn fail(mut message: AssistantMessage, error: Error) -> AssistantMessage {
    message.stop_reason = if matches!(error, Error::Aborted) {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    message.error_message = Some(error.to_string());
    message
}
