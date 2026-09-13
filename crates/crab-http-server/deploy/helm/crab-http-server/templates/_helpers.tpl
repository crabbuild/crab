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

[auth]
issuer = {{ required "config.auth.issuer is required when config.existingConfigMap is empty" .Values.config.auth.issuer | toJson }}
client_id = {{ required "config.auth.clientId is required when config.existingConfigMap is empty" .Values.config.auth.clientId | toJson }}
public_url = {{ required "config.auth.publicUrl is required when config.existingConfigMap is empty" .Values.config.auth.publicUrl | toJson }}
{{- if .Values.secrets.oidcClientSecretKey }}
client_secret_file = "/run/secrets/crab/oidc-client-secret"
{{- end }}
state_key_file = "/run/secrets/crab/state-key"
{{- end }}
