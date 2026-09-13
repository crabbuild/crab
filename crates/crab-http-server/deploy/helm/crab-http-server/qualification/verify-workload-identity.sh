#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 4 ]; then
  echo "usage: $0 PROVIDER SERVICE_ACCOUNT SERVICE_ACCOUNT_JSON PODS_JSON" >&2
  exit 2
fi

provider="$1"
service_account="$2"
service_account_json="$3"
pods_json="$4"
test -n "$service_account"
test -s "$service_account_json"
test -s "$pods_json"

case "$provider" in
  eks)
    if ! jq --exit-status --arg service_account "$service_account" '
      [.items[] | select(.metadata.deletionTimestamp == null)] as $pods |
      ($pods | length) >= 2 and
      all($pods[];
        .spec.serviceAccountName == $service_account and
        .spec.automountServiceAccountToken == false and
        ([.spec.containers[] | select(.name == "crab-http-server")] | length) == 1 and
        (.spec.containers[] | select(.name == "crab-http-server") as $container |
          any($container.env[]?;
            .name == "AWS_CONTAINER_CREDENTIALS_FULL_URI" and
            .value == "http://169.254.170.23/v1/credentials") and
          any($container.env[]?;
            .name == "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE" and
            .value == "/var/run/secrets/pods.eks.amazonaws.com/serviceaccount/eks-pod-identity-token") and
          any($container.volumeMounts[]?;
            .name == "eks-pod-identity-token" and
            .mountPath == "/var/run/secrets/pods.eks.amazonaws.com/serviceaccount/")) and
        any(.spec.volumes[]?;
          .name == "eks-pod-identity-token" and
          any((.projected.sources // [])[];
            .serviceAccountToken.audience == "pods.eks.amazonaws.com" and
            .serviceAccountToken.path == "eks-pod-identity-token")))
    ' "$pods_json" >/dev/null; then
      echo "EKS pods do not contain the required Pod Identity injection." >&2
      exit 1
    fi
    printf '%s\n' eks-pod-identity
    ;;
  gke)
    gcp_service_account="$(jq --raw-output \
      '.metadata.annotations["iam.gke.io/gcp-service-account"] // ""' \
      "$service_account_json")"
    if [[ ! "$gcp_service_account" =~ ^[a-z0-9._-]+@[a-z0-9.-]+\.iam\.gserviceaccount\.com$ ]]; then
      echo "GKE ServiceAccount must link to a Google service account." >&2
      exit 1
    fi
    if ! jq --exit-status --arg service_account "$service_account" '
      [.items[] | select(.metadata.deletionTimestamp == null)] as $pods |
      ($pods | length) >= 2 and
      all($pods[];
        .spec.serviceAccountName == $service_account and
        .spec.automountServiceAccountToken == false and
        ([.spec.containers[] | select(.name == "crab-http-server")] | length) == 1 and
        all((.spec.containers[] | select(.name == "crab-http-server") | .env[]?);
          .name != "GOOGLE_APPLICATION_CREDENTIALS" and
          .name != "GOOGLE_SERVICE_ACCOUNT" and
          .name != "GOOGLE_SERVICE_ACCOUNT_KEY"))
    ' "$pods_json" >/dev/null; then
      echo "GKE pods do not contain the required keyless identity configuration." >&2
      exit 1
    fi
    printf '%s\n' gke-workload-identity-federation
    ;;
  aks)
    azure_client_id="$(jq --raw-output \
      '.metadata.annotations["azure.workload.identity/client-id"] // ""' \
      "$service_account_json")"
    if [[ ! "$azure_client_id" =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$ ]]; then
      echo "AKS ServiceAccount must identify an Azure managed identity client." >&2
      exit 1
    fi
    if ! jq --exit-status \
      --arg client_id "$azure_client_id" \
      --arg service_account "$service_account" '
      [.items[] | select(.metadata.deletionTimestamp == null)] as $pods |
      ($pods | length) >= 2 and
      all($pods[];
        . as $pod |
        .metadata.labels["azure.workload.identity/use"] == "true" and
        .spec.serviceAccountName == $service_account and
        .spec.automountServiceAccountToken == false and
        ([.spec.containers[] | select(.name == "crab-http-server")] | length) == 1 and
        (.spec.containers[] | select(.name == "crab-http-server") as $container |
          any($container.env[]?; .name == "AZURE_CLIENT_ID" and .value == $client_id) and
            any($container.env[]?;
              .name == "AZURE_TENANT_ID" and
              (.value | test("^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"))) and
          any($container.env[]?;
            .name == "AZURE_FEDERATED_TOKEN_FILE" and
            (.value | test("^/var/run/secrets/azure/.+/azure-identity-token$"))) and
          any($pod.spec.volumes[]?;
            . as $volume |
            any(($volume.projected.sources // [])[];
              .serviceAccountToken.audience == "api://AzureADTokenExchange") and
            any($container.volumeMounts[]?;
              .name == $volume.name and
              (.mountPath | startswith("/var/run/secrets/azure/"))))))
    ' "$pods_json" >/dev/null; then
      echo "AKS pods do not contain the required Workload ID injection." >&2
      exit 1
    fi
    printf '%s\n' aks-workload-identity
    ;;
  *)
    echo "unsupported workload identity provider: $provider" >&2
    exit 2
    ;;
esac
