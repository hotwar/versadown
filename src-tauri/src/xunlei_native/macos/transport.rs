use super::super::engine::NativeEngineError;
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

pub(super) struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: Receiver<Value>,
    stopped: bool,
}

impl Worker {
    pub fn start(executable: &Path, state: &Path) -> Result<Self, NativeEngineError> {
        let mut child = Command::new(executable)
            .arg(state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| NativeEngineError::WorkerStopped)?;
        let input = child.stdin.take();
        let output = child
            .stdout
            .take()
            .ok_or(NativeEngineError::WorkerStopped)?;
        let (sender, receiver) = mpsc::sync_channel(2);
        std::thread::spawn(move || {
            // Only the bridge's small, sanitized JSON replies may cross this boundary.
            let mut reader = BufReader::new(output);
            loop {
                let mut line = String::new();
                if reader
                    .read_line(&mut line)
                    .ok()
                    .filter(|n| *n > 0)
                    .is_none()
                    || line.len() > 64 * 1024
                {
                    break;
                }
                let Ok(value) = serde_json::from_str(&line) else {
                    break;
                };
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        let mut worker = Self {
            child,
            input,
            output: receiver,
            stopped: false,
        };
        let hello = worker.receive()?;
        if hello["protocol"] != 1 {
            return Err(NativeEngineError::Incompatible);
        }
        Ok(worker)
    }

    fn receive(&mut self) -> Result<Value, NativeEngineError> {
        self.output
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| {
                self.stopped = true;
                let _ = self.child.kill();
                let _ = self.child.wait();
                NativeEngineError::WorkerStopped
            })
    }

    pub fn call(&mut self, request: Value) -> Result<Value, NativeEngineError> {
        if self.stopped {
            return Err(NativeEngineError::WorkerStopped);
        }
        let input = self
            .input
            .as_mut()
            .ok_or(NativeEngineError::WorkerStopped)?;
        serde_json::to_writer(&mut *input, &request)
            .map_err(|_| NativeEngineError::WorkerStopped)?;
        input
            .write_all(b"\n")
            .and_then(|_| input.flush())
            .map_err(|_| NativeEngineError::WorkerStopped)?;
        let reply = self.receive()?;
        if reply["code"] != 0 {
            eprintln!(
                "Mac Thunder worker operation {} failed, code {}",
                request["op"], reply["code"]
            );
            return Err(NativeEngineError::OperationFailed);
        }
        Ok(reply)
    }

    pub fn is_alive(&mut self) -> bool {
        !self.stopped && self.child.try_wait().ok().is_some_and(|v| v.is_none())
    }

    pub fn shutdown(&mut self) {
        if let Some(mut input) = self.input.take() {
            let _ = serde_json::to_writer(&mut input, &json!({"op":"shutdown"}));
            let _ = input.write_all(b"\n");
        }
        let until = Instant::now() + Duration::from_secs(5);
        while self.is_alive() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(25));
        }
        if self.is_alive() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        self.stopped = true;
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}
