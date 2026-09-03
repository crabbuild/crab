//! Mirror command policy over the shared owned Git process runner.

use super::{
    CancellationToken, CommandRunner, OutputMode, ProcessCommand, ProcessOutput, ProcessStatus,
    Result, check_cancelled, replay_stderr, replay_stdout,
};
use crate::git::process::{self, MAX_CAPTURE_BYTES, capture_output};
use std::io::Write as _;
use std::process::{ChildStdin, Command, ExitStatus};

pub(super) struct SystemCommandRunner {
    cancel: CancellationToken,
}

impl SystemCommandRunner {
    pub(super) fn new(cancel: CancellationToken) -> Self {
        Self { cancel }
    }
}

#[cfg(test)]
impl Default for SystemCommandRunner {
    fn default() -> Self {
        Self::new(CancellationToken::new())
    }
}

impl CommandRunner for SystemCommandRunner {
    fn run(&mut self, command: &ProcessCommand, mode: OutputMode) -> Result<ProcessOutput> {
        let mut process = Command::new(&command.program);
        process.args(&command.args);
        if let Some(path) = &command.current_dir {
            process.current_dir(path);
        }
        for key in &command.env_remove {
            process.env_remove(key);
        }
        process.envs(command.envs.iter().map(|(key, value)| (key, value)));
        let input = command.stdin.is_some() || !command.verify_blobs.is_empty();
        let write_stdin = input.then_some(|mut stdin: ChildStdin| {
            if let Some(input) = &command.stdin {
                stdin.write_all(input.as_bytes())?;
            }
            for blob in &command.verify_blobs {
                check_cancelled(&self.cancel)?;
                writeln!(stdin, "{}", gix_hash::ObjectId::Sha1(blob.oid))?;
            }
            Ok(())
        });
        let read_stdout = |stdout| {
            if command.verify_blobs.is_empty() {
                return capture_output(stdout, MAX_CAPTURE_BYTES).map_err(Into::into);
            }
            crab_git::batch::verify_blob_batch(stdout, &command.verify_blobs, &|| {
                self.cancel.is_cancelled()
            })?;
            Ok(Vec::new())
        };
        let output = process::run(process, &self.cancel, write_stdin, read_stdout)?;
        let output = process_output(output.status, output.stdout, output.stderr);
        if mode == OutputMode::Text && command.replay_output {
            replay_stdout(&output.stdout);
            replay_stderr(&output.stderr);
        }
        Ok(output)
    }
}

fn process_output(status: ExitStatus, stdout: Vec<u8>, stderr: Vec<u8>) -> ProcessOutput {
    ProcessOutput {
        status: ProcessStatus {
            success: status.success(),
            code: status.code(),
        },
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
}

#[cfg(test)]
mod tests;
