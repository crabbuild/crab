# Cell service Compose scale example

This example runs the same tenant-scoped KV service in 3, then 5, 10, and 20
containers. Each node owns one Cell and has a Docker limit of **1 vCPU and
1 GiB memory**. The service uses `CellApplication`, `CellNodeBuilder`, the
registered KV primitive, SQLite/LTX publication, and a shared RustFS object
store. A session lease is created and renewed with RustFS conditional writes;
loss of that lease closes readiness. The driver acts as the routing layer: it
addresses each node by Compose DNS name, writes one value per Cell, and checks
all acknowledged values again after every expansion.

```text
driver → node-01 → node-NN HTTP API → tenant KV Cell → local SQLite/LTX
                                                   ↘ RustFS bucket
```

This is a local horizontal-capacity simulation. Every node is on one Docker
host, and the example does not implement Cell migration, remote peer dispatch,
or follower durability. Those contracts use the production server and protected
qualification. The production `crab-http-server` requires at least 2 GiB of
effective memory and 20 GiB of usable local disk per node, so its admission
policy is intentionally not used for this 1 GiB example. Run each simulation
with a fresh project; this small service does not restore a Cell after an
individual container is replaced.

## Run

Use a Docker engine with enough aggregate memory for 20 containers plus
RustFS. The example is built for the Docker engine architecture from a
per-checkout Cargo target directory on the mounted workspace volume. On macOS,
install the matching Linux GNU cross linker first.

```bash
export CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-my-worktree"
export RUSTFS_ACCESS_KEY='<local RustFS access key>'
export RUSTFS_SECRET_KEY='<local RustFS secret key>'
crates/crab-http-server/examples/compose_kv_service/build-image.sh
python3 crates/crab-http-server/examples/compose_kv_service/run.py
```

The driver refuses to reuse an existing Compose project, creates an isolated
RustFS volume and object prefix, and tears down only its own project when it
finishes. Use `--keep` to inspect the running containers afterward. Its JSON
report lives under `$CARGO_TARGET_DIR/cell-scale/` by default and records the
image ID, per-node enforced limits, ready Cell counts, retained values, and
RustFS object counts for every stage. It contains no credentials. Local
simulation results are not signed scale or release qualification receipts.
