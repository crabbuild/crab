# Cell service Compose scale example

This example runs the same tenant-scoped KV service in 3, then 5, 10, and 20
containers. Each node owns one tenant Cell and has a Docker limit of **1 vCPU and
1 GiB memory**. The service uses `CellApplication`, `CellNodeBuilder`, the
registered KV primitive, SQLite/LTX publication, and a shared RustFS object
store. A session lease is created and renewed with RustFS conditional writes;
loss of that lease closes readiness. Any healthy node accepts a request for any
tenant. It reads the Cell's owner from RustFS and forwards the request to that
owner. The driver writes through one ingress node and reads through another,
then checks all acknowledged values again after every expansion.

```text
client → any node → RustFS Cell owner lookup → owner node → tenant KV Cell
                                                        ↘ local SQLite/LTX → RustFS
```

This is a local tenant-capacity simulation. The route is
`/tenants/{tenant}/kv/{key}`; each node currently provisions the tenant named
by its node name. New nodes add new tenant Cells, while existing tenant values
stay reachable through any node. One hot tenant still has one owner, and
existing Cells do not move automatically. Every node is on one Docker host;
the example uses plain HTTP forwarding inside the Compose network and does not
implement Cell migration, node-loss takeover, or follower durability. Those
contracts use the production server and protected qualification. The production
`crab-http-server` requires at least 2 GiB of
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
