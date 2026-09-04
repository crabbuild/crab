use std::io::{self, IoSlice, Write};
use std::sync::{Arc, Mutex};

use crate::error::OperationFailures;

// Xet may retain its writer after returning an error. Close our side before
// taking the failure snapshot, joining any in-progress write under this lock.
pub(super) struct OutputOwner<W>(Arc<Mutex<Option<W>>>);

impl<W: Write> OutputOwner<W> {
    pub(super) fn new(writer: W) -> Self {
        Self(Arc::new(Mutex::new(Some(writer))))
    }

    pub(super) fn writer(&self, failures: Arc<OperationFailures>) -> impl Write + Send + 'static
    where
        W: Send + 'static,
    {
        ObservedWriter {
            output: Arc::clone(&self.0),
            failures,
        }
    }
}

impl<W> Drop for OutputOwner<W> {
    fn drop(&mut self) {
        drop(
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
    }
}

struct ObservedWriter<W> {
    output: Arc<Mutex<Option<W>>>,
    failures: Arc<OperationFailures>,
}

impl<W: Write> ObservedWriter<W> {
    fn apply<T>(&mut self, action: impl FnOnce(&mut W) -> io::Result<T>) -> io::Result<T> {
        let mut output = self
            .output
            .lock()
            .map_err(|_| io::Error::other("output poisoned"))?;
        let writer = output.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "reconstruction output closed")
        })?;
        action(writer).map_err(|error| self.failures.writer_error(error))
    }
}

fn check_write(written: usize, nonempty: bool) -> io::Result<usize> {
    if written == 0 && nonempty {
        return Err(io::Error::from(io::ErrorKind::WriteZero));
    }
    Ok(written)
}

impl<W: Write> Write for ObservedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.apply(|writer| check_write(writer.write(bytes)?, !bytes.is_empty()))
    }

    fn write_vectored(&mut self, buffers: &[IoSlice<'_>]) -> io::Result<usize> {
        self.apply(|writer| {
            check_write(
                writer.write_vectored(buffers)?,
                buffers.iter().any(|b| !b.is_empty()),
            )
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.apply(Write::flush)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use xet_data::file_reconstruction::FileReconstructionError;

    #[test]
    fn closed_output_rejects_late_writes_without_changing_the_failure() {
        let failures = Arc::new(OperationFailures::default());
        let owner = OutputOwner::new(Vec::new());
        let mut writer = owner.writer(Arc::clone(&failures));
        writer.write_all(b"before close").unwrap();
        drop(owner);
        assert_eq!(
            writer.write(b"late").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(
            !failures
                .finish(FileReconstructionError::InternalError("stopped".into()))
                .has_writer_error()
        );
    }

    #[test]
    fn zero_nonempty_write_is_recorded_before_xet_can_replace_it() {
        let failures = Arc::new(OperationFailures::default());
        let owner = OutputOwner::new(io::Cursor::new([0_u8; 0]));
        let mut writer = owner.writer(Arc::clone(&failures));
        assert_eq!(writer.write(&[]).unwrap(), 0);
        assert_eq!(
            writer
                .write_vectored(&[IoSlice::new(b"data")])
                .unwrap_err()
                .kind(),
            io::ErrorKind::WriteZero
        );
        drop(owner);
        let error = failures.finish(FileReconstructionError::InternalWriterError(
            "stopped".into(),
        ));
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::WriteZero
        );
    }

    #[test]
    fn closing_output_waits_for_in_flight_failure_before_snapshot() {
        struct BlockingWriter {
            entered: std::sync::mpsc::SyncSender<()>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl Write for BlockingWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let failures = Arc::new(OperationFailures::default());
        let owner = OutputOwner::new(BlockingWriter {
            entered: entered_tx,
            release: release_rx,
        });
        let mut writer = owner.writer(Arc::clone(&failures));
        let write = std::thread::spawn(move || writer.write(b"data"));
        entered_rx.recv().unwrap();
        let close = std::thread::spawn(move || {
            drop(owner);
            failures.finish(FileReconstructionError::InternalWriterError(
                "stopped".into(),
            ))
        });
        release_tx.send(()).unwrap();
        assert!(write.join().unwrap().is_err());
        assert!(close.join().unwrap().has_writer_error());
    }
}
