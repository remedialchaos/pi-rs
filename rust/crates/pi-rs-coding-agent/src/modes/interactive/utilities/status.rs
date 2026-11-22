use indexmap::IndexMap;
use pi_rs_agent::{AgentEvent, AgentMessage};
use pi_rs_ai::StopReason;
use pi_rs_tui::ProgramStatusState::{Blocked, Done, Error, Idle, Working};
use pi_rs_tui::{ProgramStatus, ProgramStatusKind};

use crate::config::APP_NAME;
use crate::core::agent_session::{AgentSessionEvent, CompactionReason};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedStatus {
    pub kind: ProgramStatusKind,
    pub message: String,
}

#[derive(Default)]
pub struct ProgramStatusReporter {
    run_active: bool,
    compacting: bool,
    run_error: Option<String>,
    run_aborted: bool,
    resting: ProgramStatus,
    blocked: IndexMap<String, BlockedStatus>,
    last: Option<ProgramStatus>,
}

impl ProgramStatusReporter {
    /// Return a new wire status only when changed; the caller owns the terminal.
    pub fn handle_event(
        &mut self,
        event: &AgentSessionEvent,
        session_name: Option<&str>,
    ) -> Option<ProgramStatus> {
        match event {
            AgentSessionEvent::AgentStart | AgentSessionEvent::Core(AgentEvent::AgentStart) => {
                self.run_active = true;
                self.run_error = None;
                self.run_aborted = false;
            }
            AgentSessionEvent::Core(AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(message),
            }) => {
                self.run_error = (message.stop_reason == StopReason::Error)
                    .then(|| first_line(message.error_message.as_deref()));
                self.run_aborted = false;
            }
            AgentSessionEvent::CompactionStart { .. } => self.compacting = true,
            AgentSessionEvent::CompactionEnd {
                reason,
                error_message,
                aborted,
                ..
            } => {
                self.compacting = false;
                if self.run_active {
                    if *aborted {
                        self.run_aborted = true;
                        self.run_error = None;
                    } else if let Some(error) = error_message {
                        self.run_aborted = false;
                        self.run_error = Some(first_line(Some(error)));
                    }
                } else if *aborted {
                    self.resting = outcome(true, None);
                } else if *reason == CompactionReason::Manual {
                    self.resting = outcome(
                        false,
                        error_message
                            .as_deref()
                            .map(|error| first_line(Some(error))),
                    );
                }
            }
            AgentSessionEvent::AgentSettled { aborted } => {
                self.run_active = false;
                self.resting = outcome(*aborted || self.run_aborted, self.run_error.take());
            }
            AgentSessionEvent::SessionInfoChanged { name } => return self.report(name.as_deref()),
            _ => return None,
        }
        self.report(session_name)
    }

    /// Report the most recently opened dialog; clearing it restores any dialog behind it.
    pub fn set_blocked(
        &mut self,
        source: &str,
        status: Option<BlockedStatus>,
        session_name: Option<&str>,
    ) -> Option<ProgramStatus> {
        self.blocked.shift_remove(source);
        if let Some(status) = status {
            self.blocked.insert(source.to_owned(), status);
        }
        self.report(session_name)
    }

    /// Forget the run from the previous session without closing existing dialogs.
    pub fn reset(&mut self, session_name: Option<&str>) -> Option<ProgramStatus> {
        self.run_active = false;
        self.compacting = false;
        self.run_error = None;
        self.run_aborted = false;
        self.resting = ProgramStatus::default();
        self.report(session_name)
    }

    pub fn report(&mut self, session_name: Option<&str>) -> Option<ProgramStatus> {
        let (state, kind, message) = if let Some((_, blocked)) = self.blocked.last() {
            (Blocked, Some(blocked.kind), Some(blocked.message.as_str()))
        } else if self.compacting {
            (Working, None, Some("Compacting context"))
        } else if self.run_active || self.resting.state == Done {
            (
                if self.run_active { Working } else { Done },
                None,
                session_name,
            )
        } else {
            (self.resting.state, None, self.resting.message.as_deref())
        };
        if self.last.as_ref().is_some_and(|last| {
            last.state == state && last.kind == kind && last.message.as_deref() == message
        }) {
            return None;
        }
        let status = ProgramStatus {
            state,
            app: Some(APP_NAME.to_owned()),
            kind,
            message: message.map(str::to_owned),
        };
        self.last = Some(status.clone());
        Some(status)
    }
}

fn outcome(aborted: bool, error: Option<String>) -> ProgramStatus {
    let (state, message) = if aborted {
        (Idle, None)
    } else if error.is_some() {
        (Error, error)
    } else {
        (Done, None)
    };
    ProgramStatus {
        state,
        message,
        ..ProgramStatus::default()
    }
}

fn first_line(message: Option<&str>) -> String {
    message
        .and_then(|message| message.lines().next())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .unwrap_or("Error")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_rs_tui::ProgramStatusState;

    fn state(report: Option<ProgramStatus>) -> Option<ProgramStatusState> {
        report.map(|status| status.state)
    }

    #[test]
    fn blocks_compaction_and_settled_runs_in_order() -> Result<(), String> {
        let mut reporter = ProgramStatusReporter::default();
        let name = Some("session A");
        assert_eq!(state(reporter.report(name)), Some(Idle));
        assert!(reporter.report(name).is_none());
        assert_eq!(
            state(reporter.handle_event(&AgentSessionEvent::AgentStart, name)),
            Some(Working)
        );
        let reason = CompactionReason::Overflow;
        let compacting =
            reporter.handle_event(&AgentSessionEvent::CompactionStart { reason }, name);
        assert_eq!(
            compacting.and_then(|status| status.message).as_deref(),
            Some("Compacting context")
        );
        let status = BlockedStatus {
            kind: ProgramStatusKind::Question,
            message: "Choose one".into(),
        };
        assert_eq!(
            state(reporter.set_blocked("dialog", Some(status), name)),
            Some(Blocked)
        );
        assert!(
            reporter
                .handle_event(
                    &AgentSessionEvent::CompactionEnd {
                        reason,
                        result: None,
                        error_message: Some("first line\r\nprivate continuation".into()),
                        aborted: false,
                        will_retry: false,
                    },
                    name
                )
                .is_none()
        );
        assert!(
            reporter
                .handle_event(&AgentSessionEvent::AgentSettled { aborted: false }, name)
                .is_none()
        );
        let error = reporter
            .set_blocked("dialog", None, name)
            .ok_or("No error report")?;
        assert_eq!(error.state, Error);
        assert_eq!(error.message.as_deref(), Some("first line"));
        assert_eq!(state(reporter.reset(name)), Some(Idle));
        Ok(())
    }
}
