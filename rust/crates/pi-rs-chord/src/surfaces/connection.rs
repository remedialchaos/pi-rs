//! Deterministic connection-loop state. Adapters execute effects and feed socket/timer events back by generation.
use serde::{Deserialize, Serialize};
pub const RETRY_INITIAL_MS: u64 = 250;
pub const RETRY_MAX_MS: u64 = 5_000;
pub const STABLE_MS: u64 = 10_000;
pub const CONNECT_TIMEOUT_MS: u64 = 20_000;
pub const STALE_MS: u64 = 25_000;
pub const WAKE_GRACE_MS: u64 = 1_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionState {
    Connecting,
    Connected,
    Waiting,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionEffect {
    Open {
        generation: u64,
    },
    Close {
        generation: u64,
    },
    State(ConnectionState),
    AskRetry,
    Timer {
        at: u64,
        generation: u64,
        kind: TimerKind,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerKind {
    Connect,
    Retry,
    Stale,
}
struct Attempt {
    generation: u64,
    opened_at: Option<u64>,
    last_frame: u64,
}
pub struct ConnectionLoop {
    pub state: ConnectionState,
    attempt: Option<Attempt>,
    generation: u64,
    failures: u32,
    retry: Option<u64>,
    stale: Option<u64>,
    woken: Option<u64>,
    asking: bool,
    stopped: bool,
}
impl Default for ConnectionLoop {
    fn default() -> Self {
        Self {
            state: ConnectionState::Waiting,
            attempt: None,
            generation: 0,
            failures: 0,
            retry: None,
            stale: None,
            woken: None,
            asking: false,
            stopped: false,
        }
    }
}
impl ConnectionLoop {
    pub fn connect(&mut self, now: u64) -> Vec<ConnectionEffect> {
        if self.stopped {
            return Vec::new();
        }
        self.generation += 1;
        self.retry = None;
        self.state = ConnectionState::Connecting;
        self.attempt = Some(Attempt {
            generation: self.generation,
            opened_at: None,
            last_frame: now,
        });
        vec![
            ConnectionEffect::State(self.state),
            ConnectionEffect::Open {
                generation: self.generation,
            },
            ConnectionEffect::Timer {
                at: now.saturating_add(CONNECT_TIMEOUT_MS),
                generation: self.generation,
                kind: TimerKind::Connect,
            },
        ]
    }
    pub fn opened(&mut self, generation: u64, now: u64) -> Vec<ConnectionEffect> {
        let Some(attempt) = self.attempt.as_mut().filter(|a| a.generation == generation) else {
            return Vec::new();
        };
        attempt.opened_at = Some(now);
        attempt.last_frame = now;
        self.state = ConnectionState::Connected;
        vec![ConnectionEffect::State(self.state)]
    }
    pub fn received(&mut self, generation: u64, now: u64) {
        if let Some(attempt) = self.attempt.as_mut().filter(|a| a.generation == generation) {
            attempt.last_frame = now;
        }
    }
    pub fn closed(&mut self, generation: u64, now: u64, online: bool) -> Vec<ConnectionEffect> {
        if self
            .attempt
            .as_ref()
            .is_none_or(|a| a.generation != generation)
        {
            return Vec::new();
        }
        let Some(attempt) = self.attempt.take() else {
            return Vec::new();
        };
        self.stale = None;
        if self.stopped {
            return Vec::new();
        }
        self.state = ConnectionState::Waiting;
        if attempt
            .opened_at
            .is_some_and(|at| now.saturating_sub(at) >= STABLE_MS)
        {
            self.failures = 0;
        }
        let woke = self
            .woken
            .take()
            .is_some_and(|at| now.saturating_sub(at) <= STALE_MS + WAKE_GRACE_MS);
        let backoff = RETRY_INITIAL_MS
            .saturating_mul(1_u64 << self.failures.min(30))
            .min(RETRY_MAX_MS);
        self.failures = self.failures.saturating_add(1);
        let at = now.saturating_add(if woke {
            0
        } else if online {
            backoff
        } else {
            RETRY_MAX_MS
        });
        self.retry = Some(at);
        let mut effects = vec![
            ConnectionEffect::State(self.state),
            ConnectionEffect::Timer {
                at,
                generation,
                kind: TimerKind::Retry,
            },
        ];
        if !self.asking {
            self.asking = true;
            effects.push(ConnectionEffect::AskRetry);
        }
        effects
    }
    pub fn retry_answer(&mut self, retry: bool) -> Vec<ConnectionEffect> {
        self.asking = false;
        if retry { Vec::new() } else { self.stop() }
    }
    pub fn wake(&mut self, now: u64) -> Vec<ConnectionEffect> {
        if self.stopped {
            return Vec::new();
        }
        self.woken = Some(now);
        if self.retry.is_some() {
            return self.connect(now);
        }
        if let Some(attempt) = &self.attempt
            && attempt.opened_at.is_some()
        {
            let at = now.saturating_add(
                WAKE_GRACE_MS.max(
                    attempt
                        .last_frame
                        .saturating_add(STALE_MS + 1)
                        .saturating_sub(now),
                ),
            );
            self.stale = Some(at);
            return vec![ConnectionEffect::Timer {
                at,
                generation: attempt.generation,
                kind: TimerKind::Stale,
            }];
        }
        Vec::new()
    }
    pub fn timer(&mut self, kind: TimerKind, generation: u64, now: u64) -> Vec<ConnectionEffect> {
        if self.stopped {
            return Vec::new();
        }
        match kind {
            TimerKind::Retry
                if generation == self.generation && self.retry.is_some_and(|at| at <= now) =>
            {
                self.connect(now)
            }
            TimerKind::Connect => self
                .attempt
                .as_ref()
                .filter(|a| a.generation == generation && a.opened_at.is_none())
                .map(|_| vec![ConnectionEffect::Close { generation }])
                .unwrap_or_default(),
            TimerKind::Stale if self.stale.is_some_and(|at| at <= now) => {
                self.stale = None;
                self.attempt
                    .as_ref()
                    .filter(|a| {
                        a.generation == generation && now.saturating_sub(a.last_frame) > STALE_MS
                    })
                    .map(|_| vec![ConnectionEffect::Close { generation }])
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }
    pub fn stop(&mut self) -> Vec<ConnectionEffect> {
        if self.stopped {
            return Vec::new();
        }
        self.stopped = true;
        self.retry = None;
        self.stale = None;
        let mut effects = Vec::new();
        if let Some(attempt) = self.attempt.take() {
            effects.push(ConnectionEffect::Close {
                generation: attempt.generation,
            });
            self.state = ConnectionState::Waiting;
            effects.push(ConnectionEffect::State(self.state));
        }
        effects
    }
}
