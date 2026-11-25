use crate::core::agent_session_runtime::{DiagnosticLevel, RuntimeDiagnostic};

pub fn write_diagnostics_to_stderr(diagnostics: &[RuntimeDiagnostic]) {
    for diagnostic in diagnostics {
        let prefix = match diagnostic.level {
            DiagnosticLevel::Info => "",
            DiagnosticLevel::Warning => "Warning: ",
            DiagnosticLevel::Error => "Error: ",
        };
        eprintln!("{prefix}{}", diagnostic.message);
    }
}
