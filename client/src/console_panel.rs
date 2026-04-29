//! Console output panel - captures and displays application console output.
//! Uses a ring buffer to store recent log lines.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;
use parking_lot::Mutex;

/// Maximum number of console lines to keep in memory
const MAX_CONSOLE_LINES: usize = 1000;

/// A single console output line with timestamp and content.
#[derive(Debug, Clone)]
pub struct ConsoleLine {
    pub timestamp: String,
    pub text: String,
    pub is_stderr: bool,
}

/// Console panel state with ring buffer and channel receiver.
pub struct ConsolePanel {
    /// Ring buffer of console lines
    lines: Vec<ConsoleLine>,
    /// Receiver for new console output
    receiver: Receiver<ConsoleLine>,
}

impl ConsolePanel {
    /// Create a new console panel with a channel for receiving output.
    pub fn new() -> (Self, Sender<ConsoleLine>) {
        let (tx, rx) = mpsc::channel();
        let panel = Self {
            lines: Vec::with_capacity(MAX_CONSOLE_LINES),
            receiver: rx,
        };
        (panel, tx)
    }

    /// Process any new console output from the channel.
    pub fn poll(&mut self) {
        // Drain all available messages without blocking
        while let Ok(line) = self.receiver.try_recv() {
            self.lines.push(line);
            // Trim if we exceed the maximum
            if self.lines.len() > MAX_CONSOLE_LINES {
                self.lines.remove(0);
            }
        }
    }

    /// Get all console lines.
    pub fn lines(&self) -> &[ConsoleLine] {
        &self.lines
    }

    /// Clear all console lines.
    pub fn clear(&mut self) {
        self.lines.clear();
    }
}

impl Default for ConsolePanel {
    fn default() -> Self {
        Self::new().0
    }
}

/// Global console panel instance (using OnceLock for lazy initialization)
static CONSOLE_PANEL: OnceLock<Mutex<ConsolePanel>> = OnceLock::new();

/// Global sender for sending console messages
static CONSOLE_SENDER: OnceLock<Sender<ConsoleLine>> = OnceLock::new();

/// Initialize the console globals. Called once at startup.
fn ensure_initialized() {
    let _ = CONSOLE_PANEL.get_or_init(|| {
        let (panel, sender) = ConsolePanel::new();
        // Also initialize the sender
        let _ = CONSOLE_SENDER.get_or_init(|| sender);
        Mutex::new(panel)
    });
    let _ = CONSOLE_SENDER.get_or_init(|| {
        let (tx, _rx) = mpsc::channel();
        tx
    });
}

/// Get a reference to the global console panel.
pub fn get_console_panel() -> &'static Mutex<ConsolePanel> {
    ensure_initialized();
    CONSOLE_PANEL.get().unwrap()
}

/// Get a reference to the global console sender.
pub fn get_console_sender() -> &'static Sender<ConsoleLine> {
    ensure_initialized();
    CONSOLE_SENDER.get().unwrap()
}

/// Poll the console panel for new messages (call this in the main loop).
pub fn poll_console() {
    if let Some(panel) = CONSOLE_PANEL.get() {
        let mut guard = panel.lock();
        guard.poll();
    }
}

/// Log a message to the console.
pub fn log(message: &str) {
    let line = ConsoleLine {
        timestamp: current_timestamp(),
        text: message.to_string(),
        is_stderr: false,
    };
    if let Some(sender) = CONSOLE_SENDER.get() {
        let _ = sender.send(line);
    }
}

/// Log an error message to the console.
pub fn log_error(message: &str) {
    let line = ConsoleLine {
        timestamp: current_timestamp(),
        text: message.to_string(),
        is_stderr: true,
    };
    if let Some(sender) = CONSOLE_SENDER.get() {
        let _ = sender.send(line);
    }
}

/// Format current timestamp for console output.
pub fn current_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let hours = (secs / 3600) % 24;
    let mins = (secs / 60) % 60;
    let secs = secs % 60;
    let millis = now.subsec_millis();
    format!("{:02}:{:02}:{:02}.{:03}", hours, mins, secs, millis)
}
