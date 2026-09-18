{{- define "crab-http-server.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "crab-http-server.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := include "crab-http-server.name" . }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "crab-http-server.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | quote }}
app.kubernetes.io/name: {{ include "crab-http-server.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "crab-http-server.selectorLabels" -}}
app.kubernetes.io/name: {{ include "crab-http-server.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "crab-http-server.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "crab-http-server.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- required "serviceAccount.name is required when serviceAccount.create is false" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "crab-http-server.image" -}}
{{- $repository := required "image.repository is required" .Values.image.repository -}}
{{- if contains "@" $repository -}}
{{- fail "image.repository must not contain a digest; set image.digest separately" -}}
{{- end -}}
{{- if contains ":" (last (splitList "/" $repository)) -}}
{{- fail "image.repository must not contain a tag; set image.digest instead" -}}
{{- end -}}
{{- printf "%s@%s" $repository (required "image.digest is required" .Values.image.digest) }}
{{- end }}

{{- define "crab-http-server.configMapName" -}}
{{- $managed := or .Values.config.storageUrl .Values.config.auth.issuer .Values.config.auth.clientId .Values.config.auth.publicUrl -}}
{{- if and .Values.config.existingConfigMap $managed -}}
{{- fail "config.existingConfigMap and managed config values are mutually exclusive" -}}
{{- else if .Values.config.existingConfigMap -}}
{{- .Values.config.existingConfigMap -}}
{{- else -}}
{{- $_ := include "crab-http-server.managedConfig" . -}}
{{- include "crab-http-server.fullname" . -}}
{{- end -}}
{{- end }}

{{- define "crab-http-server.managedConfig" -}}
listen = "0.0.0.0:8788"
management_listen = "0.0.0.0:8789"

[storage]
url = {{ required "config.storageUrl is required when config.existingConfigMap is empty" .Values.config.storageUrl | toJson }}

[cells]
data_dir = "/var/lib/crab/cells"
local_disk_limit_bytes = {{ .Values.scratch.sizeBytes }}
peer_advertise = "https://127.0.0.1:8789"
peer_tls_server_name = {{ required "cells.tlsServerName is required" .Values.cells.tlsServerName | toJson }}
peer_certificate = "/run/secrets/crab/peer/tls.crt"
peer_private_key = "/run/secrets/crab/peer/tls.key"
peer_ca = "/run/secrets/crab/peer/ca.crt"

[auth]
issuer = {{ required "config.auth.issuer is required when config.existingConfigMap is empty" .Values.config.auth.issuer | toJson }}
client_id = {{ required "config.auth.clientId is required when config.existingConfigMap is empty" .Values.config.auth.clientId | toJson }}
public_url = {{ required "config.auth.publicUrl is required when config.existingConfigMap is empty" .Values.config.auth.publicUrl | toJson }}
{{- if .Values.secrets.oidcClientSecretKey }}
client_secret_file = "/run/secrets/crab/oidc-client-secret"
{{- end }}
state_key_file = "/run/secrets/crab/state-key"
{{- end }}

{{- define "crab-http-server.validateNetworkPeers" -}}
{{- $name := .name -}}
{{- range $index, $peer := .peers -}}
{{- $cidr := "" -}}
{{- if hasKey $peer "ipBlock" -}}
{{- $cidr = default "" (get (get $peer "ipBlock") "cidr") -}}
{{- if hasSuffix "/0" $cidr -}}
{{- fail (printf "%s[%d] must not admit an unrestricted CIDR" $name $index) -}}
{{- end -}}
{{- end -}}
{{- $namespace := default dict (get $peer "namespaceSelector") -}}
{{- $pod := default dict (get $peer "podSelector") -}}
{{- $namespaceRestricted := or (not (empty (get $namespace "matchLabels"))) (not (empty (get $namespace "matchExpressions"))) -}}
{{- $podRestricted := or (not (empty (get $pod "matchLabels"))) (not (empty (get $pod "matchExpressions"))) -}}
{{- if not (or (not (empty $cidr)) $namespaceRestricted $podRestricted) -}}
{{- fail (printf "%s[%d] must contain a restrictive CIDR, namespace selector, or pod selector" $name $index) -}}
{{- end -}}
{{- end -}}
{{- end }}

{{- define "crab-http-server.validateAvailability" -}}
{{- $minimumReplicas := int .Values.replicaCount -}}
{{- if .Values.autoscaling.enabled -}}
{{- $minimumReplicas = int .Values.autoscaling.minReplicas -}}
{{- end -}}
{{- if ge (int .Values.podDisruptionBudget.minAvailable) $minimumReplicas -}}
{{- fail "podDisruptionBudget.minAvailable must be lower than the minimum replicas so one pod can be voluntarily disrupted" -}}
{{- end -}}
{{- $zoneReady := false -}}
{{- $hostReady := false -}}
{{- range .Values.topologySpreadConstraints -}}
{{- if and (eq .topologyKey "topology.kubernetes.io/zone") (eq (int .maxSkew) 1) (ge (int (default 0 .minDomains)) 2) (eq .whenUnsatisfiable "DoNotSchedule") -}}
{{- $zoneReady = true -}}
{{- end -}}
{{- if and (eq .topologyKey "kubernetes.io/hostname") (eq (int .maxSkew) 1) (ge (int (default 0 .minDomains)) 3) (eq .whenUnsatisfiable "DoNotSchedule") -}}
{{- $hostReady = true -}}
{{- end -}}
{{- end -}}
{{- if not $zoneReady -}}
{{- fail "topologySpreadConstraints must hard-spread replicas across at least two zones with maxSkew 1" -}}
{{- end -}}
{{- if not $hostReady -}}
{{- fail "topologySpreadConstraints must hard-spread replicas across at least three nodes with maxSkew 1" -}}
{{- end -}}
{{- end }}

{{- define "crab-http-server.validateExtraEnv" -}}
{{- /* Provider admission injects workload identity after render. Values may select an AWS region, but must not replace identity or storage authority. */ -}}
{{- $seen := dict -}}
{{- $genericProviderOptions := list
  "ACCESS_KEY_ID" "SECRET_ACCESS_KEY" "DEFAULT_REGION" "REGION"
  "BUCKET" "BUCKET_NAME" "ENDPOINT_URL" "ENDPOINT" "SESSION_TOKEN" "TOKEN"
  "VIRTUAL_HOSTED_STYLE_REQUEST" "S3_EXPRESS" "IMDSV1_FALLBACK" "METADATA_ENDPOINT"
  "UNSIGNED_PAYLOAD" "CHECKSUM_ALGORITHM" "CONTAINER_CREDENTIALS_RELATIVE_URI"
  "CONTAINER_CREDENTIALS_FULL_URI" "CONTAINER_AUTHORIZATION_TOKEN_FILE"
  "WEB_IDENTITY_TOKEN_FILE" "ROLE_ARN" "ROLE_SESSION_NAME" "ENDPOINT_URL_STS"
  "SKIP_SIGNATURE" "COPY_IF_NOT_EXISTS" "CONDITIONAL_PUT" "DISABLE_TAGGING"
  "DISABLE_BULK_DELETE" "REQUEST_PAYER" "ALLOW_HTTP" "SERVER_SIDE_ENCRYPTION"
  "SSE_KMS_KEY_ID" "SSE_BUCKET_KEY_ENABLED" "SSE_CUSTOMER_KEY_BASE64"
  "SERVICE_ACCOUNT" "SERVICE_ACCOUNT_PATH" "SERVICE_ACCOUNT_KEY" "BASE_URL"
  "APPLICATION_CREDENTIALS" "BEARER_TOKEN" "MASTER_KEY" "ACCOUNT_KEY" "ACCESS_KEY"
  "ACCOUNT_NAME" "CLIENT_ID" "CLIENT_SECRET" "TENANT_ID" "AUTHORITY_ID"
  "AUTHORITY_HOST" "SAS_KEY" "SAS_TOKEN" "USE_EMULATOR" "IDENTITY_ENDPOINT"
  "MSI_ENDPOINT" "OBJECT_ID" "MSI_RESOURCE_ID" "FEDERATED_TOKEN_FILE"
  "USE_FABRIC_ENDPOINT" "USE_AZURE_CLI" "CONTAINER_NAME" "FABRIC_TOKEN_SERVICE_URL"
  "FABRIC_WORKLOAD_HOST" "FABRIC_SESSION_TOKEN" "FABRIC_CLUSTER_IDENTIFIER"
  "CREDENTIAL_TYPE" "ENCRYPTION_KEY" -}}
{{- range $index, $entry := .Values.extraEnv -}}
{{- $name := upper (default "" $entry.name) -}}
{{- if eq $name "CRAB_POD_IP" -}}
{{- fail "extraEnv.CRAB_POD_IP is owned by the chart" -}}
{{- end -}}
{{- if hasKey $seen $name -}}
{{- fail (printf "extraEnv[%d].name %q duplicates another environment variable" $index $entry.name) -}}
{{- end -}}
{{- $_ := set $seen $name true -}}
{{- $providerPrefixed := or (hasPrefix "AWS_" $name) (hasPrefix "GOOGLE_" $name) (hasPrefix "AZURE_" $name) -}}
{{- $regionOnly := or (eq $name "AWS_REGION") (eq $name "AWS_DEFAULT_REGION") -}}
{{- if or (and $providerPrefixed (not $regionOnly)) (has $name $genericProviderOptions) -}}
{{- fail (printf "extraEnv[%d].name %q is not allowed; use workload identity and the provider-native storage endpoint" $index $entry.name) -}}
{{- end -}}
{{- end -}}
{{- end }}
