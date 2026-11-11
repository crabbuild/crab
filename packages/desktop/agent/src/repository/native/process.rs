use super::*;

pub(super) struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr: JoinHandle<std::io::Result<Vec<u8>>>,
    closed: bool,
    receipts: Receipts,
    query_id: u64,
}

impl Process {
    pub(super) async fn spawn(
        root: &Path,
        options: Option<&DirectStoreOptions>,
        read_progress: ReadProgress,
    ) -> Result<Self> {
        let mut command = tokio::process::Command::new(shellout::resolve_crab_bin());
        if let Some(options) = options {
            options.configure_command(command.as_std_mut())?;
        }
        let mut child = command
            .arg("read-pointer")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (Some(stdin), Some(stdout), Some(mut stderr)) = pipes else {
            child.kill().await?;
            return Err(AgentError::Other("native reader pipes unavailable".into()));
        };
        let stderr = tokio::spawn(async move {
            let mut bytes = Vec::new();
            (&mut stderr)
                .take(64 * 1024)
                .read_to_end(&mut bytes)
                .await?;
            tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await?;
            Ok(bytes)
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout).lines(),
            stderr,
            closed: false,
            receipts: Receipts {
                received_bytes: 0,
                idle: read_progress,
            },
            query_id: 0,
        })
    }

    pub(super) async fn open(
        &mut self,
        request: Request,
        selected: Option<&Connection>,
        stop: &CancellationToken,
        id: &str,
    ) -> Result<u64> {
        self.send(request).await?;
        let response = tokio::select! {
            () = stop.cancelled() => return Err(cancelled(id)),
            result = tokio::time::timeout(READ_TIMEOUT, response(&mut self.stdout, &mut self.receipts, None)) => result.map_err(|_| AgentError::Other("native reader open timed out".into()))??,
        };
        let Response::Ready { size, remote_url } = response else {
            return Err(unexpected(response));
        };
        if let (Some(selected), Some(remote)) = (selected, remote_url) {
            selected.repository_prefix(&remote)?;
        }
        Ok(size)
    }

    pub(super) async fn serve(
        &mut self,
        mut receive: mpsc::Receiver<Query>,
        stop: &CancellationToken,
        id: &str,
    ) -> Result<()> {
        loop {
            let request = tokio::select! {
                biased;
                () = stop.cancelled() => return Ok(()),
                request = receive.recv() => request,
                result = response(&mut self.stdout, &mut self.receipts, None) => return Err(unexpected(result?)),
            };
            let Some(Query {
                request,
                mut reply,
                cancel,
                read_progress,
            }) = request
            else {
                return Ok(());
            };
            if reply.is_closed() || cancel.is_cancelled() {
                let _ = reply.send(Err(cancelled(id)));
                continue;
            }
            let result = self
                .query(request, &mut reply, stop, &cancel, id, &read_progress)
                .await;
            match result {
                Err(error)
                    if !matches!(
                        error,
                        AgentError::NativeRead { .. } | AgentError::OpCancelled { .. }
                    ) || stop.is_cancelled()
                        && !matches!(error, AgentError::OpCancelled { .. }) =>
                {
                    let error = Arc::new(error);
                    let _ = reply.send(Err(AgentError::Shared(error.clone())));
                    return Err(AgentError::Shared(error));
                }
                result => {
                    let _ = reply.send(result);
                }
            }
            if stop.is_cancelled() {
                return Ok(());
            }
        }
    }

    async fn query(
        &mut self,
        request: Request,
        reply: &mut oneshot::Sender<Result<Response>>,
        stop: &CancellationToken,
        cancel: &CancellationToken,
        id: &str,
        read_progress: &ReadProgress,
    ) -> Result<Response> {
        // Both peers count only range/chunk commands actually sent. Advancing
        // for an unsent cancellation would attribute the next frame incorrectly.
        self.query_id = self
            .query_id
            .checked_add(1)
            .ok_or_else(|| AgentError::Other("native reader query counter overflow".into()))?;
        self.send(request).await?;
        let mut timed_out = false;
        let next = response(
            &mut self.stdout,
            &mut self.receipts,
            Some((self.query_id, read_progress)),
        );
        tokio::pin!(next);
        let result = tokio::select! {
            biased;
            result = &mut next => result,
            () = stop.cancelled() => {
                self.stdin.take();
                tokio::time::timeout(CLOSE_TIMEOUT, next).await.map_err(|_| AgentError::Other("native reader did not acknowledge close".into()))?
            },
            () = cancel.cancelled() => {
                send(&mut self.stdin, Request::Cancel).await?;
                tokio::time::timeout(CLOSE_TIMEOUT, next).await.map_err(|_| AgentError::Other("native reader did not acknowledge cancellation".into()))?
            },
            () = reply.closed() => {
                send(&mut self.stdin, Request::Cancel).await?;
                tokio::time::timeout(CLOSE_TIMEOUT, next).await.map_err(|_| AgentError::Other("native reader did not acknowledge cancellation".into()))?
            },
            () = tokio::time::sleep(READ_TIMEOUT) => {
                send(&mut self.stdin, Request::Cancel).await?;
                timed_out = true;
                tokio::time::timeout(CLOSE_TIMEOUT, next).await.map_err(|_| AgentError::Other("native reader did not acknowledge cancellation".into()))?
            },
        }?;
        let result = match result {
            reply @ (Response::Data { .. } | Response::Chunks { .. }) => Ok(reply),
            Response::Cancelled => Err(cancelled(id)),
            Response::Closed if stop.is_cancelled() => {
                self.closed = true;
                Err(cancelled(id))
            }
            Response::Failed { code, message } => {
                if stop.is_cancelled() {
                    self.closed = true;
                }
                Err(AgentError::NativeRead { code, message })
            }
            other => Err(unexpected(other)),
        };
        if timed_out && matches!(&result, Ok(_) | Err(AgentError::OpCancelled { .. })) {
            return Err(AgentError::Other(format!(
                "native read exceeded {} seconds",
                READ_TIMEOUT.as_secs()
            )));
        }
        result
    }

    async fn send(&mut self, request: Request) -> Result<()> {
        send(&mut self.stdin, request).await
    }

    pub(super) async fn finish(mut self) -> Result<()> {
        self.stdin.take();
        let drain = async {
            let mut failure = None;
            while !self.closed {
                match response(&mut self.stdout, &mut self.receipts, None).await {
                    Ok(Response::Closed) => self.closed = true,
                    Ok(Response::Failed { code, message }) => {
                        failure = Some(AgentError::NativeRead { code, message });
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            // Do not short-circuit on a protocol/wait error: stderr still owns
            // a task and the child must be reaped before returning a close result.
            let status = self.child.wait().await;
            let stderr = (&mut self.stderr).await;
            let status = status?;
            let stderr = stderr.map_err(std::io::Error::other)??;
            if !status.success() {
                return Err(AgentError::Cli {
                    status: status.code().unwrap_or(-1),
                    stderr: String::from_utf8_lossy(&stderr).into_owned(),
                });
            }
            if let Some(error) = failure {
                return Err(error);
            }
            Ok(())
        };
        match tokio::time::timeout(CLOSE_TIMEOUT, drain).await {
            Ok(result) => result,
            Err(_) => {
                let kill = self.child.kill().await;
                let stderr = self.stderr.await;
                kill?;
                stderr.map_err(std::io::Error::other)??;
                Err(AgentError::Other(
                    "native reader cleanup timed out; process terminated".into(),
                ))
            }
        }
    }
}

async fn send(stdin: &mut Option<ChildStdin>, request: Request) -> Result<()> {
    let stdin = stdin
        .as_mut()
        .ok_or_else(|| AgentError::Other("native reader input is closed".into()))?;
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    stdin.write_all(&bytes).await?;
    stdin.flush().await?;
    Ok(())
}

struct Receipts {
    received_bytes: u64,
    idle: ReadProgress,
}

impl Receipts {
    fn observe(
        &mut self,
        query: Option<u64>,
        received_bytes: u64,
        active: Option<(u64, &ReadProgress)>,
    ) -> Result<()> {
        let progress = match (query, active) {
            (None, _) => &self.idle,
            (Some(query), Some((expected, progress))) if query == expected => progress,
            _ => {
                return Err(AgentError::Other(
                    "native reader attributed traffic to an unexpected query".into(),
                ));
            }
        };
        let bytes = received_bytes
            .checked_sub(self.received_bytes)
            .ok_or_else(|| AgentError::Other("native reader traffic counter regressed".into()))?;
        progress.record_received(bytes);
        self.received_bytes = received_bytes;
        Ok(())
    }
}

async fn response(
    stdout: &mut Lines<BufReader<ChildStdout>>,
    receipts: &mut Receipts,
    active: Option<(u64, &ReadProgress)>,
) -> Result<Response> {
    loop {
        match raw_response(stdout).await? {
            Response::ReadProgress {
                query,
                received_bytes,
            } => receipts.observe(query, received_bytes, active)?,
            response => return Ok(response),
        }
    }
}

async fn raw_response(stdout: &mut Lines<BufReader<ChildStdout>>) -> Result<Response> {
    let line = stdout
        .next_line()
        .await?
        .ok_or_else(|| AgentError::Other("native reader closed its response pipe".into()))?;
    if line.len() > 12 * 1024 * 1024 {
        return Err(AgentError::Other(
            "native reader response exceeded its range limit".into(),
        ));
    }
    Ok(serde_json::from_str(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_background_receipts_never_inflate_a_later_query() {
        let (idle, _) = ReadProgress::channel();
        let (first, _) = ReadProgress::channel();
        let (second, _) = ReadProgress::channel();
        let mut receipts = Receipts {
            received_bytes: 0,
            idle: idle.clone(),
        };
        receipts.observe(None, 8, None).unwrap();
        receipts.observe(Some(1), 20, Some((1, &first))).unwrap();
        receipts.observe(None, 23, Some((2, &second))).unwrap();
        receipts.observe(Some(2), 30, Some((2, &second))).unwrap();
        assert_eq!(
            (
                idle.received_bytes(),
                first.received_bytes(),
                second.received_bytes()
            ),
            (11, 12, 7)
        );
    }

    #[test]
    fn invalid_receipt_ownership_and_regression_leave_counters_unchanged() {
        let (idle, _) = ReadProgress::channel();
        let (active, _) = ReadProgress::channel();
        let mut receipts = Receipts {
            received_bytes: 10,
            idle: idle.clone(),
        };
        assert!(receipts.observe(Some(1), 12, Some((2, &active))).is_err());
        assert!(receipts.observe(None, 9, None).is_err());
        assert_eq!(
            (
                idle.received_bytes(),
                active.received_bytes(),
                receipts.received_bytes
            ),
            (0, 0, 10)
        );
    }
}
