//! Shared, bounded bridge between blocking SQLite calls and asynchronous stores.

use crate::{CrabError, PagedDatabase, Result};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, Weak, mpsc},
    time::{Duration, Instant},
};

type Pages = Vec<(u32, Vec<u8>)>;
pub(crate) type DriverSlot = Arc<Mutex<Weak<Driver>>>;

struct Request {
    database: PagedDatabase,
    view: u64,
    page: u32,
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
    let pages = tokio::time::timeout_at(deadline, request.database.read_run(request.page, 64))
        .await
        .map_err(|e| CrabError::Other(Box::new(e)))??;
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
    database: PagedDatabase,
    view: u64,
    gate: Mutex<()>,
}

impl Io {
    pub(crate) fn new(database: PagedDatabase) -> Result<Self> {
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
        let request = Request {
            database: self.database.clone(),
            view: self.view,
            page,
            deadline: Instant::now() + Duration::from_secs(30),
            reply,
        };
        self.driver
            .sender
            .as_ref()
            .ok_or(CrabError::InvalidState("paged I/O closed"))?
            .try_send(request)
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    CrabError::Limit("paged request queue")
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    CrabError::InvalidState("paged I/O closed")
                }
            })?;
        response.recv().map_err(|e| CrabError::Other(Box::new(e)))?
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
