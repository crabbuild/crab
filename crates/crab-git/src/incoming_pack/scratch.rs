use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Admission policy for the simultaneously live bytes in one private workspace.
pub trait ScratchAdmission: Send + Sync {
    /// Admit the new total before growth; rejection must retain the prior admission.
    fn resize(&self, bytes: u64) -> Result<(), ScratchError>;
}

/// Failure admitting private Git files before they grow.
#[derive(Debug, thiserror::Error)]
pub enum ScratchError {
    #[error("Git scratch requires {required} bytes, exceeding {capacity} capacity")]
    Capacity { required: u64, capacity: u64 },
    #[error("Git scratch admission failed")]
    Admission(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("Git scratch I/O failed")]
    Io(#[source] io::Error),
    #[error("Git scratch byte count overflowed")]
    Overflow,
    #[error("Git scratch accounting lock was poisoned")]
    Poisoned,
}

impl ScratchAdmission for u64 {
    fn resize(&self, bytes: u64) -> Result<(), ScratchError> {
        if bytes > *self {
            return Err(ScratchError::Capacity {
                required: bytes,
                capacity: *self,
            });
        }
        Ok(())
    }
}

/// Caller-owned volume and admission shared by quarantine and prepared artifacts.
#[derive(Clone)]
pub struct ScratchSpace {
    root: Arc<PathBuf>,
    state: Arc<State>,
}

struct State {
    bytes: Mutex<u64>,
    admission: Box<dyn ScratchAdmission>,
}

impl ScratchSpace {
    /// Bind an existing private directory to its byte-admission owner.
    pub fn new(directory: &Path, admission: impl ScratchAdmission + 'static) -> Self {
        Self {
            root: Arc::new(directory.to_owned()),
            state: Arc::new(State {
                bytes: Mutex::new(0),
                admission: Box::new(admission),
            }),
        }
    }

    /// Return the caller-owned parent directory.
    pub fn path(&self) -> &Path {
        &self.root
    }

    pub(super) fn directory(&self, parent: &Path, prefix: &str) -> io::Result<ScratchDirectory> {
        Ok(ScratchDirectory {
            directory: tempfile::Builder::new()
                .prefix(prefix)
                .tempdir_in(parent)
                .map_err(scratch_io)?,
            charge: Arc::new(Charge {
                space: self.clone(),
                bytes: Mutex::new(0),
            }),
        })
    }
}

struct Charge {
    space: ScratchSpace,
    bytes: Mutex<u64>,
}

impl Charge {
    fn add(&self, added: u64) -> io::Result<()> {
        if added == 0 {
            return Ok(());
        }
        let mut held = self
            .bytes
            .lock()
            .map_err(|_| io::Error::other(ScratchError::Poisoned))?;
        let mut total = self
            .space
            .state
            .bytes
            .lock()
            .map_err(|_| io::Error::other(ScratchError::Poisoned))?;
        let next = total
            .checked_add(added)
            .ok_or_else(|| io::Error::other(ScratchError::Overflow))?;
        self.space
            .state
            .admission
            .resize(next)
            .map_err(io::Error::other)?;
        *held += added;
        *total = next;
        Ok(())
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        let held = match self.bytes.get_mut() {
            Ok(held) => *held,
            Err(poisoned) => *poisoned.into_inner(),
        };
        let mut total = match self.space.state.bytes.lock() {
            Ok(total) => total,
            Err(poisoned) => poisoned.into_inner(),
        };
        *total -= held;
        // The owning directory has already removed its files. A rejected shrink
        // retains the caller's larger reservation until request cleanup.
        if let Err(error) = self.space.state.admission.resize(*total) {
            tracing::warn!(error = ?error, "Git scratch reservation shrink failed");
        }
    }
}

pub(super) struct ScratchDirectory {
    // Files disappear before their charge; writers must close before this owner.
    directory: tempfile::TempDir,
    charge: Arc<Charge>,
}

impl std::fmt::Debug for ScratchDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScratchDirectory")
            .field("path", &self.path())
            .finish_non_exhaustive()
    }
}

impl ScratchDirectory {
    pub(super) fn path(&self) -> &Path {
        self.directory.path()
    }
    pub(super) fn space(&self) -> &ScratchSpace {
        &self.charge.space
    }
    pub(super) fn reserve(&self, bytes: u64) -> io::Result<()> {
        self.charge.add(bytes)
    }
    pub(super) fn create(&self, name: &str) -> io::Result<ScratchFile> {
        Ok(ScratchFile {
            file: File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(self.path().join(name))
                .map_err(scratch_io)?,
            charge: Arc::clone(&self.charge),
            position: 0,
            length: 0,
        })
    }
}

pub(super) struct ScratchFile {
    file: File,
    charge: Arc<Charge>,
    position: u64,
    length: u64,
}

impl Write for ScratchFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other(ScratchError::Overflow))?;
        self.charge.add(end.saturating_sub(self.length))?;
        self.length = self.length.max(end);
        let count = self.file.write(bytes).map_err(scratch_io)?;
        self.position += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush().map_err(scratch_io)
    }
}

impl Read for ScratchFile {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.file.read(bytes).map_err(scratch_io)?;
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for ScratchFile {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.position = self.file.seek(position).map_err(scratch_io)?;
        Ok(self.position)
    }
}

fn scratch_io(source: io::Error) -> io::Error {
    // Distinguish private-file failure from malformed input at the HTTP owner.
    io::Error::other(ScratchError::Io(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simultaneous_files_reject_before_growth_and_overwrites_keep_their_charge() {
        let root = tempfile::tempdir().unwrap();
        let space = ScratchSpace::new(root.path(), 16_u64);
        let first = space.directory(root.path(), "first-").unwrap();
        let second = space.directory(root.path(), "second-").unwrap();
        let mut left = first.create("data").unwrap();
        let mut right = second.create("data").unwrap();
        left.write_all(&[1; 8]).unwrap();
        left.rewind().unwrap();
        left.write_all(&[2; 8]).unwrap();
        right.write_all(&[3; 8]).unwrap();
        assert!(right.write_all(&[4]).is_err());
        assert_eq!(right.file.metadata().unwrap().len(), 8);
        drop(right);
        drop(second);
        left.write_all(&[5; 8]).unwrap();
        drop(left);
        drop(first);
        assert_eq!(*space.state.bytes.lock().unwrap(), 0);
        assert_eq!(root.path().read_dir().unwrap().count(), 0);
    }
}
