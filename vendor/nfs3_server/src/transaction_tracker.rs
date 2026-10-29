use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// `TransactionTracker` tracks the state of transactions to detect retransmissions.
#[derive(Debug)]
pub struct TransactionTracker {
    retention_period: Duration,
    transactions: RwLock<HashMap<String, Arc<Mutex<ClientTransactions>>>>,
    max_active_transactions: u16,
    trim_limit: usize,
}

impl TransactionTracker {
    #[must_use]
    pub fn new(
        retention_period: Duration,
        max_active_transactions: u16,
        trim_limit: usize,
    ) -> Self {
        Self {
            retention_period,
            transactions: RwLock::new(HashMap::new()),
            max_active_transactions,
            trim_limit,
        }
    }

    pub(crate) fn start_transaction(
        &self,
        client_addr: &str,
        xid: u32,
        now: Instant,
    ) -> Result<TransactionLock, TransactionError> {
        // First, we check if client is already in the transactions map
        {
            let transactions = self
                .transactions
                .read()
                .expect("unable to unlock transactions mutex");

            if let Some(client_transactions) = transactions.get(client_addr) {
                let mut client_lock = client_transactions.lock().expect("lock is poisoned");
                client_lock.add_transaction(xid, now)?;
                return Ok(TransactionLock::new(
                    client_transactions.clone(),
                    xid,
                    self.retention_period,
                ));
            }
        }

        // If client is not in the transactions map, we need to add it
        // It's possible that another thread added it while we were checking, so we need to
        // check again
        let mut transactions = self.transactions.write().expect("lock is poisoned");

        let val = transactions
            .entry(client_addr.to_owned())
            .or_insert_with(|| self.new_client_transactions(now));

        let mut client_lock = val.lock().expect("lock is poisoned");
        client_lock.add_transaction(xid, now)?;

        Ok(TransactionLock::new(
            val.clone(),
            xid,
            self.retention_period,
        ))
    }

    fn new_client_transactions(&self, now: Instant) -> Arc<Mutex<ClientTransactions>> {
        Arc::new(Mutex::new(ClientTransactions::new(
            now,
            self.max_active_transactions,
            self.trim_limit,
        )))
    }

    pub(crate) fn cleanup(&self, now: Instant) {
        let mut transactions = self.transactions.write().expect("lock is poisoned");

        transactions.retain(|_, client_transactions| {
            let mut client_lock = client_transactions.lock().expect("lock is poisoned");
            if client_lock.is_active(now, self.retention_period) {
                client_lock.remove_old_transactions(now, self.retention_period);
                true
            } else {
                // If the client is not active, we remove it from the map
                false
            }
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionError {
    AlreadyExists,
    TooManyRequests,
}

impl std::fmt::Display for TransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyExists => write!(f, "transaction already exists"),
            Self::TooManyRequests => write!(f, "too many requests"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionState {
    InProgress,
    Completed(Instant),
}

#[derive(Debug)]
struct Transaction {
    xid: u32,
    state: TransactionState,
}

impl Transaction {
    const fn in_progress(xid: u32) -> Self {
        Self {
            xid,
            state: TransactionState::InProgress,
        }
    }

    fn complete(&mut self, now: Instant) {
        assert!(
            matches!(self.state, TransactionState::InProgress),
            "transaction is already completed"
        );
        self.state = TransactionState::Completed(now);
    }

    fn is_stale(&self, now: Instant, max_age: Duration) -> bool {
        match self.state {
            TransactionState::InProgress => false,
            TransactionState::Completed(tx_time) => now - tx_time > max_age,
        }
    }
}

#[derive(Debug)]
struct ClientTransactions {
    // Sorted by the xid of the transaction
    // In general, it's expected that transactions from the same host will be in order
    transactions: VecDeque<Transaction>,
    last_active: Instant,
    active_transactions: u16,
    max_active_transactions: u16,
    trim_limit: usize,
}

impl ClientTransactions {
    /// Creates a new `ClientTransactions` instance with the given parameters.
    /// `max_active_transactions` is the maximum number of transactions that can be active at the
    /// same time.
    /// `trim_limit` is the soft limit for number of transactions that can be kept in memory.
    /// `max_active_transactions` should be less than `trim_limit`.
    fn new(now: Instant, max_active_transactions: u16, trim_limit: usize) -> Self {
        assert!((max_active_transactions as usize) < trim_limit);
        Self {
            transactions: VecDeque::new(),
            last_active: now,
            active_transactions: 0,
            max_active_transactions,
            trim_limit,
        }
    }
    // Finds a transaction by its xid taking into account that the list is sorted
    #[allow(clippy::option_if_let_else)]
    fn find_transaction(&self, xid: u32) -> Result<usize, usize> {
        use std::cmp::Ordering;
        if let Some(last_tx) = self.transactions.back() {
            match last_tx.xid.cmp(&xid) {
                // transaction is the last one, so we can return it directly
                Ordering::Equal => Ok(self.transactions.len() - 1),
                // transaction does not exist, so we return the position where it should be inserted
                Ordering::Less => Err(self.transactions.len()),
                // transaction is not the last one, so we need to do a binary search
                Ordering::Greater => self.transactions.binary_search_by_key(&xid, |t| t.xid),
            }
        } else {
            // transaction list is empty
            Err(0)
        }
    }

    fn add_transaction(&mut self, xid: u32, now: Instant) -> Result<(), TransactionError> {
        self.last_active = now;
        match self.find_transaction(xid) {
            Ok(_) => Err(TransactionError::AlreadyExists),
            Err(p) => {
                if self.active_transactions >= self.max_active_transactions {
                    return Err(TransactionError::TooManyRequests);
                }
                self.active_transactions += 1;
                self.transactions.insert(p, Transaction::in_progress(xid));
                self.trim_if_needed();
                Ok(())
            }
        }
    }

    fn complete_transaction(&mut self, xid: u32, now: Instant) {
        self.last_active = now;
        if let Ok(p) = self.find_transaction(xid) {
            self.transactions[p].complete(now);
            self.active_transactions -= 1;
        } else {
            // transaction not found, do nothing
        }
    }

    /// Removes transactions older than the specified `max_age`, starting from the beginning of the
    /// list.
    fn remove_old_transactions(&mut self, now: Instant, max_age: Duration) {
        while let Some(tx) = self.transactions.front() {
            if tx.is_stale(now, max_age) {
                self.transactions.pop_front();
            } else {
                break;
            }
        }
    }

    fn is_active(&self, now: Instant, max_age: Duration) -> bool {
        if now - self.last_active < max_age {
            true
        } else {
            self.has_active_transactions(now, max_age)
        }
    }

    fn has_active_transactions(&self, now: Instant, max_age: Duration) -> bool {
        self.transactions
            .iter()
            .any(|tx| !tx.is_stale(now, max_age))
    }

    // Remove the oldest transactions until we are below the trim limit
    #[allow(clippy::unwrap_used)]
    fn trim_if_needed(&mut self) {
        while self.transactions.len() > self.trim_limit {
            if matches!(
                self.transactions.front().unwrap().state,
                TransactionState::InProgress
            ) {
                // If the transaction is still in progress, we can't remove it
                break;
            }
            self.transactions.pop_front();
        }
    }
}

#[derive(Debug)]
pub struct TransactionLock {
    transactions: Arc<Mutex<ClientTransactions>>,
    xid: u32,
    retention_period: Duration,
}

impl TransactionLock {
    const fn new(
        transactions: Arc<Mutex<ClientTransactions>>,
        xid: u32,
        retention_period: Duration,
    ) -> Self {
        Self {
            transactions,
            xid,
            retention_period,
        }
    }
}

impl Drop for TransactionLock {
    fn drop(&mut self) {
        let now = Instant::now();
        let mut transactions = self.transactions.lock().expect("lock is poisoned");
        transactions.complete_transaction(self.xid, now);
        transactions.remove_old_transactions(now, self.retention_period);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::significant_drop_tightening)]

    use super::*;

    #[test]
    fn test_transaction() {
        let mut transaction = Transaction::in_progress(1);
        assert_eq!(transaction.xid, 1);
        assert!(matches!(transaction.state, TransactionState::InProgress));

        let now = Instant::now();
        transaction.complete(now);
        assert!(matches!(transaction.state, TransactionState::Completed(_)));

        let max_age = Duration::new(1, 0);
        assert!(!transaction.is_stale(now, max_age));
        assert!(transaction.is_stale(now + max_age + Duration::new(1, 0), max_age));
    }

    #[test]
    fn test_client_transactions() {
        let now = Instant::now();
        let mut client_transactions = ClientTransactions::new(now, 100, 1000);

        assert_eq!(client_transactions.transactions.len(), 0);
        assert!(client_transactions.last_active.elapsed() < Duration::new(1, 0));

        client_transactions.add_transaction(1, now).unwrap();
        assert_eq!(client_transactions.transactions.len(), 1);
        assert_eq!(client_transactions.transactions[0].xid, 1);

        client_transactions.complete_transaction(1, now);
        assert_eq!(
            client_transactions.transactions[0].state,
            TransactionState::Completed(now)
        );

        client_transactions.remove_old_transactions(now + Duration::new(2, 0), Duration::new(1, 0));
        assert_eq!(client_transactions.transactions.len(), 0);
    }

    #[test]
    fn out_of_order_transactions() {
        let now = Instant::now();
        let mut client_transactions = ClientTransactions::new(now, 100, 1000);

        client_transactions.add_transaction(9, now).unwrap();
        client_transactions.add_transaction(1, now).unwrap();
        assert_eq!(collect_xids(&client_transactions)[..], [1, 9]);
        client_transactions.add_transaction(5, now).unwrap();
        assert_eq!(collect_xids(&client_transactions)[..], [1, 5, 9]);
        client_transactions.add_transaction(2, now).unwrap();
        assert_eq!(collect_xids(&client_transactions)[..], [1, 2, 5, 9]);
    }

    fn collect_xids(client: &ClientTransactions) -> Vec<u32> {
        client.transactions.iter().map(|t| t.xid).collect()
    }

    #[test]
    fn test_client_transactions_stale() {
        let now = Instant::now();
        let mut client_transactions = ClientTransactions::new(now, 100, 1000);

        client_transactions.add_transaction(1, now).unwrap();
        client_transactions.add_transaction(2, now).unwrap();
        client_transactions.complete_transaction(2, now);

        assert_eq!(client_transactions.transactions.len(), 2);
        assert_eq!(client_transactions.transactions[0].xid, 1);
        assert_eq!(client_transactions.transactions[1].xid, 2);
        assert!(client_transactions.transactions[0].state == TransactionState::InProgress);
        assert!(client_transactions.transactions[1].state == TransactionState::Completed(now));

        client_transactions.remove_old_transactions(now + Duration::new(2, 0), Duration::new(1, 0));
        assert_eq!(client_transactions.transactions.len(), 2);

        client_transactions.complete_transaction(1, now);
        assert_eq!(
            client_transactions.transactions[0].state,
            TransactionState::Completed(now)
        );
        assert_eq!(
            client_transactions.transactions[1].state,
            TransactionState::Completed(now)
        );
        client_transactions.remove_old_transactions(now + Duration::new(2, 0), Duration::new(1, 0));

        assert_eq!(client_transactions.transactions.len(), 0);
    }

    #[test]
    fn too_many_transactions() {
        let now = Instant::now();
        let mut client_transactions = ClientTransactions::new(now, 2, 1000);

        assert!(client_transactions.add_transaction(1, now).is_ok());
        assert!(client_transactions.add_transaction(2, now).is_ok());
        assert_eq!(
            client_transactions.add_transaction(3, now).unwrap_err(),
            TransactionError::TooManyRequests
        );
    }

    #[test]
    fn already_exists() {
        let now = Instant::now();
        let mut client_transactions = ClientTransactions::new(now, 100, 1000);

        assert!(client_transactions.add_transaction(1, now).is_ok());
        assert_eq!(
            client_transactions.add_transaction(1, now).unwrap_err(),
            TransactionError::AlreadyExists
        );
    }

    #[test]
    fn trim_limit() {
        let now = Instant::now();
        let mut client_transactions = ClientTransactions::new(now, 1, 2);

        assert!(client_transactions.add_transaction(1, now).is_ok());
        client_transactions.complete_transaction(1, now);

        assert!(client_transactions.add_transaction(2, now).is_ok());
        client_transactions.complete_transaction(2, now);
        assert_eq!(collect_xids(&client_transactions)[..], [1, 2]);

        assert!(client_transactions.add_transaction(3, now).is_ok());
        assert_eq!(collect_xids(&client_transactions)[..], [2, 3]);
        client_transactions.complete_transaction(3, now);

        assert!(client_transactions.add_transaction(4, now).is_ok());
        assert_eq!(collect_xids(&client_transactions)[..], [3, 4]);
    }

    #[test]
    fn test_transaction_tracker() {
        let tracker = TransactionTracker::new(Duration::new(1, 0), 100, 1000);
        let now = Instant::now();

        let transaction = tracker.start_transaction("client1", 1, now).unwrap();
        assert_eq!(
            tracker.start_transaction("client1", 1, now).unwrap_err(),
            TransactionError::AlreadyExists
        );
        assert_eq!(transaction.xid, 1);

        {
            let tracker_lock = tracker.transactions.read().unwrap();
            assert_eq!(tracker_lock.len(), 1);
            let client = tracker_lock.get("client1").unwrap();
            let client = client.lock().unwrap();
            assert_eq!(client.transactions.len(), 1);
            assert_eq!(client.transactions[0].xid, 1);
            assert_eq!(client.last_active, now);
            assert_eq!(client.transactions[0].state, TransactionState::InProgress);
        }

        drop(transaction);

        {
            let tracker_lock = tracker.transactions.read().unwrap();
            assert_eq!(tracker_lock.len(), 1);
            let client = tracker_lock.get("client1").unwrap();
            let client = client.lock().unwrap();
            assert_eq!(client.transactions.len(), 1);
            assert_eq!(client.transactions[0].xid, 1);
            assert!(client.last_active >= now);
            assert!(matches!(
                client.transactions[0].state,
                TransactionState::Completed(_)
            ));
        }
    }

    #[test]
    fn test_cleanup() {
        let tracker = TransactionTracker::new(Duration::new(1, 0), 100, 1000);
        let now = Instant::now();

        let transaction1 = tracker.start_transaction("client1", 1, now).unwrap();
        let transaction2 = tracker.start_transaction("client1", 2, now).unwrap();

        tracker.cleanup(now + Duration::new(2, 0));

        {
            let tracker_lock = tracker.transactions.read().unwrap();
            assert_eq!(tracker_lock.len(), 1);
            let client = tracker_lock.get("client1").unwrap();
            let client = client.lock().unwrap();
            assert_eq!(client.transactions.len(), 2);
        }

        tracker.cleanup(now + Duration::new(3, 0));

        {
            let tracker_lock = tracker.transactions.read().unwrap();
            assert_eq!(tracker_lock.len(), 1);
        }

        drop(transaction1);
        let now = Instant::now(); // drop updates the time

        tracker.cleanup(now + Duration::new(4, 0));

        {
            let tracker_lock = tracker.transactions.read().unwrap();
            assert_eq!(tracker_lock.len(), 1);
            let client = tracker_lock.get("client1").unwrap();
            let client = client.lock().unwrap();
            assert_eq!(client.transactions.len(), 1);
        }
        drop(transaction2);
        let now = Instant::now(); // drop updates the time

        tracker.cleanup(now + Duration::new(5, 0));

        {
            let tracker_lock = tracker.transactions.read().unwrap();
            assert_eq!(tracker_lock.len(), 0);
        }
    }
}
