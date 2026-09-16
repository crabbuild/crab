#!/usr/bin/env bash
set -euo pipefail
set +x
umask 077
unset GIT_CURL_VERBOSE GIT_TRACE GIT_TRACE_CURL GIT_TRACE_CURL_NO_DATA \
  GIT_TRACE_PACKET GIT_TRACE2 GIT_TRACE2_EVENT GIT_TRACE2_PERF

usage() {
  echo "usage: qualify-kubernetes.sh PROVIDER NAMESPACE DEPLOYMENT HTTPS_ORIGIN OWNER REPOSITORY EVIDENCE_FILE" >&2
  echo "Set CRAB_HTTP_SERVER_GIT_TOKEN, CRAB_HTTP_SERVER_EXPECTED_IMAGE, CRAB_HTTP_SERVER_EXPECTED_CHART," >&2
  echo "CRAB_HTTP_SERVER_RELEASE_TAG, CRAB_HTTP_SERVER_SOURCE_SHA, CRAB_HTTP_SERVER_APPROVE_ROLLOUT=true," >&2
  echo "and CRAB_HTTP_SERVER_APPROVE_OWNER_LOSS=true." >&2
  exit 2
}

test "$#" -eq 7 || usage
provider="$1"
namespace="$2"
deployment="$3"
origin="${4%/}"
owner="$5"
repository="$6"
evidence_file="$7"
git_token="${CRAB_HTTP_SERVER_GIT_TOKEN:?set CRAB_HTTP_SERVER_GIT_TOKEN to a write-scoped token for the qualification repository}"
expected_image="${CRAB_HTTP_SERVER_EXPECTED_IMAGE:?set CRAB_HTTP_SERVER_EXPECTED_IMAGE to the exact deployed repository@sha256 image}"
expected_chart="${CRAB_HTTP_SERVER_EXPECTED_CHART:?set CRAB_HTTP_SERVER_EXPECTED_CHART to the exact oci:// chart@sha256 reference}"
test "${CRAB_HTTP_SERVER_APPROVE_ROLLOUT:-}" = true || {
  echo "Set CRAB_HTTP_SERVER_APPROVE_ROLLOUT=true to approve a rolling restart." >&2
  exit 2
}
test "${CRAB_HTTP_SERVER_APPROVE_OWNER_LOSS:-}" = true || {
  echo "Set CRAB_HTTP_SERVER_APPROVE_OWNER_LOSS=true to approve forced deletion of the current Cell owner Pod." >&2
  exit 2
}
if [[ ! "$expected_image" =~ ^[^[:space:]@]+@sha256:[0-9a-f]{64}$ ]]; then
  echo "CRAB_HTTP_SERVER_EXPECTED_IMAGE must be an immutable image reference." >&2
  exit 2
fi
if [[ ! "$expected_chart" =~ ^oci://[^[:space:]@]+@sha256:[0-9a-f]{64}$ ]]; then
  echo "CRAB_HTTP_SERVER_EXPECTED_CHART must be an immutable OCI chart reference." >&2
  exit 2
fi
release_tag="${CRAB_HTTP_SERVER_RELEASE_TAG:?set CRAB_HTTP_SERVER_RELEASE_TAG to the qualified crab-http-server-vX.Y.Z tag}"
source_sha="${CRAB_HTTP_SERVER_SOURCE_SHA:?set CRAB_HTTP_SERVER_SOURCE_SHA to the release tag commit}"
if [[ ! "$release_tag" =~ ^crab-http-server-v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "CRAB_HTTP_SERVER_RELEASE_TAG must be a stable crab-http-server-vX.Y.Z tag." >&2
  exit 2
fi
if [[ ! "$source_sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "CRAB_HTTP_SERVER_SOURCE_SHA must be a lowercase 40-character Git commit." >&2
  exit 2
fi

case "$provider" in
  eks | gke | aks) ;;
  *) usage ;;
esac
[[ "$origin" =~ ^https://([^/:]+)$ ]] || {
  echo "HTTPS_ORIGIN must be an HTTPS origin without a port, path, query, or fragment." >&2
  exit 2
}
public_host="${BASH_REMATCH[1]}"
name_pattern='^[A-Za-z0-9][A-Za-z0-9._-]*$'
[[ "$owner" =~ $name_pattern && "$repository" =~ $name_pattern ]] || {
  echo "OWNER and REPOSITORY must be safe URL path segments." >&2
  exit 2
}
evidence_parent="$(dirname -- "$evidence_file")"
test -d "$evidence_parent" || {
  echo "The evidence directory does not exist: ${evidence_parent}" >&2
  exit 2
}
evidence_file="$(cd "$evidence_parent" && pwd)/$(basename -- "$evidence_file")"
test ! -e "$evidence_file" || {
  echo "Refusing to overwrite evidence: ${evidence_file}" >&2
  exit 2
}
for dependency in base64 cmp curl dd git jq kubectl ln mktemp; do
  command -v "$dependency" >/dev/null || {
    echo "Missing required command: ${dependency}" >&2
    exit 2
  }
done
git lfs version >/dev/null

work_parent="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
work_dir="$(mktemp -d "${work_parent%/}/crab-http-server-kubernetes.XXXXXX")"
forward_pids=()
rollout_pid=""
lock_held=false
network_probe_pod=""
client="${work_dir}/client"
payload=""
evidence_temp=""
basic_token=""

git_public() {
  GIT_TERMINAL_PROMPT=0 \
    GIT_CONFIG_COUNT=6 \
    GIT_CONFIG_KEY_0=http.extraHeader \
    GIT_CONFIG_VALUE_0="" \
    GIT_CONFIG_KEY_1=http.extraHeader \
    GIT_CONFIG_VALUE_1="Authorization: Basic ${basic_token}" \
    GIT_CONFIG_KEY_2=http.sslVerify \
    GIT_CONFIG_VALUE_2=true \
    GIT_CONFIG_KEY_3=protocol.version \
    GIT_CONFIG_VALUE_3=2 \
    GIT_CONFIG_KEY_4=http.lowSpeedLimit \
    GIT_CONFIG_VALUE_4=1 \
    GIT_CONFIG_KEY_5=http.lowSpeedTime \
    GIT_CONFIG_VALUE_5=30 \
    git "$@"
}

git_pod() {
  GIT_TERMINAL_PROMPT=0 \
    GIT_CONFIG_COUNT=7 \
    GIT_CONFIG_KEY_0=http.extraHeader \
    GIT_CONFIG_VALUE_0="" \
    GIT_CONFIG_KEY_1=http.extraHeader \
    GIT_CONFIG_VALUE_1="Authorization: Basic ${basic_token}" \
    GIT_CONFIG_KEY_2=http.extraHeader \
    GIT_CONFIG_VALUE_2="Host: ${public_host}" \
    GIT_CONFIG_KEY_3=http.sslVerify \
    GIT_CONFIG_VALUE_3=true \
    GIT_CONFIG_KEY_4=protocol.version \
    GIT_CONFIG_VALUE_4=2 \
    GIT_CONFIG_KEY_5=http.lowSpeedLimit \
    GIT_CONFIG_VALUE_5=1 \
    GIT_CONFIG_KEY_6=http.lowSpeedTime \
    GIT_CONFIG_VALUE_6=30 \
    git "$@"
}

curl_pod() {
  curl --disable --config "$curl_config" --fail --silent --show-error "$@"
}

sha256_file() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

stop_forwards() {
  local pid
  for pid in "${forward_pids[@]}"; do
    kill "$pid" >/dev/null 2>&1 || true
    wait "$pid" 2>/dev/null || true
  done
  forward_pids=()
}

cleanup() {
  local result=$?
  if $lock_held && [ -d "${client}/.git" ]; then
    if [ -n "${remote_a:-}" ]; then
      git -C "$client" remote set-url origin "$remote_a" >/dev/null 2>&1 || true
    fi
    git_pod -C "$client" lfs unlock "$payload" >/dev/null 2>&1 || true
  fi
  if [ -n "$rollout_pid" ]; then
    kill "$rollout_pid" >/dev/null 2>&1 || true
    wait "$rollout_pid" 2>/dev/null || true
  fi
  if [ -n "$network_probe_pod" ]; then
    kubectl --namespace "$namespace" delete pod "$network_probe_pod" \
      --ignore-not-found --wait=false >/dev/null 2>&1 || true
  fi
  stop_forwards
  unset git_token basic_token GIT_CONFIG_VALUE_0
  if [ -e "$evidence_temp" ]; then
    rm -f -- "$evidence_temp"
  fi
  rm -rf -- "$work_dir"
  exit "$result"
}
trap cleanup EXIT

evidence_temp="$(mktemp "${evidence_file}.tmp.XXXXXX")"
basic_token="$(printf 'crab:%s' "$git_token" | base64 | tr -d '\r\n')"
curl_config="${work_dir}/curl-auth.conf"
printf 'header = "Authorization: Basic %s"\nheader = "Host: %s"\n' \
  "$basic_token" "$public_host" > "$curl_config"
chmod 0600 "$curl_config"

deployment_json="${work_dir}/deployment.json"
namespace_json="${work_dir}/namespace.json"
service_json="${work_dir}/service.json"
service_account_json="${work_dir}/service-account.json"
policy_json="${work_dir}/network-policy.json"
ingress_json="${work_dir}/ingress.json"
pdb_json="${work_dir}/pdb.json"
hpa_json="${work_dir}/hpa.json"
pods_json="${work_dir}/pods.json"

kubectl config current-context >/dev/null
kubectl get namespace "$namespace" -o json > "$namespace_json"
for mode in enforce audit warn; do
  policy="$(jq --raw-output --arg mode "$mode" \
    '.metadata.labels["pod-security.kubernetes.io/\($mode)"] // ""' \
    "$namespace_json")"
  test "$policy" = restricted || {
    echo "Namespace ${namespace} must set Pod Security ${mode}=restricted." >&2
    exit 1
  }
  policy_version="$(jq --raw-output --arg mode "$mode" \
    '.metadata.labels["pod-security.kubernetes.io/\($mode)-version"] // ""' \
    "$namespace_json")"
  if [[ ! "$policy_version" =~ ^v1\.([1-9][0-9]*)$ ]]; then
    echo "Namespace ${namespace} must pin Pod Security ${mode} to v1.29 or newer." >&2
    exit 1
  fi
  policy_minor="${BASH_REMATCH[1]}"
  if [ "$policy_minor" -lt 29 ]; then
    echo "Namespace ${namespace} must pin Pod Security ${mode} to v1.29 or newer." >&2
    exit 1
  fi
done
kubectl --namespace "$namespace" rollout status "deployment/${deployment}" --timeout=15m
kubectl --namespace "$namespace" get deployment "$deployment" -o json > "$deployment_json"
# Recheck the admitted Deployment so qualification does not assume that it came
# from the chart or that its values passed render-time validation.
jq --exit-status '
  def forbidden_cloud_env:
    ascii_upcase as $name |
    (((($name | startswith("AWS_")) or
       ($name | startswith("GOOGLE_")) or
       ($name | startswith("AZURE_"))) and
      $name != "AWS_REGION" and $name != "AWS_DEFAULT_REGION") or
     (["ACCESS_KEY_ID", "SECRET_ACCESS_KEY", "DEFAULT_REGION", "REGION",
       "BUCKET", "BUCKET_NAME", "ENDPOINT_URL", "ENDPOINT", "SESSION_TOKEN", "TOKEN",
       "VIRTUAL_HOSTED_STYLE_REQUEST", "S3_EXPRESS", "IMDSV1_FALLBACK", "METADATA_ENDPOINT",
       "UNSIGNED_PAYLOAD", "CHECKSUM_ALGORITHM", "CONTAINER_CREDENTIALS_RELATIVE_URI",
       "CONTAINER_CREDENTIALS_FULL_URI", "CONTAINER_AUTHORIZATION_TOKEN_FILE",
       "WEB_IDENTITY_TOKEN_FILE", "ROLE_ARN", "ROLE_SESSION_NAME", "ENDPOINT_URL_STS",
       "SKIP_SIGNATURE", "COPY_IF_NOT_EXISTS", "CONDITIONAL_PUT", "DISABLE_TAGGING",
       "DISABLE_BULK_DELETE", "REQUEST_PAYER", "ALLOW_HTTP", "SERVER_SIDE_ENCRYPTION",
       "SSE_KMS_KEY_ID", "SSE_BUCKET_KEY_ENABLED", "SSE_CUSTOMER_KEY_BASE64",
       "SERVICE_ACCOUNT", "SERVICE_ACCOUNT_PATH", "SERVICE_ACCOUNT_KEY", "BASE_URL",
       "APPLICATION_CREDENTIALS", "BEARER_TOKEN", "MASTER_KEY", "ACCOUNT_KEY", "ACCESS_KEY",
       "ACCOUNT_NAME", "CLIENT_ID", "CLIENT_SECRET", "TENANT_ID", "AUTHORITY_ID",
       "AUTHORITY_HOST", "SAS_KEY", "SAS_TOKEN", "USE_EMULATOR", "IDENTITY_ENDPOINT",
       "MSI_ENDPOINT", "OBJECT_ID", "MSI_RESOURCE_ID", "FEDERATED_TOKEN_FILE",
       "USE_FABRIC_ENDPOINT", "USE_AZURE_CLI", "CONTAINER_NAME", "FABRIC_TOKEN_SERVICE_URL",
       "FABRIC_WORKLOAD_HOST", "FABRIC_SESSION_TOKEN", "FABRIC_CLUSTER_IDENTIFIER",
       "CREDENTIAL_TYPE", "ENCRYPTION_KEY"] | index($name)) != null);
  (.metadata.generation == .status.observedGeneration) and
  (.status.readyReplicas >= 3) and
  (.status.availableReplicas >= 3) and
  (.status.updatedReplicas == .spec.replicas) and
  (.spec.replicas >= 3) and
  (.spec.strategy.type == "RollingUpdate") and
  (.spec.strategy.rollingUpdate.maxUnavailable == 0) and
  (.spec.strategy.rollingUpdate.maxSurge == 1) and
  (.spec.minReadySeconds >= 5) and
  (.spec.progressDeadlineSeconds >= 600) and
  (.spec.template.spec.automountServiceAccountToken == false) and
  (.spec.template.spec.terminationGracePeriodSeconds >= 630) and
  (.spec.template.spec.securityContext.runAsNonRoot == true) and
  (.spec.template.spec.securityContext.runAsUser == 10001) and
  (.spec.template.spec.securityContext.runAsGroup == 10001) and
  (.spec.template.spec.securityContext.seccompProfile.type == "RuntimeDefault") and
  (.spec.selector.matchLabels as $selector |
    any(.spec.template.spec.topologySpreadConstraints[]?;
      .topologyKey == "topology.kubernetes.io/zone" and
      .maxSkew == 1 and .minDomains >= 2 and
      .whenUnsatisfiable == "DoNotSchedule" and
      .labelSelector.matchLabels == $selector) and
    any(.spec.template.spec.topologySpreadConstraints[]?;
      .topologyKey == "kubernetes.io/hostname" and
      .maxSkew == 1 and .minDomains >= 3 and
      .whenUnsatisfiable == "DoNotSchedule" and
      .labelSelector.matchLabels == $selector)) and
  (.spec.template.spec.containers[] | select(.name == "crab-http-server") |
    (.image | test("@sha256:[0-9a-f]{64}$")) and
    (.args == ["--config", "/etc/crab/http-server/server.toml",
      "--peer-advertise-host", "$(CRAB_POD_IP)"]) and
    (.securityContext.allowPrivilegeEscalation == false) and
    (.securityContext.readOnlyRootFilesystem == true) and
    (.securityContext.capabilities.drop == ["ALL"]) and
    (.startupProbe.exec.command == ["crab-http-server", "--config",
      "/etc/crab/http-server/server.toml", "healthcheck"]) and
    (.readinessProbe.exec.command == ["crab-http-server", "--config",
      "/etc/crab/http-server/server.toml", "healthcheck"]) and
    (.livenessProbe.tcpSocket.port == "management") and
    any(.env[]?; .name == "CRAB_POD_IP" and .valueFrom.fieldRef.fieldPath == "status.podIP") and
    any(.volumeMounts[]?; .name == "scratch" and .mountPath == "/var/lib/crab") and
    all(.env[]?; (.name | forbidden_cloud_env | not)) and
    (.lifecycle.preStop.exec.command == ["/usr/bin/sleep", "15"])) and
  any(.spec.template.spec.volumes[]?;
    .name == "scratch" and (.emptyDir.sizeLimit | length) > 0) and
  any(.spec.template.spec.volumes[]?;
    .name == "secrets" and
    any(.projected.sources[]?.secret.items[]?; .path == "peer/tls.crt") and
    any(.projected.sources[]?.secret.items[]?; .path == "peer/tls.key") and
    any(.projected.sources[]?.secret.items[]?; .path == "peer/ca.crt"))
' "$deployment_json" >/dev/null
release_version="${release_tag#crab-http-server-v}"
expected_chart_label="crab-http-server-${release_version}"
jq --exit-status \
  --arg chart "$expected_chart_label" \
  --arg version "$release_version" '
  .metadata.labels["helm.sh/chart"] == $chart and
  .metadata.labels["app.kubernetes.io/version"] == $version and
  .metadata.labels["app.kubernetes.io/managed-by"] == "Helm"
' "$deployment_json" >/dev/null
image="$(jq --raw-output '.spec.template.spec.containers[] | select(.name == "crab-http-server") | .image' "$deployment_json")"
if [ "$image" != "$expected_image" ]; then
  echo "The deployed image does not match CRAB_HTTP_SERVER_EXPECTED_IMAGE." >&2
  exit 1
fi
selector_json="$(jq --compact-output '.spec.selector.matchLabels' "$deployment_json")"
selector="$(jq --raw-output '.spec.selector.matchLabels | to_entries | map("\(.key)=\(.value)") | join(",")' "$deployment_json")"
service_account="$(jq --raw-output '.spec.template.spec.serviceAccountName // ""' "$deployment_json")"
test -n "$service_account"
kubectl --namespace "$namespace" get serviceaccount "$service_account" \
  -o json > "$service_account_json"

kubectl --namespace "$namespace" get service "$deployment" -o json > "$service_json"
jq --exit-status --argjson selector "$selector_json" '
  .spec.type == "ClusterIP" and
  (.spec.selector == $selector) and
  (.spec.ports | length == 1) and
  (.spec.ports[0].name == "http") and
  (.spec.ports[0].targetPort == "http")
' "$service_json" >/dev/null
kubectl --namespace "$namespace" get networkpolicy "$deployment" -o json > "$policy_json"
jq --exit-status --argjson selector "$selector_json" '
  def restricted_selector($selector):
    $selector != null and
    ((($selector.matchLabels // {}) | length) > 0 or
     (($selector.matchExpressions // []) | length) > 0);
  def restricted_peer:
    ((.ipBlock.cidr? // "") as $cidr |
      ($cidr | endswith("/0") | not) and
      ($cidr != "" or restricted_selector(.namespaceSelector) or
       restricted_selector(.podSelector)));
  (.spec.policyTypes == ["Ingress"]) and
  (.spec.podSelector.matchLabels == $selector) and
  any(.spec.ingress[]?; (.from | length) > 0 and any(.ports[]?; .port == "http")) and
  any(.spec.ingress[]?;
    any(.ports[]?; .port == "management") and
    any(.from[]?; .podSelector.matchLabels == $selector)) and
  all(.spec.ingress[]?.from[]?; restricted_peer) and
  ([.spec.ingress[]?.ports[]?.port] | all(. == "http" or . == "management"))
' "$policy_json" >/dev/null
kubectl --namespace "$namespace" get poddisruptionbudget "$deployment" -o json > "$pdb_json"
minimum_replicas="$(jq --raw-output '.spec.replicas' "$deployment_json")"
kubectl --namespace "$namespace" get horizontalpodautoscaler "$deployment" \
  --ignore-not-found -o json > "$hpa_json"
if [ -s "$hpa_json" ]; then
  minimum_replicas="$(jq --raw-output '.spec.minReplicas' "$hpa_json")"
fi
jq --exit-status --argjson selector "$selector_json" \
  --argjson minimum "$minimum_replicas" '
  ((.spec.minAvailable | type) == "number") and
  (.spec.minAvailable >= 2) and
  (.spec.minAvailable < $minimum) and
  (.spec.unhealthyPodEvictionPolicy == "AlwaysAllow") and
  (.spec.selector.matchLabels == $selector)
' "$pdb_json" >/dev/null
kubectl --namespace "$namespace" get ingress "$deployment" -o json > "$ingress_json"
jq --exit-status --arg host "$public_host" --arg service "$deployment" '
  any(.spec.tls[]?; (.secretName | length) > 0 and any(.hosts[]?; . == $host)) and
  any(.spec.rules[]?; .host == $host and
    any(.http.paths[]?; .backend.service.name == $service and .backend.service.port.name == "http"))
' "$ingress_json" >/dev/null

load_ready_pods() {
  kubectl --namespace "$namespace" get pods --selector "$selector" -o json > "$pods_json"
  jq --exit-status '
    [.items[] | select(.metadata.deletionTimestamp == null)] as $pods |
    ($pods | length) >= 3 and
    all($pods[];
      .status.phase == "Running" and
      any(.status.conditions[]?; .type == "Ready" and .status == "True"))
  ' "$pods_json" >/dev/null
}

check_placement() {
  local nodes_file="${work_dir}/nodes"
  local details_file="${work_dir}/node-details"
  local provider_ids_file="${work_dir}/provider-ids"
  local zones_file="${work_dir}/zones"
  jq --raw-output '.items[] | select(.metadata.deletionTimestamp == null) | .spec.nodeName' \
    "$pods_json" | sort -u > "$nodes_file"
  node_count="$(wc -l < "$nodes_file" | tr -d '[:space:]')"
  test "$node_count" -ge 3
  : > "$details_file"
  while IFS= read -r node; do
    kubectl get node "$node" \
      -o jsonpath='{.metadata.labels.topology\.kubernetes\.io/zone}{"\t"}{.spec.providerID}{"\n"}' \
      >> "$details_file"
  done < "$nodes_file"
  cut -f1 "$details_file" | sed '/^$/d' | sort -u > "$zones_file"
  zone_count="$(wc -l < "$zones_file" | tr -d '[:space:]')"
  test "$zone_count" -ge 2
  cut -f2 "$details_file" | sed '/^$/d' > "$provider_ids_file"
  test "$(wc -l < "$provider_ids_file" | tr -d '[:space:]')" -eq "$node_count"
  case "$provider" in
    eks) provider_pattern='^aws://' ;;
    gke) provider_pattern='^gce://' ;;
    aks) provider_pattern='^azure://' ;;
  esac
  if grep --extended-regexp --invert-match "$provider_pattern" "$provider_ids_file"; then
    echo "One or more nodes do not belong to the declared ${provider} provider." >&2
    exit 1
  fi
}

check_pod_health() {
  local pod
  while IFS= read -r pod; do
    kubectl --namespace "$namespace" exec "$pod" -- \
      crab-http-server --config /etc/crab/http-server/server.toml healthcheck
  done < <(jq --raw-output '.items[] | select(.metadata.deletionTimestamp == null) | .metadata.name' "$pods_json")
}

capture_capacity_envelopes() {
  local phase="$1"
  local output="$2"
  local entries="${work_dir}/capacity-${phase}.jsonl"
  local pod
  local pod_uid
  local report
  : > "$entries"

  while IFS=$'\t' read -r pod pod_uid; do
    report="${work_dir}/capacity-${phase}-${pod}.json"
    kubectl --namespace "$namespace" exec "$pod" -- \
      crab-http-server --config /etc/crab/http-server/server.toml \
        cells capacity --json --live > "$report"
    jq --compact-output \
      --arg phase "$phase" --arg pod "$pod" --arg pod_uid "$pod_uid" \
      '{phase: $phase, pod: $pod, pod_uid: $pod_uid, envelope: .}' \
      "$report" >> "$entries"
  done < <(jq --raw-output '
    .items[] | select(.metadata.deletionTimestamp == null) |
    [.metadata.name, .metadata.uid] | @tsv
  ' "$pods_json")

  jq --slurp . "$entries" > "$output"
  jq --exit-status --arg phase "$phase" --from-file \
    "$(dirname -- "$0")/validate-capacity-envelope.jq" "$output" >/dev/null
}

check_workload_identity() {
  workload_identity_mechanism="$(
    "$(dirname -- "$0")/verify-workload-identity.sh" \
      "$provider" "$service_account" "$service_account_json" "$pods_json"
  )"
}

check_management_isolation() {
  local target_host
  local target_ip
  local phase=""
  target_ip="$(jq --raw-output '
    .items[] | select(.metadata.deletionTimestamp == null) | .status.podIP
  ' "$pods_json" | head -1)"
  test -n "$target_ip" && test "$target_ip" != null
  target_host="$target_ip"
  if [[ "$target_ip" == *:* ]]; then
    target_host="[${target_ip}]"
  fi
  network_probe_pod="crab-http-network-probe-${RANDOM}-$$"

  kubectl --namespace "$namespace" create -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: ${network_probe_pod}
  labels:
    crab.build/qualification-probe: network-isolation
spec:
  automountServiceAccountToken: false
  restartPolicy: Never
  securityContext:
    runAsNonRoot: true
    runAsUser: 65534
    runAsGroup: 65534
    seccompProfile:
      type: RuntimeDefault
  containers:
    - name: probe
      image: caddy:2.10.2-alpine@sha256:4c6e91c6ed0e2fa03efd5b44747b625fec79bc9cd06ac5235a779726618e530d
      imagePullPolicy: IfNotPresent
      command: ["/bin/sh", "-ec"]
      args:
        - |
          if curl --connect-timeout 5 --max-time 8 --silent --output /dev/null \
            http://${target_host}:8789/healthz; then
            echo "management endpoint is reachable from an ordinary peer pod" >&2
            exit 42
          fi
      resources:
        requests:
          cpu: 10m
          memory: 16Mi
          ephemeral-storage: 16Mi
        limits:
          cpu: 100m
          memory: 64Mi
          ephemeral-storage: 64Mi
      securityContext:
        allowPrivilegeEscalation: false
        readOnlyRootFilesystem: true
        capabilities:
          drop: ["ALL"]
EOF

  for _attempt in $(seq 1 60); do
    phase="$(kubectl --namespace "$namespace" get pod "$network_probe_pod" \
      -o jsonpath='{.status.phase}')"
    case "$phase" in
      Succeeded)
        kubectl --namespace "$namespace" delete pod "$network_probe_pod" \
          --wait --timeout=1m >/dev/null
        network_probe_pod=""
        return
        ;;
      Failed)
        kubectl --namespace "$namespace" get pod "$network_probe_pod" -o yaml >&2
        echo "The management network-isolation probe failed; inspect its exit code above." >&2
        return 1
        ;;
    esac
    sleep 2
  done

  kubectl --namespace "$namespace" get pod "$network_probe_pod" -o yaml >&2
  echo "The management network-isolation probe did not complete." >&2
  return 1
}

load_ready_pods
check_placement
check_workload_identity
check_management_isolation
check_pod_health
capacity_before_traffic="${work_dir}/capacity-before-traffic.json"
capacity_after_rollout="${work_dir}/capacity-after-rollout.json"
capacity_after_owner_loss="${work_dir}/capacity-after-owner-loss.json"
capture_capacity_envelopes before-traffic "$capacity_before_traffic"
jq --raw-output '.items[] | select(.metadata.deletionTimestamp == null) | .metadata.uid' \
  "$pods_json" | sort > "${work_dir}/old-uids"

login_headers="${work_dir}/login-headers"
login_status="$(curl --disable --silent --show-error --output /dev/null \
  --dump-header "$login_headers" --write-out '%{http_code}' \
  "${origin}/auth/login?return_to=%2F")"
test "$login_status" = 303
grep --extended-regexp --ignore-case '^x-request-id: [0-9a-f-]{36}[[:space:]]*$' \
  "$login_headers" >/dev/null
grep --extended-regexp --ignore-case '^location: https://' "$login_headers" >/dev/null
grep --extended-regexp --ignore-case '^set-cookie: __Host-crab_login=.*Secure' \
  "$login_headers" >/dev/null

pods=()
while IFS= read -r pod; do
  pods+=("$pod")
done < <(jq --raw-output '.items[] | select(.metadata.deletionTimestamp == null) | .metadata.name' "$pods_json")

start_forward() {
  local pod="$1"
  local port="$2"
  local log="$3"
  kubectl --namespace "$namespace" port-forward "pod/${pod}" "${port}:8788" > "$log" 2>&1 &
  local pid=$!
  forward_pids+=("$pid")
  local ready=false
  for _attempt in $(seq 1 30); do
    if curl --disable --silent --show-error --max-time 2 \
      --header "Host: ${public_host}" --output /dev/null \
      "http://127.0.0.1:${port}/"; then
      ready=true
      break
    fi
    kill -0 "$pid" 2>/dev/null || break
    sleep 1
  done
  if ! $ready; then
    sed -n '1,120p' "$log" >&2
    echo "Could not reach pod ${pod} through port-forward." >&2
    exit 1
  fi
}

port_a="${CRAB_HTTP_SERVER_PORT_A:-28788}"
port_b="${CRAB_HTTP_SERVER_PORT_B:-28789}"
port_c="${CRAB_HTTP_SERVER_PORT_C:-28790}"
start_forward "${pods[0]}" "$port_a" "${work_dir}/forward-a.log"
start_forward "${pods[1]}" "$port_b" "${work_dir}/forward-b.log"
start_forward "${pods[2]}" "$port_c" "${work_dir}/forward-c.log"

remote_public="${origin}/git/${owner}/${repository}.git"
remote_a="http://127.0.0.1:${port_a}/git/${owner}/${repository}.git"
remote_b="http://127.0.0.1:${port_b}/git/${owner}/${repository}.git"
git_pod clone "$remote_a" "$client"
git -C "$client" config user.name "Crab live qualification"
git -C "$client" config user.email "qualification@example.invalid"
git -C "$client" lfs install --local

qualification_id="$(date -u +%Y%m%dT%H%M%SZ)-$$"
branch="crab-qualification/${qualification_id}"
if git -C "$client" rev-parse --verify HEAD >/dev/null 2>&1; then
  git -C "$client" switch --create "$branch"
else
  git -C "$client" switch --orphan "$branch"
fi
payload="qualification/${qualification_id}.bin"
mkdir -p "${client}/qualification"
if [ -s "${client}/.gitattributes" ]; then
  printf '\n' >> "${client}/.gitattributes"
fi
printf '%s filter=lfs diff=lfs merge=lfs -text lockable\n' "$payload" \
  >> "${client}/.gitattributes"
dd if=/dev/urandom of="${client}/${payload}" bs=1048576 count=1 status=none
first_lfs_oid="$(sha256_file "${client}/${payload}")"
first_lfs_size="$(wc -c < "${client}/${payload}" | tr -d '[:space:]')"
lfs_object_path="/git/${owner}/${repository}.git/info/lfs/objects/${first_lfs_oid}?size=${first_lfs_size}"
curl_pod --request PUT --data-binary "@${client}/${payload}" --output /dev/null \
  "http://127.0.0.1:${port_a}${lfs_object_path}"
curl_pod --output "${work_dir}/replica-lfs-object" \
  "http://127.0.0.1:${port_b}${lfs_object_path}"
cmp "${client}/${payload}" "${work_dir}/replica-lfs-object"
git -C "$client" add .gitattributes "$payload"
git -C "$client" commit --message "Qualify Crab Kubernetes replicas"
first_oid="$(git -C "$client" rev-parse HEAD)"
git_pod -C "$client" push "$remote_a" "HEAD:refs/heads/${branch}"

visible_oid=""
for _attempt in $(seq 1 30); do
  if visible_oid="$(git_pod ls-remote "$remote_b" "refs/heads/${branch}" | cut -f1)"; then
    :
  else
    visible_oid=""
  fi
  if [ "$visible_oid" = "$first_oid" ]; then
    break
  fi
  sleep 2
done
test "$visible_oid" = "$first_oid"
git_pod clone --branch "$branch" --single-branch \
  "$remote_b" "${work_dir}/replica-clone"
cmp "${client}/${payload}" "${work_dir}/replica-clone/${payload}"

git_pod -C "$client" lfs lock "$payload"
lock_held=true
curl_pod --get --data-urlencode "path=${payload}" \
  "http://127.0.0.1:${port_b}/git/${owner}/${repository}.git/info/lfs/locks" \
  | jq --exit-status --arg path "$payload" \
    '.locks | length == 1 and .[0].path == $path' >/dev/null
git -C "$client" remote set-url origin "$remote_public"
git -C "$client" config "lfs.${remote_public}/info/lfs.locksverify" true
printf 'second revision %s\n' "$qualification_id" >> "${client}/${payload}"
git -C "$client" add "$payload"
git -C "$client" commit --message "Qualify durable LFS lock owner write"
final_oid="$(git -C "$client" rev-parse HEAD)"
git_public -C "$client" push "$remote_public" "HEAD:refs/heads/${branch}"
git_public -C "$client" lfs unlock "$payload"
lock_held=false

uuid_from_text() {
  local digest
  if command -v sha256sum >/dev/null; then
    digest="$(printf '%s' "$1" | sha256sum | awk '{print $1}')"
  else
    digest="$(printf '%s' "$1" | shasum -a 256 | awk '{print $1}')"
  fi
  printf '%s-%s-%s-%s-%s' \
    "${digest:0:8}" "${digest:8:4}" "${digest:12:4}" \
    "${digest:16:4}" "${digest:20:12}"
}

status_context="crab/live-qualification"
status_request_id="$(uuid_from_text "${qualification_id}:status")"
jq --null-input \
  --arg request_id "$status_request_id" \
  --arg context "$status_context" \
  --arg target_url "${origin}/qualification/${qualification_id}" \
  '{request_id: $request_id, context: $context, state: "success",
    description: "Three-node Cell qualification", target_url: $target_url}' \
  > "${work_dir}/status-input.json"
curl_pod --request POST --header 'content-type: application/json' \
  --data-binary "@${work_dir}/status-input.json" \
  --output "${work_dir}/status-created.json" \
  "http://127.0.0.1:${port_a}/api/repos/${owner}/${repository}/statuses/${final_oid}"
jq --exit-status \
  --arg context "$status_context" \
  '.context == $context and .state == "success"' \
  "${work_dir}/status-created.json" >/dev/null
curl_pod --output "${work_dir}/status-replica.json" \
  "http://127.0.0.1:${port_b}/api/repos/${owner}/${repository}/commits/${final_oid}/status"
jq --exit-status \
  --arg oid "$final_oid" --arg context "$status_context" \
  '.sha == $oid and .state == "success" and
   any(.statuses[]; .context == $context and .state == "success")' \
  "${work_dir}/status-replica.json" >/dev/null

check_request_id="$(uuid_from_text "${qualification_id}:check")"
jq --null-input \
  --arg request_id "$check_request_id" \
  --arg head_sha "$final_oid" \
  --arg details_url "${origin}/qualification/${qualification_id}" \
  '{request_id: $request_id, head_sha: $head_sha,
    name: "crab/live-qualification", status: "completed",
    conclusion: "success", details_url: $details_url,
    output: {title: "Three-node Cell qualification passed",
      summary: "The record must survive replacement of every serving Pod.",
      text: null, steps: [], annotations: []}}' \
  > "${work_dir}/check-input.json"
curl_pod --request POST --header 'content-type: application/json' \
  --data-binary "@${work_dir}/check-input.json" \
  --output "${work_dir}/check-created.json" \
  "http://127.0.0.1:${port_c}/api/repos/${owner}/${repository}/check-runs"
check_run_id="$(jq --raw-output \
  'select(.name == "crab/live-qualification" and .status == "completed" and .conclusion == "success") | .id' \
  "${work_dir}/check-created.json")"
[[ "$check_run_id" =~ ^[1-9][0-9]*$ ]]
check_path="/api/repos/${owner}/${repository}/commits/${final_oid}/check-runs/${check_run_id}"
curl_pod --output "${work_dir}/check-replica.json" \
  "http://127.0.0.1:${port_a}${check_path}"
jq --exit-status \
  --argjson id "$check_run_id" \
  '.id == $id and .status == "completed" and .conclusion == "success" and
   .output.title == "Three-node Cell qualification passed"' \
  "${work_dir}/check-replica.json" >/dev/null

stop_forwards
kubectl --namespace "$namespace" rollout restart "deployment/${deployment}"
rollout_log="${work_dir}/rollout.log"
kubectl --namespace "$namespace" rollout status "deployment/${deployment}" --timeout=15m \
  > "$rollout_log" 2>&1 &
rollout_pid=$!
probes=0
probe_failures=0
while kill -0 "$rollout_pid" 2>/dev/null; do
  if git_public ls-remote "$remote_public" "refs/heads/${branch}" \
    | grep --fixed-strings "$final_oid" >/dev/null; then
    probes=$((probes + 1))
  else
    probe_failures=$((probe_failures + 1))
  fi
  sleep 2
done
if ! wait "$rollout_pid"; then
  sed -n '1,160p' "$rollout_log" >&2
  exit 1
fi
rollout_pid=""
test "$probes" -ge 1
test "$probe_failures" -eq 0

load_ready_pods
check_placement
check_workload_identity
check_pod_health
capture_capacity_envelopes after-rollout "$capacity_after_rollout"
jq --raw-output '.items[] | select(.metadata.deletionTimestamp == null) | .metadata.uid' \
  "$pods_json" | sort > "${work_dir}/new-uids"
test -z "$(comm -12 "${work_dir}/old-uids" "${work_dir}/new-uids")"

pods=()
while IFS= read -r pod; do
  pods+=("$pod")
done < <(jq --raw-output '.items[] | select(.metadata.deletionTimestamp == null) | .metadata.name' "$pods_json")
start_forward "${pods[0]}" "$port_a" "${work_dir}/forward-restored-a.log"
start_forward "${pods[1]}" "$port_b" "${work_dir}/forward-restored-b.log"
start_forward "${pods[2]}" "$port_c" "${work_dir}/forward-restored-c.log"

curl_pod --output "${work_dir}/status-restored.json" \
  "http://127.0.0.1:${port_c}/api/repos/${owner}/${repository}/commits/${final_oid}/status"
jq --exit-status \
  --arg oid "$final_oid" --arg context "$status_context" \
  '.sha == $oid and .state == "success" and
   any(.statuses[]; .context == $context and .state == "success")' \
  "${work_dir}/status-restored.json" >/dev/null
curl_pod --output "${work_dir}/check-restored.json" \
  "http://127.0.0.1:${port_b}${check_path}"
jq --exit-status \
  --argjson id "$check_run_id" \
  '.id == $id and .status == "completed" and .conclusion == "success" and
   .output.title == "Three-node Cell qualification passed"' \
  "${work_dir}/check-restored.json" >/dev/null

control_before="${work_dir}/control-before-owner-loss.json"
kubectl --namespace "$namespace" exec "${pods[0]}" -- \
  crab-http-server --config /etc/crab/http-server/server.toml \
    cells status --owner "$owner" --name "$repository" > "$control_before"
jq --exit-status '
  .version == 1 and .state == "serving" and
  (.owner.session | test("^[0-9a-f]{32}$")) and
  (.owner.endpoint | startswith("https://")) and
  (.root.digest | test("^[0-9a-f]{64}$")) and
  .root.commit_sequence > 0
' "$control_before" >/dev/null
owner_endpoint="$(jq --raw-output '.owner.endpoint' "$control_before")"
owner_session_before="$(jq --raw-output '.owner.session' "$control_before")"
owner_epoch_before="$(jq --raw-output '.epoch' "$control_before")"
owner_commit_sequence_before="$(jq --raw-output '.root.commit_sequence' "$control_before")"
owner_pod=""
owner_pod_uid=""
while IFS=$'\t' read -r pod ip uid; do
  peer_host="$ip"
  if [[ "$ip" == *:* ]]; then
    peer_host="[${ip}]"
  fi
  if [ "$owner_endpoint" = "https://${peer_host}:8789/" ]; then
    owner_pod="$pod"
    owner_pod_uid="$uid"
    break
  fi
done < <(jq --raw-output '
  .items[] | select(.metadata.deletionTimestamp == null) |
  [.metadata.name, .status.podIP, .metadata.uid] | @tsv
' "$pods_json")
test -n "$owner_pod"
test -n "$owner_pod_uid"

stop_forwards
kubectl --namespace "$namespace" delete pod "$owner_pod" \
  --grace-period=0 --force --wait=false >/dev/null
kubectl --namespace "$namespace" wait --for=delete "pod/${owner_pod}" --timeout=2m
kubectl --namespace "$namespace" rollout status "deployment/${deployment}" --timeout=15m
owner_replacement_ready=false
for _attempt in $(seq 1 60); do
  if load_ready_pods; then
    owner_replacement_ready=true
    break
  fi
  sleep 2
done
$owner_replacement_ready
jq --exit-status --arg uid "$owner_pod_uid" '
  [.items[] | select(.metadata.deletionTimestamp == null) | .metadata.uid] |
  index($uid) == null
' "$pods_json" >/dev/null
owner_advertisement_status="${work_dir}/owner-advertisement-expired.json"
owner_advertisement_expired=false
inspection_pod="$(jq --raw-output '
  first(.items[] | select(.metadata.deletionTimestamp == null) | .metadata.name)
' "$pods_json")"
for _attempt in $(seq 1 60); do
  if kubectl --namespace "$namespace" exec "$inspection_pod" -- \
      crab-http-server --config /etc/crab/http-server/server.toml \
        cells node --session "$owner_session_before" --json \
        > "$owner_advertisement_status" &&
    jq --exit-status \
      --arg session "$owner_session_before" '
      .version == 1 and .session == $session and .live == false and
      .observed_at_ms >= 0
    ' "$owner_advertisement_status" >/dev/null; then
    owner_advertisement_expired=true
    break
  fi
  sleep 2
done
$owner_advertisement_expired
check_placement
check_workload_identity
check_pod_health
capture_capacity_envelopes after-owner-loss "$capacity_after_owner_loss"

pods=()
while IFS= read -r pod; do
  pods+=("$pod")
done < <(jq --raw-output '.items[] | select(.metadata.deletionTimestamp == null) | .metadata.name' "$pods_json")
start_forward "${pods[0]}" "$port_a" "${work_dir}/forward-owner-loss-a.log"
start_forward "${pods[1]}" "$port_b" "${work_dir}/forward-owner-loss-b.log"
start_forward "${pods[2]}" "$port_c" "${work_dir}/forward-owner-loss-c.log"

owner_loss_restored=false
for _attempt in $(seq 1 60); do
  if curl_pod --output "${work_dir}/status-owner-loss.json" \
      "http://127.0.0.1:${port_a}/api/repos/${owner}/${repository}/commits/${final_oid}/status" &&
    jq --exit-status \
      --arg oid "$final_oid" --arg context "$status_context" \
      '.sha == $oid and .state == "success" and
       any(.statuses[]; .context == $context and .state == "success")' \
      "${work_dir}/status-owner-loss.json" >/dev/null; then
    owner_loss_restored=true
    break
  fi
  sleep 2
done
$owner_loss_restored

control_after="${work_dir}/control-after-owner-loss.json"
kubectl --namespace "$namespace" exec "${pods[0]}" -- \
  crab-http-server --config /etc/crab/http-server/server.toml \
    cells status --owner "$owner" --name "$repository" > "$control_after"
jq --exit-status \
  --arg session "$owner_session_before" \
  --argjson epoch "$owner_epoch_before" \
  --argjson sequence "$owner_commit_sequence_before" '
  .version == 1 and .state == "serving" and
  .owner.session != $session and .epoch > $epoch and
  .root.commit_sequence >= $sequence
' "$control_after" >/dev/null

continuation_context="crab/live-qualification-after-owner-loss"
continuation_request_id="$(uuid_from_text "${qualification_id}:owner-loss")"
jq --null-input \
  --arg request_id "$continuation_request_id" \
  --arg context "$continuation_context" \
  --arg target_url "${origin}/qualification/${qualification_id}" \
  '{request_id: $request_id, context: $context, state: "success",
    description: "Cell publication continued after owner loss", target_url: $target_url}' \
  > "${work_dir}/owner-loss-status-input.json"
curl_pod --request POST --header 'content-type: application/json' \
  --data-binary "@${work_dir}/owner-loss-status-input.json" \
  --output "${work_dir}/owner-loss-status-created.json" \
  "http://127.0.0.1:${port_b}/api/repos/${owner}/${repository}/statuses/${final_oid}"
jq --exit-status --arg context "$continuation_context" \
  '.context == $context and .state == "success"' \
  "${work_dir}/owner-loss-status-created.json" >/dev/null
curl_pod --output "${work_dir}/owner-loss-status-replica.json" \
  "http://127.0.0.1:${port_c}/api/repos/${owner}/${repository}/commits/${final_oid}/status"
jq --exit-status --arg context "$continuation_context" \
  'any(.statuses[]; .context == $context and .state == "success")' \
  "${work_dir}/owner-loss-status-replica.json" >/dev/null
control_final="${work_dir}/control-after-owner-loss-publication.json"
kubectl --namespace "$namespace" exec "${pods[0]}" -- \
  crab-http-server --config /etc/crab/http-server/server.toml \
    cells status --owner "$owner" --name "$repository" > "$control_final"
jq --exit-status \
  --arg session "$(jq --raw-output '.owner.session' "$control_after")" \
  --argjson epoch "$(jq --raw-output '.epoch' "$control_after")" \
  --argjson sequence "$(jq --raw-output '.root.commit_sequence' "$control_after")" '
  .version == 1 and .state == "serving" and
  .owner.session == $session and .epoch == $epoch and
  .root.commit_sequence > $sequence
' "$control_final" >/dev/null

git_public clone --branch "$branch" --single-branch \
  "$remote_public" "${work_dir}/post-rollout-clone"
test "$(git -C "${work_dir}/post-rollout-clone" rev-parse HEAD)" = "$final_oid"
cmp "${client}/${payload}" "${work_dir}/post-rollout-clone/${payload}"

payload_sha256="$(sha256_file "${client}/${payload}")"
completed_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
replica_count="$(jq '[.items[] | select(.metadata.deletionTimestamp == null)] | length' "$pods_json")"
old_uids="$(jq --raw-input --slurp 'split("\n") | map(select(length > 0))' "${work_dir}/old-uids")"
new_uids="$(jq --raw-input --slurp 'split("\n") | map(select(length > 0))' "${work_dir}/new-uids")"
jq --null-input \
  --arg provider "$provider" \
  --arg namespace "$namespace" \
  --arg deployment "$deployment" \
  --arg origin "$origin" \
  --arg image "$image" \
  --arg chart "$expected_chart" \
  --arg release_tag "$release_tag" \
  --arg source_sha "$source_sha" \
  --arg service_account "$service_account" \
  --arg workload_identity_mechanism "$workload_identity_mechanism" \
  --arg repository "${owner}/${repository}" \
  --arg branch "$branch" \
  --arg commit "$final_oid" \
  --arg payload_sha256 "$payload_sha256" \
  --arg status_context "$status_context" \
  --arg continuation_context "$continuation_context" \
  --arg owner_pod_uid "$owner_pod_uid" \
  --arg owner_session_before "$owner_session_before" \
  --arg owner_session_after "$(jq --raw-output '.owner.session' "$control_after")" \
  --argjson owner_advertisement_observed_at_ms \
    "$(jq --raw-output '.observed_at_ms' "$owner_advertisement_status")" \
  --arg root_digest_before "$(jq --raw-output '.root.digest' "$control_before")" \
  --arg root_digest_after "$(jq --raw-output '.root.digest' "$control_after")" \
  --argjson owner_epoch_before "$owner_epoch_before" \
  --argjson owner_epoch_after "$(jq --raw-output '.epoch' "$control_after")" \
  --argjson root_sequence_before "$owner_commit_sequence_before" \
  --argjson root_sequence_after "$(jq --raw-output '.root.commit_sequence' "$control_after")" \
  --argjson root_sequence_final "$(jq --raw-output '.root.commit_sequence' "$control_final")" \
  --argjson check_run_id "$check_run_id" \
  --arg completed_at "$completed_at" \
  --argjson rollout_probes "$probes" \
  --argjson rollout_probe_failures "$probe_failures" \
  --argjson replica_count "$replica_count" \
  --argjson zone_count "$zone_count" \
  --argjson old_pod_uids "$old_uids" \
  --argjson new_pod_uids "$new_uids" \
  --slurpfile capacity_before_traffic "$capacity_before_traffic" \
  --slurpfile capacity_after_rollout "$capacity_after_rollout" \
  --slurpfile capacity_after_owner_loss "$capacity_after_owner_loss" \
  '{schema: 7, provider: $provider, namespace: $namespace, deployment: $deployment,
    origin: $origin, image: $image, chart: $chart,
    qualification_source: {release_tag: $release_tag, commit: $source_sha},
    workload_identity: {
      service_account: $service_account,
      mechanism: $workload_identity_mechanism
    },
    repository: $repository, branch: $branch,
    commit: $commit, payload_sha256: $payload_sha256,
    status_context: $status_context, continuation_context: $continuation_context,
    check_run_id: $check_run_id,
    owner_loss: {
      deleted_pod_uid: $owner_pod_uid,
      session_before: $owner_session_before,
      session_after: $owner_session_after,
      previous_advertisement_expired: true,
      advertisement_observed_at_ms: $owner_advertisement_observed_at_ms,
      epoch_before: $owner_epoch_before,
      epoch_after: $owner_epoch_after,
      root_digest_before: $root_digest_before,
      root_digest_after: $root_digest_after,
      root_sequence_before: $root_sequence_before,
      root_sequence_after: $root_sequence_after,
      root_sequence_final: $root_sequence_final
    },
    replica_count: $replica_count, zone_count: $zone_count,
    old_pod_uids: $old_pod_uids, new_pod_uids: $new_pod_uids,
    capacity: {
      before_traffic: $capacity_before_traffic[0],
      after_rollout: $capacity_after_rollout[0],
      after_owner_loss: $capacity_after_owner_loss[0]
    },
    rollout_probes: $rollout_probes,
    rollout_probe_failures: $rollout_probe_failures,
    checks: {
      oidc_login_redirect: true,
      oidc_secure_flow_cookie: true,
      restricted_namespace: true,
      release_chart_version: true,
      workload_identity_only: true,
      management_network_isolation: true,
      capacity_envelopes: true,
      cross_replica_git: true,
      cross_replica_lfs: true,
      durable_lfs_lock: true,
      cross_replica_cell: true,
      zero_unavailable_rollout: true,
      post_rollout_clone: true,
      post_rollout_cell_restore: true,
      abrupt_owner_loss: true,
      owner_advertisement_expired: true,
      owner_loss_exact_root_restore: true,
      owner_loss_publication_continues: true
    },
    completed_at: $completed_at}' \
  > "$evidence_temp"
ln "$evidence_temp" "$evidence_file"
rm -f -- "$evidence_temp"
evidence_temp=""

printf 'Qualified %s deployment=%s repository=%s/%s branch=%s commit=%s evidence=%s\n' \
  "$provider" "$deployment" "$owner" "$repository" "$branch" "$final_oid" "$evidence_file"
