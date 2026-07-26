use std::sync::Mutex;

use mcp_cli::DiagnosticSink;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiagnosticEvent {
    Warning(String),
    Debug(String),
    ServerStderr { server: String, bytes: Vec<u8> },
}

/// In-memory diagnostic sink preserving event order and stream payload bytes.
#[derive(Debug, Default)]
pub struct RecordingDiagnosticSink {
    events: Mutex<Vec<DiagnosticEvent>>,
}

impl RecordingDiagnosticSink {
    pub fn events(&self) -> Vec<DiagnosticEvent> {
        self.events
            .lock()
            .expect("diagnostic lock poisoned")
            .clone()
    }

    pub fn take(&self) -> Vec<DiagnosticEvent> {
        std::mem::take(&mut *self.events.lock().expect("diagnostic lock poisoned"))
    }
}

impl DiagnosticSink for RecordingDiagnosticSink {
    fn warning(&self, message: &str) {
        self.events
            .lock()
            .expect("diagnostic lock poisoned")
            .push(DiagnosticEvent::Warning(message.to_owned()));
    }

    fn debug(&self, message: &str) {
        self.events
            .lock()
            .expect("diagnostic lock poisoned")
            .push(DiagnosticEvent::Debug(message.to_owned()));
    }

    fn server_stderr(&self, server: &str, bytes: &[u8]) {
        self.events
            .lock()
            .expect("diagnostic lock poisoned")
            .push(DiagnosticEvent::ServerStderr {
                server: server.to_owned(),
                bytes: bytes.to_vec(),
            });
    }
}
