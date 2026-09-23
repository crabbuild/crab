//! Cell-scoped LTX replication tests: exact roots, parallel restore, local replication.

#![cfg(feature = "replica")]

mod cell {
    pub mod replication;
    pub mod restore;
    pub mod roots;
}
