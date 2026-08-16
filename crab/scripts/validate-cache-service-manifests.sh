#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRAB_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
MANIFEST_DIR="$CRAB_DIR/deploy/cache-service/kubernetes"
GRAFANA_DASHBOARD="$CRAB_DIR/deploy/cache-service/grafana-dashboard.json"
RULES_MANIFEST="$MANIFEST_DIR/prometheus-rules.yaml"

python3 - "$MANIFEST_DIR" "$GRAFANA_DASHBOARD" <<'PY'
from pathlib import Path
import json
import sys

manifest_dir = Path(sys.argv[1])
dashboard = Path(sys.argv[2])
required = [
    "configmap.yaml",
    "deployment.yaml",
    "service.yaml",
    "service-monitor.yaml",
    "prometheus-rules.yaml",
]

for name in required:
    path = manifest_dir / name
    if not path.is_file():
        raise SystemExit(f"missing cache-service manifest: {path}")
    data = path.read_bytes()
    if not data.endswith(b"\n"):
        raise SystemExit(f"{path}: missing trailing newline")
    for line_number, line in enumerate(data.splitlines(), start=1):
        if line.endswith((b" ", b"\t")):
            raise SystemExit(f"{path}:{line_number}: trailing whitespace")

if not dashboard.is_file():
    raise SystemExit(f"missing cache-service dashboard: {dashboard}")
data = dashboard.read_bytes()
if not data.endswith(b"\n"):
    raise SystemExit(f"{dashboard}: missing trailing newline")
for line_number, line in enumerate(data.splitlines(), start=1):
    if line.endswith((b" ", b"\t")):
        raise SystemExit(f"{dashboard}:{line_number}: trailing whitespace")

parsed = json.loads(data)
if parsed.get("uid") != "crab-cache-service":
    raise SystemExit(f"{dashboard}: unexpected dashboard uid")
if not parsed.get("panels"):
    raise SystemExit(f"{dashboard}: dashboard must define panels")
PY

cargo test \
    --manifest-path "$CRAB_DIR/Cargo.toml" \
    -p crab \
    --lib cache_service::metrics \
    -- --nocapture

if command -v promtool >/dev/null 2>&1; then
    tmp_rules="$(mktemp)"
    trap 'rm -f "$tmp_rules"' EXIT
    python3 - "$RULES_MANIFEST" "$tmp_rules" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1])
target = Path(sys.argv[2])
lines = source.read_text(encoding="utf-8").splitlines()

try:
    start = lines.index("  groups:")
except ValueError as exc:
    raise SystemExit(f"{source}: missing spec.groups") from exc

rule_lines = []
for line in lines[start:]:
    if line.startswith("  "):
        rule_lines.append(line[2:])
    elif line:
        raise SystemExit(f"{source}: unexpected top-level content after spec.groups: {line}")
    else:
        rule_lines.append(line)

target.write_text("\n".join(rule_lines) + "\n", encoding="utf-8")
PY
    promtool check rules "$tmp_rules"
elif [ "${CI:-}" = "true" ]; then
    echo "::error::promtool not found; install Prometheus tooling before validating cache-service rules" >&2
    exit 1
else
    echo "warning: promtool not found; skipping Prometheus rule syntax validation" >&2
fi
