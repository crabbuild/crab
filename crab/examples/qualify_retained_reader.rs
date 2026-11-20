use crab_cache::LocalCache;
use crab_cache_store::{CacheConfig, CachingStore};
use crab_metadata::file_index_lookup::SharedFileIndexLookup;
use crab_read::{
    ReadRuntimeBuilder,
    capsule_protocol::{CapsuleReadLimits, open_git_view_from_root},
};
use crab_storage::{
    Store, StoreLayout,
    provider_store::{ObjectStoreCredentials, build_object_store_with_endpoint},
};
use crab_types::pointer::Pointer;
use serde_json::json;
use std::{
    error::Error,
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

type Outcome<T> = Result<T, Box<dyn Error + Send + Sync>>;

async fn run() -> Outcome<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 7 {
        return Err("expected endpoint bucket prefix cache output pointer...".into());
    }
    let cache = PathBuf::from(&args[3]);
    let output = PathBuf::from(&args[4]);
    if cache.exists() || output.exists() {
        return Err("qualification requires fresh cache and output".into());
    }
    let mut pointers = Vec::new();
    for name in &args[5..] {
        let mut pointer = Pointer::parse(&std::fs::read(name)?)?;
        // Force selected dependency resolution through the captured catalog;
        // direct shard hints would bypass the immutable-root invariant.
        pointer.shard_hint = None;
        pointers.push(pointer);
    }
    let provider = build_object_store_with_endpoint(
        &args[1],
        ObjectStoreCredentials::Aws {
            access_key_id: std::env::var("AWS_ACCESS_KEY_ID")?,
            secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY")?,
            session_token: None,
            region: std::env::var("AWS_DEFAULT_REGION")?,
        },
        Some(&args[0]),
    )?;
    let requests = Arc::new(AtomicU64::new(0));
    let observed = requests.clone();
    let store = Store::new(provider.inner).with_read_request_observer(Arc::new(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    let layout = StoreLayout::new(store.clone(), args[2].clone());
    let root = crab_metadata::capsule_protocol::load_root(&layout).await?;
    let view = open_git_view_from_root(
        &layout,
        root,
        CapsuleReadLimits {
            max_capsule_bytes: u64::MAX,
            max_frontier_bytes: u64::MAX,
        },
    )
    .await?;
    let captured_digest = view.root().digest().to_owned();
    let catalog = view.pointer_catalog_lookup(&layout, u64::MAX)?;
    let lookup = SharedFileIndexLookup::for_pointer_catalog(layout.clone(), catalog);
    let operation: Outcome<()> = async {
        let idle_requests = requests.load(Ordering::SeqCst);
        println!("{}", json!({"event":"captured", "root_digest":captured_digest, "provider_requests":idle_requests, "files":pointers.len()}));
        io::stdout().flush()?;
        // No storage future exists at this boundary. Maintenance may exceed
        // the provider HTTP deadline without extending or disabling it.
        let mut command = String::new();
        io::stdin().read_line(&mut command)?;
        if command.trim() != "resume" { return Err("missing explicit resume".into()); }
        if requests.load(Ordering::SeqCst) != idle_requests { return Err("provider I/O occurred while held".into()); }
        std::fs::create_dir_all(&output)?;
        let local = Arc::new(LocalCache::with_limits(cache, Some(2 * 1024_u64.pow(3)), Some(2 * 1024_u64.pow(3))));
        let caching = CachingStore::new_with_local_cache(store, CacheConfig::default(), local)?;
        let hydrator = ReadRuntimeBuilder::new(caching, layout, 8).with_file_index_lookup(lookup.clone()).build()?;
        for (index, pointer) in pointers.iter().enumerate() {
            let bytes = hydrator.reconstruct_to_path(pointer, &output.join(format!("file-{index}.bin"))).await?;
            println!("{}", json!({"event":"reconstructed", "index":index, "bytes":bytes, "root_digest":captured_digest, "provider_requests":requests.load(Ordering::SeqCst)}));
            io::stdout().flush()?;
        }
        Ok(())
    }.await;
    let closed = lookup.close().await;
    if let Err(error) = closed {
        eprintln!("reader close failed: {error}");
        return Err(error.into());
    }
    operation?;
    println!(
        "{}",
        json!({"event":"closed", "root_digest":captured_digest})
    );
    Ok(())
}

fn main() -> Outcome<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(run())
}
