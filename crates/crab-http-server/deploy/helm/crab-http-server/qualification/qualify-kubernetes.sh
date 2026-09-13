#!/usr/bin/env bash
set -euo pipefail
set +x
umask 077
unset GIT_CURL_VERBOSE GIT_TRACE GIT_TRACE_CURL GIT_TRACE_CURL_NO_DATA \
  GIT_TRACE_PACKET GIT_TRACE2 GIT_TRACE2_EVENT GIT_TRACE2_PERF

usage() {
  echo "usage: qualify-kubernetes.sh PROVIDER NAMESPACE DEPLOYMENT HTTPS_ORIGIN OWNER REPOSITORY EVIDENCE_FILE" >&2
  echo "Set CRAB_HTTP_SERVER_GIT_TOKEN, CRAB_HTTP_SERVER_EXPECTED_IMAGE, CRAB_HTTP_SERVER_EXPECTED_CHART," >&2
  echo "CRAB_HTTP_SERVER_RELEASE_TAG, CRAB_HTTP_SERVER_SOURCE_SHA, and CRAB_HTTP_SERVER_APPROVE_ROLLOUT=true." >&2
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
  (.status.readyReplicas >= 2) and
  (.status.availableReplicas >= 2) and
  (.status.updatedReplicas == .spec.replicas) and
  (.spec.replicas >= 2) and
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
      .maxSkew == 1 and .minDomains >= 2 and
      .whenUnsatisfiable == "DoNotSchedule" and
      .labelSelector.matchLabels == $selector)) and
  (.spec.template.spec.containers[] | select(.name == "crab-http-server") |
    (.image | test("@sha256:[0-9a-f]{64}$")) and
    (.securityContext.allowPrivilegeEscalation == false) and
    (.securityContext.readOnlyRootFilesystem == true) and
    (.securityContext.capabilities.drop == ["ALL"]) and
    all(.env[]?; (.name | forbidden_cloud_env | not)) and
    (.lifecycle.preStop.exec.command == ["/usr/bin/sleep", "15"])) and
  any(.spec.template.spec.volumes[]?;
    .name == "scratch" and (.emptyDir.sizeLimit | length) > 0)
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
  (.spec.minAvailable >= 1) and
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
    ($pods | length) >= 2 and
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
  test "$node_count" -ge 2
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
start_forward "${pods[0]}" "$port_a" "${work_dir}/forward-a.log"
start_forward "${pods[1]}" "$port_b" "${work_dir}/forward-b.log"

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
jq --raw-output '.items[] | select(.metadata.deletionTimestamp == null) | .metadata.uid' \
  "$pods_json" | sort > "${work_dir}/new-uids"
test -z "$(comm -12 "${work_dir}/old-uids" "${work_dir}/new-uids")"

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
  --arg completed_at "$completed_at" \
  --argjson rollout_probes "$probes" \
  --argjson rollout_probe_failures "$probe_failures" \
  --argjson replica_count "$replica_count" \
  --argjson zone_count "$zone_count" \
  --argjson old_pod_uids "$old_uids" \
  --argjson new_pod_uids "$new_uids" \
  '{schema: 3, provider: $provider, namespace: $namespace, deployment: $deployment,
    origin: $origin, image: $image, chart: $chart,
    qualification_source: {release_tag: $release_tag, commit: $source_sha},
    workload_identity: {
      service_account: $service_account,
      mechanism: $workload_identity_mechanism
    },
    repository: $repository, branch: $branch,
    commit: $commit, payload_sha256: $payload_sha256,
    replica_count: $replica_count, zone_count: $zone_count,
    old_pod_uids: $old_pod_uids, new_pod_uids: $new_pod_uids,
    rollout_probes: $rollout_probes,
    rollout_probe_failures: $rollout_probe_failures,
    checks: {
      oidc_login_redirect: true,
      oidc_secure_flow_cookie: true,
      restricted_namespace: true,
      release_chart_version: true,
      workload_identity_only: true,
      management_network_isolation: true,
      cross_replica_git: true,
      cross_replica_lfs: true,
      durable_lfs_lock: true,
      zero_unavailable_rollout: true,
      post_rollout_clone: true
    },
    completed_at: $completed_at}' \
  > "$evidence_temp"
ln "$evidence_temp" "$evidence_file"
rm -f -- "$evidence_temp"
evidence_temp=""

printf 'Qualified %s deployment=%s repository=%s/%s branch=%s commit=%s evidence=%s\n' \
  "$provider" "$deployment" "$owner" "$repository" "$branch" "$final_oid" "$evidence_file"
