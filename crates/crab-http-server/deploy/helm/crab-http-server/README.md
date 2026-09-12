# Kubernetes deployment

This chart is the portable `crab-http-server` deployment for EKS, GKE, and
AKS. It runs two replicas, keeps the management listener out of the Service,
uses object storage for the catalog and identity state, and treats pod scratch
as disposable. The chart does not create buckets, identities, DNS, TLS, or an
ingress controller.

## Inputs

Create a ConfigMap with `server.toml`. Its secret paths must match the projected
files in the chart:

```toml
listen = "0.0.0.0:8788"
management_listen = "0.0.0.0:8789"

[storage]
url = "s3://bucket/repositories" # or gs:// / az://

[auth]
issuer = "https://identity.example/realm"
client_id = "crab-browser"
public_url = "https://git.example.com"
client_secret_file = "/run/secrets/crab/oidc-client-secret"
state_key_file = "/run/secrets/crab/state-key"
```

The existing Secret must contain `oidc-client-secret` and a random `state-key`
of at least 32 bytes. The latter must remain stable across rollouts. Replace
the example's all-zero image digest with a qualified image digest, then use one
of the checked-in provider value files:

```sh
helm upgrade --install crab-http-server \
  crates/crab-http-server/deploy/helm/crab-http-server \
  --namespace crab --create-namespace \
  --values crates/crab-http-server/deploy/helm/crab-http-server/eks-values.example.yaml
```

For EKS, create an [EKS Pod Identity association](https://docs.aws.amazon.com/eks/latest/userguide/pod-identities.html)
for the chart's Kubernetes service account and a least-privilege IAM role. For
GKE, bind the Kubernetes service account through
[Workload Identity Federation for GKE](https://docs.cloud.google.com/kubernetes-engine/docs/how-to/workload-identity)
and set the checked-in annotation. For AKS, follow the
[Microsoft Entra Workload ID setup](https://learn.microsoft.com/en-us/azure/aks/workload-identity-deploy-cluster),
create the federated identity credential, set the client ID annotation, and
retain the pod label that enables the workload identity webhook. In all three
cases grant list/read/write/delete access only below the configured storage
root; static cloud keys do not belong in the Kubernetes Secret.

Configure a provider lifecycle rule that deletes objects below
`.crab/http-server/v1/auth/` after 24 hours. The server checks active expiry and
parent-session validity on every auth lookup; the lifecycle rule collects only
bounded expired state.

The Service and ingress must exclude the management port. Restrict port 8789
with the cluster's network policy while preserving kubelet probe access.
Startup and readiness verify that the catalog can be read from object storage;
liveness only verifies that the process is serving. Put TLS and
request-size/time limits appropriate for large Git and LFS streams on the
ingress or external load balancer.
