use std::sync::Arc;

use remoteprocess::Pid;
use serde_derive::Serialize;

/// Call stack for a single VM thread.
#[derive(Debug, Clone, Serialize)]
pub struct StackTrace {
    /// The process id that generated this stack trace.
    pub pid: Pid,
    /// The VM thread id for this stack trace.
    pub thread_id: u64,
    /// The VM thread name for this stack trace, if known.
    pub thread_name: Option<String>,
    /// The OS thread id for this stack trace.
    pub os_thread_id: Option<u64>,
    /// Whether or not the thread was active when sampled.
    pub active: bool,
    /// The frames, innermost first.
    pub frames: Vec<Frame>,
    /// Process command line / parent process info.
    pub process_info: Option<Arc<ProcessInfo>>,
}

/// Information about a single function call in a stack trace.
#[derive(Debug, Hash, Eq, PartialEq, Ord, PartialOrd, Clone, Serialize)]
pub struct Frame {
    /// The function or method name.
    pub name: String,
    /// The full filename, image marker, or module path for the frame.
    pub filename: String,
    /// The native module/shared library for the frame, if available.
    pub module: Option<String>,
    /// A short, more readable representation of the filename.
    pub short_filename: Option<String>,
    /// The line number inside the file, or 0 if no line information is available.
    pub line: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessInfo {
    pub pid: Pid,
    pub command_line: String,
    pub parent: Option<Box<ProcessInfo>>,
}

impl StackTrace {
    pub fn status_str(&self) -> &str {
        if self.active {
            "active"
        } else {
            "idle"
        }
    }

    pub fn format_threadid(&self) -> String {
        // Native thread ids on macOS are not very useful, so use the VM thread id instead.
        #[cfg(target_os = "macos")]
        return format!("{:#X}", self.thread_id);

        #[cfg(not(target_os = "macos"))]
        match self.os_thread_id {
            Some(tid) => format!("{}", tid),
            None => format!("{:#X}", self.thread_id),
        }
    }
}

impl ProcessInfo {
    pub fn to_frame(&self) -> Frame {
        Frame {
            name: format!("process {}:\"{}\"", self.pid, self.command_line),
            filename: String::new(),
            module: None,
            short_filename: None,
            line: 0,
        }
    }
}
