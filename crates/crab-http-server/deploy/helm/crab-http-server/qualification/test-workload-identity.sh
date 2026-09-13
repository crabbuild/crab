#!/usr/bin/env bash
set -euo pipefail

verifier="$(dirname -- "$0")/verify-workload-identity.sh"
work_dir="$(mktemp -d)"
trap 'rm -rf -- "$work_dir"' EXIT
service_account=crab-http-server

verify() {
  local expected="$1"
  local provider="$2"
  local actual
  actual="$("$verifier" "$provider" "$service_account" \
    "$work_dir/${provider}-service-account.json" "$work_dir/${provider}-pods.json")"
  test "$actual" = "$expected"
}

reject() {
  local provider="$1"
  if "$verifier" "$provider" "$service_account" \
    "$work_dir/${provider}-service-account.json" "$work_dir/${provider}-pods.json" \
    >/dev/null 2>&1; then
    echo "accepted invalid ${provider} workload identity wiring" >&2
    exit 1
  fi
}

jq --null-input '{metadata: {annotations: {}}}' \
  > "$work_dir/eks-service-account.json"
jq --null-input '{items: [range(0; 2) | {
  spec: {
    serviceAccountName: "crab-http-server",
    automountServiceAccountToken: false,
    containers: [{
      name: "crab-http-server",
      env: [
        {name: "AWS_CONTAINER_CREDENTIALS_FULL_URI", value: "http://169.254.170.23/v1/credentials"},
        {name: "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", value: "/var/run/secrets/pods.eks.amazonaws.com/serviceaccount/eks-pod-identity-token"}
      ],
      volumeMounts: [{name: "eks-pod-identity-token", mountPath: "/var/run/secrets/pods.eks.amazonaws.com/serviceaccount/"}]
    }],
    volumes: [{
      name: "eks-pod-identity-token",
      projected: {sources: [{serviceAccountToken: {audience: "pods.eks.amazonaws.com", path: "eks-pod-identity-token"}}]}
    }]
  }
}]}' > "$work_dir/eks-pods.json"
verify eks-pod-identity eks
jq 'del(.items[0].spec.volumes[0].projected.sources[0])' \
  "$work_dir/eks-pods.json" > "$work_dir/eks-pods.invalid"
mv "$work_dir/eks-pods.invalid" "$work_dir/eks-pods.json"
reject eks

jq --null-input '{metadata: {annotations: {
  "iam.gke.io/gcp-service-account": "crab-http-server@example.iam.gserviceaccount.com"
}}}' > "$work_dir/gke-service-account.json"
jq --null-input '{items: [range(0; 2) | {
  spec: {
    serviceAccountName: "crab-http-server",
    automountServiceAccountToken: false,
    containers: [{name: "crab-http-server", env: []}]
  }
}]}' > "$work_dir/gke-pods.json"
verify gke-workload-identity-federation gke
jq '.items[0].spec.containers[0].env = [{name: "GOOGLE_APPLICATION_CREDENTIALS", value: "/secret/key.json"}]' \
  "$work_dir/gke-pods.json" > "$work_dir/gke-pods.invalid"
mv "$work_dir/gke-pods.invalid" "$work_dir/gke-pods.json"
reject gke

jq --null-input '{metadata: {annotations: {
  "azure.workload.identity/client-id": "01234567-89ab-cdef-0123-456789abcdef"
}}}' > "$work_dir/aks-service-account.json"
jq --null-input '{items: [range(0; 2) | {
  metadata: {labels: {"azure.workload.identity/use": "true"}},
  spec: {
    serviceAccountName: "crab-http-server",
    automountServiceAccountToken: false,
    containers: [{
      name: "crab-http-server",
      env: [
        {name: "AZURE_CLIENT_ID", value: "01234567-89ab-cdef-0123-456789abcdef"},
        {name: "AZURE_TENANT_ID", value: "abcdef01-2345-6789-abcd-ef0123456789"},
        {name: "AZURE_FEDERATED_TOKEN_FILE", value: "/var/run/secrets/azure/tokens/azure-identity-token"}
      ],
      volumeMounts: [{name: "azure-identity-token", mountPath: "/var/run/secrets/azure/tokens"}]
    }],
    volumes: [{
      name: "azure-identity-token",
      projected: {sources: [{serviceAccountToken: {audience: "api://AzureADTokenExchange"}}]}
    }]
  }
}]}' > "$work_dir/aks-pods.json"
verify aks-workload-identity aks
jq '.items[0].metadata.labels["azure.workload.identity/use"] = "false"' \
  "$work_dir/aks-pods.json" > "$work_dir/aks-pods.invalid"
mv "$work_dir/aks-pods.invalid" "$work_dir/aks-pods.json"
reject aks
