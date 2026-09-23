//! Distributed primitive integration tests: SQL, KV, Blob, Cron, Queue, Workflow.

mod support;

mod primitives {
    pub mod blob_cron;
    pub mod cron_api;
    pub mod kv;
    pub mod queue;
    pub mod sql;
    pub mod workflow;
    pub mod workflow_activity_codec;
    pub mod workflow_api;
}
