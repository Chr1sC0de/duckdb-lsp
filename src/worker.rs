use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

pub struct Worker {
    process: Mutex<Option<Process>>,
}
struct Process {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    python: String,
}
impl Default for Worker {
    fn default() -> Self {
        Self {
            process: Mutex::new(None),
        }
    }
}
impl Worker {
    pub async fn close(&self) {
        if let Some(mut p) = self.process.lock().await.take() {
            let _ = p.child.kill().await;
        }
    }
    pub async fn request(
        &self,
        python: &str,
        timeout_ms: u64,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let mut guard = self.process.lock().await;
        if guard.as_ref().is_some_and(|p| p.python != python) {
            if let Some(mut p) = guard.take() {
                let _ = p.child.kill().await;
            }
        }
        if guard.is_none() {
            let mut child = Command::new(python)
                .args(["-u", "-c", include_str!("../workers/worker.py")])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(|_| "Python worker unavailable; set duckdbLsp.python.".to_string())?;
            let input = child.stdin.take().unwrap();
            let output = BufReader::new(child.stdout.take().unwrap());
            *guard = Some(Process {
                child,
                input,
                output,
                python: python.into(),
            });
        }
        // Taking ownership ensures cancellation drops (and kills) the child;
        // a later request must never consume the cancelled request's response.
        let mut p = guard.take().unwrap();
        let work = async {
            p.input
                .write_all(format!("{}\n", json!({"method":method,"params":params})).as_bytes())
                .await
                .map_err(|_| "Worker input closed".to_string())?;
            p.input
                .flush()
                .await
                .map_err(|_| "Worker input closed".to_string())?;
            let mut line = String::new();
            p.output
                .read_line(&mut line)
                .await
                .map_err(|_| "Worker output closed".to_string())?;
            if line.len() > 32 * 1024 * 1024 {
                return Err("Worker response exceeded limit".into());
            }
            let response: Value =
                serde_json::from_str(&line).map_err(|_| "Invalid worker response".to_string())?;
            if let Some(error) = response.get("error") {
                return Err(error.as_str().unwrap_or("Worker operation failed").into());
            }
            Ok(response["result"].clone())
        };
        match tokio::time::timeout(Duration::from_millis(timeout_ms.clamp(500, 120000)), work).await
        {
            Ok(Ok(v)) => {
                *guard = Some(p);
                Ok(v)
            }
            other => {
                let _ = p.child.kill().await;
                match other {
                    Ok(Err(e)) => Err(e),
                    _ => Err("Worker timed out; cached completions remain available.".into()),
                }
            }
        }
    }
}
