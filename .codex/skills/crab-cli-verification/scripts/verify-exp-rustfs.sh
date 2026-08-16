#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: verify-exp-rustfs.sh [options]

Creates a RustFS-backed Crab fixture and verifies the crab exp command group
against real local object-store side effects.

Options:
  --run-id ID          Run id. Default: exp-verify-<UTC timestamp>.
  --size-mib N         Size of each baseline .bin file. Default: 4.
  --crab-bin PATH      Crab binary to run. Default: crab.
  -h, --help           Show this help.
USAGE
}

run_id="exp-verify-$(date -u +%Y%m%d-%H%M%S)"
size_mib=4
crab_bin="crab"

while (($#)); do
  case "$1" in
    --run-id)
      run_id="${2:?missing value for --run-id}"
      shift
      ;;
    --size-mib)
      size_mib="${2:?missing value for --size-mib}"
      shift
      ;;
    --crab-bin)
      crab_bin="${2:?missing value for --crab-bin}"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 127
  fi
}

slugify() {
  printf '%s' "$1" | tr -cs 'A-Za-z0-9._-' '-' | sed -E 's/^-+|-+$//g'
}

step() {
  printf '\n==> %s\n' "$*"
}

run_log() {
  local name="$1"
  shift
  local slug
  slug="$(slugify "$name")"
  [[ -n "$slug" ]] || slug="command"
  step "$name"
  "$@" >"$exp_logs/$slug.stdout.log" 2>"$exp_logs/$slug.stderr.log" || {
    local status=$?
    echo "command failed ($status): $name" >&2
    echo "--- stdout: $exp_logs/$slug.stdout.log" >&2
    sed -n '1,160p' "$exp_logs/$slug.stdout.log" >&2 || true
    echo "--- stderr: $exp_logs/$slug.stderr.log" >&2
    sed -n '1,160p' "$exp_logs/$slug.stderr.log" >&2 || true
    exit "$status"
  }
}

run_log_in() {
  local name="$1"
  local cwd="$2"
  shift 2
  run_log "$name" bash -lc 'cd "$1" && shift && "$@"' bash "$cwd" "$@"
}

run_json() {
  local name="$1"
  local schema="$2"
  shift 2
  run_log "$name" "$@"
  local slug
  slug="$(slugify "$name")"
  python3 - "$exp_logs/$slug.stdout.log" "$schema" <<'PY'
import json
import sys

path, expected_schema = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as fh:
    envelope = json.load(fh)
if envelope.get("schema") != expected_schema:
    raise SystemExit(f"schema mismatch: expected {expected_schema}, got {envelope.get('schema')}")
if envelope.get("error") is not None:
    raise SystemExit(f"unexpected error envelope: {envelope['error']}")
if "data" not in envelope:
    raise SystemExit("missing data envelope field")
PY
}

json_field() {
  local file="$1"
  local path="$2"
  python3 - "$file" "$path" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as fh:
    value = json.load(fh)["data"]
for part in sys.argv[2].split("."):
    if part.isdigit():
        value = value[int(part)]
    else:
        value = value[part]
if isinstance(value, (dict, list)):
    print(json.dumps(value, sort_keys=True))
elif value is None:
    print("")
else:
    print(value)
PY
}

assert_json_contains() {
  local file="$1"
  local needle="$2"
  python3 - "$file" "$needle" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as fh:
    text = json.dumps(json.load(fh), sort_keys=True)
if sys.argv[2] not in text:
    raise SystemExit(f"missing {sys.argv[2]!r} in {sys.argv[1]}")
PY
}

assert_file_contains() {
  local path="$1"
  local needle="$2"
  if ! grep -Fq "$needle" "$path"; then
    echo "expected '$needle' in $path" >&2
    sed -n '1,120p' "$path" >&2 || true
    exit 1
  fi
}

assert_missing() {
  local path="$1"
  if [[ -e "$path" ]]; then
    echo "expected path to be absent: $path" >&2
    exit 1
  fi
}

need git
need aws
need python3
need "$crab_bin"
if [[ "$crab_bin" == */* ]]; then
  crab_bin="$(cd "$(dirname "$crab_bin")" && pwd -P)/$(basename "$crab_bin")"
else
  crab_bin="$(command -v "$crab_bin")"
fi

repo_root="$(git rev-parse --show-toplevel)"
fixture_helper="$repo_root/.codex/skills/crab-cli-verification/scripts/create-rustfs-cli-fixture.sh"
if [[ ! -x "$fixture_helper" ]]; then
  echo "fixture helper is not executable: $fixture_helper" >&2
  exit 1
fi

step "create baseline RustFS fixture"
"$fixture_helper" --run-id "$run_id" --size-mib "$size_mib" --crab-bin "$crab_bin"

run_root="/Volumes/Workspace/CrabCLI/$run_id"
# shellcheck source=/dev/null
source "$run_root/env.sh"
exp_logs="$CRAB_VERIFY_RUN_ROOT/exp-logs"
mkdir -p "$exp_logs"

seed="$CRAB_VERIFY_SEED_REPO"
remote_url="$CRAB_VERIFY_REMOTE_URL"

step "install workflow into seed repo"
(
  cd "$seed"
  rm -f .gitattributes.lock
  mkdir -p src metrics
  python3 - <<'PY'
from pathlib import Path

Path(".crab").mkdir(exist_ok=True)
config = Path(".crab/config.toml")
text = config.read_text(encoding="utf-8") if config.exists() else ""
if "[workflow]" not in text:
    text = text.rstrip() + "\n\n[workflow]\nenabled = true\n"
else:
    lines = text.splitlines()
    out = []
    in_workflow = False
    saw_enabled = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            if in_workflow and not saw_enabled:
                out.append("enabled = true")
            in_workflow = stripped == "[workflow]"
            saw_enabled = False
        if in_workflow and stripped.startswith("enabled"):
            line = "enabled = true"
            saw_enabled = True
        out.append(line)
    if in_workflow and not saw_enabled:
        out.append("enabled = true")
    text = "\n".join(out).rstrip() + "\n"
config.write_text(text, encoding="utf-8")

Path("params.yaml").write_text("model:\n  lr: 0.001\n  epochs: 3\n", encoding="utf-8")
Path("src/train.py").write_text(
    """import json\nimport pathlib\nimport re\n\nroot = pathlib.Path('.')\ntext = (root / 'params.yaml').read_text(encoding='utf-8')\nlr = float(re.search(r'lr:\\s*([0-9.]+)', text).group(1))\nepochs = int(re.search(r'epochs:\\s*([0-9]+)', text).group(1))\nloss = round(1.0 / (1.0 + lr * 1000.0), 6)\n(root / 'metrics').mkdir(exist_ok=True)\n(root / 'out.txt').write_text(f'lr={lr}\\nepochs={epochs}\\nloss={loss}\\n', encoding='utf-8')\n(root / 'metrics/train.json').write_text(json.dumps({'loss': loss, 'lr': lr, 'epochs': epochs}, sort_keys=True) + '\\n', encoding='utf-8')\nprint(f'trained lr={lr} epochs={epochs} loss={loss}')\n""",
    encoding="utf-8",
)
Path("crab.yaml").write_text(
    """params:\n  - params.yaml\nstages:\n  train:\n    cmd: \"python3 src/train.py\"\n    deps:\n      - params.yaml\n      - src/train.py\n    outs:\n      - out.txt\n    metrics:\n      - metrics/train.json\n    params:\n      - model.lr\n      - model.epochs\n""",
    encoding="utf-8",
)
PY
  git add -f .crab/config.toml crab.yaml params.yaml src/train.py
  git commit -m "add exp verification workflow"
) >"$exp_logs/install-workflow.stdout.log" 2>"$exp_logs/install-workflow.stderr.log"

run_log_in "push workflow commit" "$seed" \
  "$crab_bin" push --json --upload-concurrency 0 origin HEAD:refs/heads/main

step "verify exp parser aliases"
for sub in run show diff ls list promote branch apply save rename push pull remove rm clean gc queue start status stop; do
  "$crab_bin" exp "$sub" --help >"$exp_logs/help-exp-$sub.stdout.log" 2>"$exp_logs/help-exp-$sub.stderr.log"
done

cd "$seed"

run_json "exp-run-low" workflow.exp.run \
  "$crab_bin" exp run -S model.lr=0.01 -n low-lr -m "low lr" --json
exp1="$(json_field "$exp_logs/exp-run-low.stdout.log" exp_id)"
assert_json_contains ".crab/workflow/exp/$exp1.meta.json" "low-lr"
[[ -d ".crab/workflow/exp/$exp1.workspace" ]]

run_json "exp-run-high" workflow.exp.run \
  "$crab_bin" exp run -S model.lr=0.02 -n high-lr -m "high lr" --json
exp2="$(json_field "$exp_logs/exp-run-high.stdout.log" exp_id)"
assert_json_contains ".crab/workflow/exp/$exp2.meta.json" "high-lr"
[[ -d ".crab/workflow/exp/$exp2.workspace" ]]

run_json "exp-show-list" workflow.exp.show "$crab_bin" exp show --json
assert_json_contains "$exp_logs/exp-show-list.stdout.log" "$exp1"
assert_json_contains "$exp_logs/exp-show-list.stdout.log" "$exp2"

run_json "exp-show-one" workflow.exp.show "$crab_bin" exp show "$exp1" --json
assert_json_contains "$exp_logs/exp-show-one.stdout.log" "low-lr"
assert_json_contains "$exp_logs/exp-show-one.stdout.log" "model.lr"

run_json "exp-ls" workflow.exp.ls "$crab_bin" exp ls --json
assert_json_contains "$exp_logs/exp-ls.stdout.log" "$exp1"

run_json "exp-list-alias" workflow.exp.ls "$crab_bin" exp list --json
assert_json_contains "$exp_logs/exp-list-alias.stdout.log" "$exp2"

run_json "exp-diff" workflow.exp.diff "$crab_bin" exp diff "$exp1" "$exp2" --json
assert_json_contains "$exp_logs/exp-diff.stdout.log" "model.lr"
assert_json_contains "$exp_logs/exp-diff.stdout.log" "0.02"

run_json "exp-rename" workflow.exp.rename "$crab_bin" exp rename "$exp2" renamed-high --json
run_json "exp-show-renamed" workflow.exp.show "$crab_bin" exp show "$exp2" --json
assert_json_contains "$exp_logs/exp-show-renamed.stdout.log" "renamed-high"

run_json "exp-promote" workflow.exp.promote "$crab_bin" exp promote "$exp1" promote-exp1 --json
git show-ref --verify --quiet refs/heads/promote-exp1

run_json "exp-branch-alias" workflow.exp.promote "$crab_bin" exp branch "$exp2" branch-exp2 --json
git show-ref --verify --quiet refs/heads/branch-exp2

run_json "exp-apply" workflow.exp.apply "$crab_bin" exp apply "$exp1" --json
assert_file_contains params.yaml "lr: 0.01"
assert_file_contains out.txt "lr=0.01"
assert_file_contains metrics/train.json '"lr": 0.01'

run_json "exp-save" workflow.exp.save \
  "$crab_bin" exp save -n workspace-save -m "workspace snapshot" --include-untracked out.txt --json
save_id="$(json_field "$exp_logs/exp-save.stdout.log" exp_id)"
assert_json_contains ".crab/workflow/exp/$save_id.meta.json" "saved"

run_json "exp-queue" workflow.exp.queue \
  "$crab_bin" exp queue -S model.lr=0.03 -m "queued lr" --json
queue_id="$(json_field "$exp_logs/exp-queue.stdout.log" experiment_ids.0)"

run_json "exp-status-pending" workflow.exp.status "$crab_bin" exp status --json
assert_json_contains "$exp_logs/exp-status-pending.stdout.log" '"pending": 1'

run_json "exp-start" workflow.exp.start "$crab_bin" exp start --jobs 1 --json
assert_json_contains "$exp_logs/exp-start.stdout.log" '"succeeded": 1'
assert_json_contains ".crab/workflow/exp/$queue_id.meta.json" "success"

run_json "queue-logs" workflow.exp.queue.logs "$crab_bin" queue logs "$queue_id" --json
assert_json_contains "$exp_logs/queue-logs.stdout.log" "$queue_id"

run_json "exp-status-done" workflow.exp.status "$crab_bin" exp status --json
assert_json_contains "$exp_logs/exp-status-done.stdout.log" '"done": 1'

run_json "exp-stop" workflow.exp.stop "$crab_bin" exp stop --json

run_json "queue-remove-success" workflow.exp.queue.remove "$crab_bin" queue remove --success --json
assert_json_contains "$exp_logs/queue-remove-success.stdout.log" "$queue_id"

run_json "exp-run-queue-shortcut" workflow.exp.queue \
  "$crab_bin" exp run --queue -S model.lr=0.04,0.05 -n sweep --json
run_json "exp-run-all-shortcut" workflow.exp.start \
  "$crab_bin" exp run --run-all --jobs 2 --json
assert_json_contains "$exp_logs/exp-run-all-shortcut.stdout.log" '"succeeded": 2'

run_json "exp-push-all" workflow.exp.push "$crab_bin" exp push --all --json
assert_json_contains "$exp_logs/exp-push-all.stdout.log" "$exp1"
assert_json_contains "$exp_logs/exp-push-all.stdout.log" "$save_id"

aws --endpoint-url "$AWS_ENDPOINT_URL" s3 ls "s3://crab/verify-cli/$run_id/workflow/exp/" \
  --recursive >"$exp_logs/aws-exp-remote-ls.stdout.log"
assert_file_contains "$exp_logs/aws-exp-remote-ls.stdout.log" "$exp1"

pull_clone="$CRAB_VERIFY_RUN_ROOT/exp-pull-clone"
rm -rf "$pull_clone"
run_log "clone for exp pull" "$crab_bin" clone "$remote_url" "$pull_clone" --jsonl
(
  cd "$pull_clone"
  run_json "exp-pull-all" workflow.exp.pull "$crab_bin" exp pull --all --json
  assert_json_contains ".crab/workflow/exp/$exp1.meta.json" "low-lr"
  run_json "exp-pull-show-exp1" workflow.exp.show "$crab_bin" exp show "$exp1" --json
  run_json "exp-pull-apply-exp1" workflow.exp.apply "$crab_bin" exp apply "$exp1" --json
  assert_file_contains params.yaml "lr: 0.01"
  assert_file_contains out.txt "lr=0.01"
)

cd "$seed"

run_json "exp-rm-alias" workflow.exp.remove "$crab_bin" exp rm "$exp2" --json
assert_missing ".crab/workflow/exp/$exp2.meta.json"

run_json "exp-remove-local" workflow.exp.remove "$crab_bin" exp remove "$save_id" --json
assert_missing ".crab/workflow/exp/$save_id.meta.json"

run_json "exp-remove-remote" workflow.exp.remove \
  "$crab_bin" exp remove --git-remote "$remote_url" "$save_id" --json
if "$crab_bin" exp pull "$save_id" --force --json \
  >"$exp_logs/exp-pull-removed-remote.stdout.log" \
  2>"$exp_logs/exp-pull-removed-remote.stderr.log"; then
  echo "expected exp pull of remote-removed experiment to fail" >&2
  exit 1
fi

run_json "exp-clean" workflow.exp.clean "$crab_bin" exp clean --json
run_json "exp-gc" workflow.exp.gc "$crab_bin" exp gc --keep 1 --json

cat >"$CRAB_VERIFY_RUN_ROOT/exp-summary.txt" <<EOF
run_id=$run_id
remote_url=$remote_url
seed_repo=$seed
pull_clone=$pull_clone
exp_logs=$exp_logs
exp1=$exp1
exp2=$exp2
save_id=$save_id
queue_id=$queue_id
EOF

cat "$CRAB_VERIFY_RUN_ROOT/exp-summary.txt"
printf '\nCrab exp RustFS verification passed.\n'
