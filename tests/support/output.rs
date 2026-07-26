use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

/// Cloneable in-memory `Write` implementation for injected stdout/stderr.
#[derive(Clone, Debug, Default)]
pub struct MemoryWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl MemoryWriter {
    pub fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("writer lock poisoned").clone()
    }

    pub fn string(&self) -> String {
        String::from_utf8(self.bytes()).expect("captured output was not UTF-8")
    }

    pub fn clear(&self) {
        self.bytes.lock().expect("writer lock poisoned").clear();
    }
}

impl Write for MemoryWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes
            .lock()
            .map_err(|_| io::Error::other("writer lock poisoned"))?
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Independent stdout/stderr capture handles for process-boundary presenters.
#[derive(Clone, Debug, Default)]
pub struct CapturedOutput {
    pub stdout: MemoryWriter,
    pub stderr: MemoryWriter,
}

impl CapturedOutput {
    pub fn clear(&self) {
        self.stdout.clear();
        self.stderr.clear();
    }
}
