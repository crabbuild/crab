//! Shared, bounded bridge between blocking SQLite calls and asynchronous stores.

use crate::{CellWritableDatabase, CrabError, Result};
use std::{
    cell::Cell,
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, Weak, mpsc},
    time::{Duration, Instant},
};

type Pages = Vec<(u32, Vec<u8>)>;
pub(crate) type DriverSlot = Arc<Mutex<Weak<Driver>>>;
const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
    static ORIGIN: Cell<crate::LtxReadOrigin> = const { Cell::new(crate::LtxReadOrigin::Sparse) };
}

struct DeadlineGuard(Option<Instant>);

impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        DEADLINE.set(self.0);
    }
}

struct OriginGuard(crate::LtxReadOrigin);

impl Drop for OriginGuard {
    fn drop(&mut self) {
        ORIGIN.set(self.0);
    }
}

/// Applies one absolute deadline to sparse page faults on the current thread.
///
/// Call this around SQLite work on its owning thread. Nested scopes retain the
/// earliest deadline. The deadline stops the synchronous VFS wait but does not
/// cancel a provider request that has already been accepted by the I/O driver.
pub fn with_paged_io_deadline<T>(deadline: Instant, operation: impl FnOnce() -> T) -> T {
    let previous = DEADLINE.get();
    DEADLINE.set(Some(
        previous.map_or(deadline, |current| current.min(deadline)),
    ));
    let _guard = DeadlineGuard(previous);
    operation()
}

pub(crate) fn with_paged_io_origin<T>(
    origin: crate::LtxReadOrigin,
    operation: impl FnOnce() -> T,
) -> T {
    let previous = ORIGIN.replace(origin);
    let _guard = OriginGuard(previous);
    operation()
}

fn deadline() -> Instant {
    DEADLINE
        .get()
        .unwrap_or_else(|| Instant::now() + DEFAULT_DEADLINE)
}

#[derive(Clone)]
pub(crate) enum Database {
    Cell(CellWritableDatabase),
    Snapshot(crate::CellPagedDatabase),
}

impl Database {
    pub(crate) fn read_only(&self) -> bool {
        matches!(self, Self::Snapshot(_))
    }

    pub(crate) fn host(&self) -> crate::Host {
        match self {
            Self::Cell(database) => database.host(),
            Self::Snapshot(database) => database.host(),
        }
    }

    pub(crate) fn limits(&self) -> crate::Limits {
        match self {
            Self::Cell(database) => database.limits(),
            Self::Snapshot(database) => database.limits(),
        }
    }

    pub(crate) fn page_size(&self) -> u32 {
        match self {
            Self::Cell(database) => database.page_size(),
            Self::Snapshot(database) => database.page_size(),
        }
    }

    pub(crate) fn page_count(&self) -> u32 {
        match self {
            Self::Cell(database) => database.page_count(),
            Self::Snapshot(database) => database.page_count(),
        }
    }

    pub(crate) fn position(&self) -> crate::Position {
        match self {
            Self::Cell(database) => database.position(),
            Self::Snapshot(database) => database.position(),
        }
    }

    pub(crate) fn checksums(&self) -> Result<crate::pages::PageChecksums> {
        match self {
            Self::Cell(database) => Ok(database.checksums()),
            Self::Snapshot(_) => Err(CrabError::InvalidState("snapshot has no capture index")),
        }
    }

    async fn read_run(
        &self,
        first: u32,
        max_pages: u32,
        origin: crate::LtxReadOrigin,
    ) -> Result<Pages> {
        match self {
            Self::Cell(database) => database.read_run(first, max_pages, origin).await,
            Self::Snapshot(database) => database.read_run(first, max_pages, origin).await,
        }
    }
}

struct Request {
    database: Database,
    view: u64,
    page: u32,
    origin: crate::LtxReadOrigin,
    deadline: Instant,
    reply: mpsc::SyncSender<Result<Vec<u8>>>,
}

#[derive(Default)]
struct Cache {
    pages: HashMap<(u64, u32), Vec<u8>>,
    order: VecDeque<(u64, u32)>,
    bytes: usize,
}

impl Cache {
    fn insert(&mut self, view: u64, pages: Pages) {
        for (page, bytes) in pages {
            let key = (view, page);
            if self.pages.contains_key(&key) {
                continue;
            }
            while self.bytes + bytes.len() > 8 << 20 {
                let Some(old) = self.order.pop_front() else {
                    break;
                };
                if let Some(bytes) = self.pages.remove(&old) {
                    self.bytes -= bytes.len();
                }
            }
            self.bytes += bytes.len();
            self.pages.insert(key, bytes);
            self.order.push_back(key);
        }
    }
}

pub(crate) struct Driver {
    sender: Option<tokio::sync::mpsc::Sender<Request>>,
    cache: Arc<Mutex<Cache>>,
    worker: Option<Box<dyn crate::environment::Worker>>,
}

impl Driver {
    fn new(host: &crate::Host) -> Result<Arc<Self>> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Request>(256);
        let (ready, started) = mpsc::sync_channel(1);
        let cache = Arc::new(Mutex::new(Cache::default()));
        let shared_cache = cache.clone();
        let worker = host.executor.start_worker(Box::new(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            if ready.send(Ok(())).is_err() {
                return;
            }
            // The runtime must keep polling while SQL is idle: provider clients
            // retain pooled connections whose drivers were spawned here.
            runtime.block_on(async move {
                let mut jobs = tokio::task::JoinSet::new();
                let mut closed = false;
                while !closed || !jobs.is_empty() {
                    tokio::select! {
                        request = receiver.recv(), if !closed && jobs.len() < 32 => {
                            match request {
                                Some(request) => {
                                    let cache = shared_cache.clone();
                                    jobs.spawn(async move {
                                        let result = fetch(&request, &cache).await;
                                        let _ = request.reply.send(result);
                                    });
                                }
                                None => closed = true,
                            }
                        }
                        _ = jobs.join_next(), if !jobs.is_empty() => {}
                    }
                }
            });
        }))?;
        let driver = Arc::new(Self {
            sender: Some(sender),
            cache,
            worker: Some(worker),
        });
        started
            .recv()
            .map_err(|e| CrabError::Other(Box::new(e)))??;
        Ok(driver)
    }
}

async fn fetch(request: &Request, cache: &Mutex<Cache>) -> Result<Vec<u8>> {
    let deadline = tokio::time::Instant::from_std(request.deadline);
    let pages = tokio::time::timeout_at(
        deadline,
        request.database.read_run(request.page, 64, request.origin),
    )
    .await
    .map_err(|_| CrabError::Deadline)??;
    let mut cache = cache
        .lock()
        .map_err(|_| CrabError::InvalidState("paged cache poisoned"))?;
    cache.insert(request.view, pages);
    cache
        .pages
        .get(&(request.view, request.page))
        .cloned()
        .ok_or(CrabError::LTXCorrupted)
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) struct Io {
    driver: Arc<Driver>,
    database: Database,
    view: u64,
    gate: Mutex<()>,
}

impl Io {
    pub(crate) fn new(database: Database) -> Result<Self> {
        let host = database.host();
        let mut slot = host
            .paged_driver
            .lock()
            .map_err(|_| CrabError::InvalidState("paged driver slot poisoned"))?;
        let driver = match slot.upgrade() {
            Some(driver) => driver,
            None => {
                let driver = Driver::new(&host)?;
                *slot = Arc::downgrade(&driver);
                driver
            }
        };
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Ok(Self {
            driver,
            database,
            view: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            gate: Mutex::new(()),
        })
    }

    pub(crate) fn page(&self, page: u32) -> Result<Vec<u8>> {
        let origin = ORIGIN.get();
        self.database.host().observe_ltx_logical_read(origin);
        let _gate = self
            .gate
            .lock()
            .map_err(|_| CrabError::InvalidState("paged request gate poisoned"))?;
        if let Some(bytes) = self
            .driver
            .cache
            .lock()
            .map_err(|_| CrabError::InvalidState("paged cache poisoned"))?
            .pages
            .get(&(self.view, page))
            .cloned()
        {
            return Ok(bytes);
        }
        let (reply, response) = mpsc::sync_channel(1);
        let deadline = deadline();
        let request = Request {
            database: self.database.clone(),
            view: self.view,
            page,
            origin,
            deadline,
            reply,
        };
        self.driver
            .sender
            .as_ref()
            .ok_or(CrabError::InvalidState("paged I/O closed"))?
            .try_send(request)
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    CrabError::Limit(crate::LimitKind::PagedRequestQueue)
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    CrabError::InvalidState("paged I/O closed")
                }
            })?;
        receive(response, deadline)
    }
}

fn receive(response: mpsc::Receiver<Result<Vec<u8>>>, deadline: Instant) -> Result<Vec<u8>> {
    match response.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(CrabError::Deadline),
        Err(error @ mpsc::RecvTimeoutError::Disconnected) => Err(CrabError::Other(Box::new(error))),
    }
}

pub(crate) fn default_slot() -> DriverSlot {
    static SLOT: std::sync::OnceLock<DriverSlot> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Arc::new(Mutex::new(Weak::new())))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_deadline_keeps_earliest_value_and_restores_the_thread() {
        let outer = Instant::now() + Duration::from_secs(2);
        let later = outer + Duration::from_secs(1);
        let earlier = outer - Duration::from_secs(1);
        with_paged_io_deadline(outer, || {
            assert_eq!(deadline(), outer);
            with_paged_io_deadline(later, || assert_eq!(deadline(), outer));
            with_paged_io_deadline(earlier, || assert_eq!(deadline(), earlier));
            assert_eq!(deadline(), outer);
        });
        assert!(deadline() >= Instant::now() + Duration::from_secs(29));
    }

    #[test]
    fn scoped_origin_restores_sparse_default() {
        assert_eq!(ORIGIN.get(), crate::LtxReadOrigin::Sparse);
        with_paged_io_origin(crate::LtxReadOrigin::Hydrating, || {
            assert_eq!(ORIGIN.get(), crate::LtxReadOrigin::Hydrating);
        });
        assert_eq!(ORIGIN.get(), crate::LtxReadOrigin::Sparse);
    }

    #[test]
    fn blocking_page_wait_stops_at_its_deadline() {
        let (reply, response) = mpsc::sync_channel(1);
        let started = Instant::now();
        let result = receive(response, started + Duration::from_millis(20));
        drop(reply);
        assert!(matches!(result, Err(CrabError::Deadline)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cache_bounds_payload_and_isolates_pinned_views() {
        let mut cache = Cache::default();
        for view in 0..16 {
            cache.insert(view, vec![(1, vec![view as u8; 1 << 20])]);
        }
        assert_eq!(cache.bytes, 8 << 20);
        assert_eq!(cache.pages.len(), 8);
        assert_eq!(cache.order.len(), 8);
        for view in 0..16 {
            match cache.pages.get(&(view, 1)) {
                Some(bytes) => {
                    assert!(view >= 8);
                    assert!(bytes.iter().all(|byte| *byte == view as u8));
                }
                None => assert!(view < 8),
            }
        }
        cache.insert(15, vec![(1, vec![99; 1 << 20])]);
        assert_eq!(cache.bytes, 8 << 20);
        assert_eq!(cache.pages[&(15, 1)][0], 15);
    }
}
