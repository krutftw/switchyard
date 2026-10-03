use crate::{
    AdapterError, ProfileBinding, Result,
    process_tree::{self, ProcessTree},
};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::mpsc,
};

const MAX_FRAME: usize = 256 * 1024;
const MAX_STREAM: usize = 2 * 1024 * 1024;
pub(crate) struct AccountProcess {
    pub(crate) child: Child,
    tree: ProcessTree,
    stdin: ChildStdin,
    incoming: mpsc::Receiver<Result<(bool, String)>>,
    readers: Vec<tokio::task::JoinHandle<()>>,
}

impl AccountProcess {
    pub(crate) fn spawn(
        executable: PathBuf,
        args: &[&str],
        profile: &ProfileBinding,
        capture_stderr: bool,
    ) -> Result<Self> {
        let mut command = Command::new(executable);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if profile.managed {
            command.current_dir(&profile.home);
        }
        crate::profile::configure(&mut command, Some(profile));
        process_tree::configure(&mut command);
        let mut child = command
            .spawn()
            .map_err(|_| AdapterError::Unavailable("The account CLI could not start.".into()))?;
        let tree = ProcessTree::attach(&child).map_err(|_| {
            AdapterError::Unavailable("Account process cleanup could not be established.".into())
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AdapterError::Unavailable("Account input is unavailable.".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AdapterError::Unavailable("Account output is unavailable.".into()))?;
        let stderr = child.stderr.take().ok_or_else(|| {
            AdapterError::Unavailable("Account diagnostic stream is unavailable.".into())
        })?;
        let (sender, incoming) = mpsc::channel(16);
        let readers = vec![
            tokio::spawn(read_lines(stdout, sender.clone(), false, true)),
            tokio::spawn(read_lines(stderr, sender, true, capture_stderr)),
        ];
        Ok(Self {
            child,
            tree,
            stdin,
            incoming,
            readers,
        })
    }

    pub(crate) async fn write(&mut self, value: &Value) -> Result<()> {
        let mut data = serde_json::to_vec(value)
            .map_err(|_| AdapterError::Protocol("Invalid account request.".into()))?;
        data.push(b'\n');
        self.stdin
            .write_all(&data)
            .await
            .map_err(|_| AdapterError::Protocol("Account CLI input closed.".into()))?;
        self.stdin
            .flush()
            .await
            .map_err(|_| AdapterError::Protocol("Account CLI input closed.".into()))
    }

    pub(crate) async fn line(&mut self) -> Result<Option<(bool, String)>> {
        self.incoming.recv().await.transpose()
    }

    pub(crate) async fn message(&mut self) -> Result<Value> {
        loop {
            let Some((stderr, line)) = self.line().await? else {
                return Err(AdapterError::Protocol(
                    "Account CLI exited before confirming the request.".into(),
                ));
            };
            if stderr || line.trim().is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(&line)
                .map_err(|_| AdapterError::Protocol("Account CLI returned invalid JSON.".into()))?;
            if message.get("method").is_some() && message.get("id").is_some() {
                self.write(&json!({"id":message["id"],"error":{"code":-32601,"message":"Unsupported account operation"}})).await?;
                continue;
            }
            return Ok(message);
        }
    }

    pub(crate) async fn rpc(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        self.write(&json!({"id":id,"method":method,"params":params}))
            .await?;
        loop {
            let message = self.message().await?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if message.get("error").is_some() {
                let code = message.pointer("/error/code").and_then(Value::as_i64);
                return Err(AdapterError::Protocol(format!(
                    "The account CLI rejected a request (code {code:?})."
                )));
            }
            return message.get("result").cloned().ok_or_else(|| {
                AdapterError::Protocol("Account CLI returned an incomplete response.".into())
            });
        }
    }

    pub(crate) async fn initialize(&mut self, profile: &ProfileBinding) -> Result<()> {
        let result = self.rpc(1, "initialize", json!({"clientInfo":{"name":"switchya_accounts","title":"Switchya","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":false,"explicitGatewayOauth":true}})).await?;
        if !crate::profile::confirmed_home(&result, Some(profile)) {
            return Err(AdapterError::Protocol(
                "Codex did not confirm the selected account directory.".into(),
            ));
        }
        self.write(&json!({"method":"initialized"})).await
    }

    pub(crate) async fn stop(&mut self) {
        let _ = self.tree.terminate();
        let _ = tokio::time::timeout(Duration::from_millis(500), self.child.wait()).await;
        for reader in &self.readers {
            reader.abort();
        }
    }
}

impl Drop for AccountProcess {
    fn drop(&mut self) {
        let _ = self.tree.terminate();
        for reader in &self.readers {
            reader.abort();
        }
    }
}

async fn read_lines<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    sender: mpsc::Sender<Result<(bool, String)>>,
    stderr: bool,
    capture: bool,
) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let mut total = 0usize;
    loop {
        let available = match reader.fill_buf().await {
            Ok(bytes) => bytes,
            Err(_) => {
                let _ = sender
                    .send(Err(AdapterError::Protocol(
                        "Account CLI stream failed.".into(),
                    )))
                    .await;
                return;
            }
        };
        if available.is_empty() {
            if capture && !line.is_empty() {
                let _ = sender
                    .send(Ok((stderr, String::from_utf8_lossy(&line).into_owned())))
                    .await;
            }
            return;
        }
        let count = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1);
        total = total.saturating_add(count);
        if total > MAX_STREAM || line.len().saturating_add(count) > MAX_FRAME {
            let _ = sender
                .send(Err(AdapterError::Limit(
                    "Account CLI exceeded its output limit.".into(),
                )))
                .await;
            return;
        }
        let complete = available[count - 1] == b'\n';
        if capture {
            line.extend_from_slice(&available[..count]);
        }
        reader.consume(count);
        if complete && capture {
            let text = match String::from_utf8(std::mem::take(&mut line)) {
                Ok(text) => text,
                Err(_) => {
                    let _ = sender
                        .send(Err(AdapterError::Protocol(
                            "Account CLI returned invalid text.".into(),
                        )))
                        .await;
                    return;
                }
            };
            if sender.send(Ok((stderr, text))).await.is_err() {
                return;
            }
        }
    }
}
