//! Optional coprocess for progress events.
//!
//! Spawned once via `--hook <cmd>`. Receives JSONL on stdin.
//! Fire-and-forget: converge does not wait for the hook to process
//! events. Ordered delivery is guaranteed by the stdin stream.

use std::io::Write;
use std::process::{Child, Command, Stdio};

use crate::halt::{HaltReport, HookEvent};
use crate::protocol::{Action, FitnessReport};

pub struct Hook {
    child: Child,
}

impl Hook {
    /// Spawn the hook command via `sh -c` so shell features work.
    pub fn spawn(cmd: &str) -> std::io::Result<Self> {
        let child = Command::new("sh")
            .args(["-c", cmd])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        Ok(Self { child })
    }

    /// Send an iteration event. Non-blocking, best-effort.
    pub fn send_iteration(&mut self, iter: u32, report: &FitnessReport, action: &Action) {
        let event = HookEvent::Iteration { iter, report, action };
        self.send(&event);
    }

    /// Send a halt event. Non-blocking, best-effort.
    pub fn send_halt(&mut self, halt: &HaltReport, last_report: Option<&FitnessReport>) {
        let event = HookEvent::Halt { halt, last_report };
        self.send(&event);
    }

    fn send(&mut self, event: &HookEvent) {
        if let Some(stdin) = self.child.stdin.as_mut() {
            if let Ok(line) = serde_json::to_string(event) {
                let _ = writeln!(stdin, "{line}");
                let _ = stdin.flush();
            }
        }
    }

    /// Close stdin and wait for the child to exit.
    pub fn finish(mut self) {
        // Drop stdin to signal EOF.
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}
