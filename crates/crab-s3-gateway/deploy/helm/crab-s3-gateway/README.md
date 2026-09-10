# Crab S3 gateway Helm chart

This chart renders the initial EKS workload contract. It is not evidence of a
live EKS deployment. Supply an immutable image digest, an existing ConfigMap
containing `s3-gateway.toml`, and an existing Secret containing every credential
file referenced by that configuration.

The chart creates two replicas by default, an optional ServiceAccount, a
rolling-update Deployment, an S3-only Service, and a PodDisruptionBudget. The
management port is reachable only inside each pod for startup, readiness, and
liveness probes; it is never added to the Service. The pod runs as UID/GID
10001 with a read-only root filesystem and no Linux capabilities. Kubernetes
projects the Secret as root-owned, process-group-readable files; the gateway
accepts that `0440` shape only when the file group matches its effective group.
Each pod also receives separate scratch and cache `emptyDir` volumes. Configure
`[cache].directory = "/var/lib/crab/cache-volume/cache"` and keep
`[cache].max_bytes` below `cache.sizeLimit`; the process creates the private
child directory and fails startup if it cannot publish and remove cache files.
The cache accelerates immutable reads but remains disposable across pod loss.

Prometheus scrapes `GET /metrics` on the pod's named `management` port. The
chart can install a release-scoped PodMonitor and the gateway's canonical alert
rules when the Prometheus Operator CRDs are present:

```yaml
monitoring:
  labels:
    release: kube-prometheus-stack
  podMonitor:
    enabled: true
  prometheusRule:
    enabled: true
```

Both resources are disabled by default so the chart remains installable without
those CRDs. Set `monitoring.labels` to labels selected by the cluster's
Prometheus instance. The PodMonitor reads the private pod port directly; port
8081 is never added to the public S3 Service. Restrict that traffic with the
cluster's monitoring NetworkPolicy. A direct operator check can use a temporary
pod port-forward:

```sh
pod="$(kubectl get pod --namespace crab-s3-gateway \
  --selector app.kubernetes.io/name=crab-s3-gateway,app.kubernetes.io/instance=crab-s3-gateway \
  --output jsonpath='{.items[0].metadata.name}')"
kubectl port-forward --namespace crab-s3-gateway "pod/${pod}" 18081:8081
curl --fail --silent --show-error http://127.0.0.1:18081/metrics
```

The canonical rules under `monitoring/` cover stalled multipart maintenance,
server and response-stream errors, backend authorization/degradation, admission
pressure, scratch health/capacity/I/O, and cache health/capacity/persistence.
CI checks their syntax and executes both healthy and faulting scenarios with a
pinned `promtool`. Route critical storage-integrity and authorization alerts to
the storage on-call; route warning capacity and degradation alerts to the
gateway owner. Prometheus selection and Alertmanager receiver delivery still
require live cluster proof.

Admission pressure should drive scaling before sustained S3 `SlowDown`
responses. Correlate backend in-flight calls with admission queues: one
identifies provider work while the other identifies local request pressure.
Kubelet ephemeral-storage telemetry remains authoritative for
`scratch.sizeLimit` when the node runtime does not represent the `emptyDir`
policy cap as a filesystem quota. Each pod's `emptyDir` is intentionally
private: the atomic pre-write gate is process-local and retains 10% of visible
filesystem capacity, bounded to 64 MiB–1 GiB.

Before installation:

- Initialize every configured Crab repository using the same image, backend
  identity, configuration, and credential mounts. Normal serving deliberately
  does not initialize storage.
- Create the ConfigMap and Secret outside this chart. Secret keys must have the
  same filenames used by each `secret_key_file` configuration path.
- Set the required cache directory and byte ceiling in the ConfigMap. Account
  for both `scratch.sizeLimit` and `cache.sizeLimit` in the container's
  ephemeral-storage limit.
- Push the qualified image to the selected registry and replace the example
  repository and digest with its immutable values.
- Associate the chart ServiceAccount with a least-privilege EKS Pod Identity
  role. The gateway uses the provider SDK's container credential chain; do not
  put backend AWS keys in the gateway credential Secret.

Validate and render before installation:

```sh
helm lint crates/crab-s3-gateway/deploy/helm/crab-s3-gateway \
  -f crates/crab-s3-gateway/deploy/helm/crab-s3-gateway/eks-values.example.yaml
helm template crab-s3-gateway \
  crates/crab-s3-gateway/deploy/helm/crab-s3-gateway \
  --namespace crab-s3-gateway \
  -f crates/crab-s3-gateway/deploy/helm/crab-s3-gateway/eks-values.example.yaml
```

Replace every placeholder in `eks-values.example.yaml`. Associate the rendered
ServiceAccount with a least-privilege EKS Pod Identity role outside Helm. The
gateway configuration must bind `listen` to `0.0.0.0:8080` and
`management_listen` to `0.0.0.0:8081`; credential paths must resolve beneath
`/run/secrets/crab-s3/`.

Install or upgrade one named release, then wait for the rollout:

```sh
helm upgrade --install crab-s3-gateway \
  crates/crab-s3-gateway/deploy/helm/crab-s3-gateway \
  --namespace crab-s3-gateway --create-namespace \
  --values /path/to/qualified-values.yaml \
  --atomic --timeout 15m
kubectl rollout status deployment/crab-s3-gateway \
  --namespace crab-s3-gateway --timeout 15m
kubectl logs --namespace crab-s3-gateway \
  --selector app.kubernetes.io/name=crab-s3-gateway,app.kubernetes.io/instance=crab-s3-gateway \
  --all-containers --prefix --tail 200
kubectl get events --namespace crab-s3-gateway \
  --sort-by=.metadata.creationTimestamp
```

The gateway reads configuration and client credential files at process start.
After updating the existing ConfigMap or Secret, explicitly restart and verify
the rollout. Roll back only to a revision whose gateway metadata and multipart
schemas remain compatible:

```sh
kubectl rollout restart deployment/crab-s3-gateway \
  --namespace crab-s3-gateway
kubectl rollout status deployment/crab-s3-gateway \
  --namespace crab-s3-gateway --timeout 15m
helm history crab-s3-gateway --namespace crab-s3-gateway
helm rollback crab-s3-gateway REVISION --namespace crab-s3-gateway \
  --wait --timeout 15m
```

`helm uninstall crab-s3-gateway --namespace crab-s3-gateway` removes only the
chart-owned workload objects. It does not remove the existing ConfigMap, Secret,
Pod Identity association, physical bucket, or Crab repository data.

The example Service is an internal AWS Network Load Balancer without TLS.
Production external traffic requires a deployment-specific certificate and a
tested TLS listener that preserves the signed Host, path, query, and headers.
Do not treat a successful render as proof of EKS, Pod Identity, load-balancer,
DNS, TLS, or multi-zone runtime behavior.
