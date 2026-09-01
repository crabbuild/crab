# Crab — build, test, and install
#
# Usage:
#   make              Build release binary
#   make install      Build and install crab, crab-cache-server, and symlinks
#   make debug        Build debug binary
#   make test         Run full test suite
#   make clippy       Run clippy lints
#   make split-crate-clippy-check
#                     Run strict clippy on split crates without legacy CLI backlog
#   make split-crate-test-check
#                     Run unit/doc tests for split crates
#   make fmt          Format code
#   make check        cargo check (fast compile check)
#   make architecture-check
#                     Check multi-crate architecture guardrails
#   make crate-interface-check
#                     Check split-crate Interface compile slices
#   make crate-behavior-check
#                     Check focused split-crate behavior slices
#   make shipped-binary-version-check
#                     Check shipped binary --version contracts
#   make install-layout-check
#                     Check staged local install layout
#   make auth-helper-packaging-check
#                     Check auth helper deployment packaging
#   make release-archive-contents-check
#                     Check hosted release archive contents
#   make final-integration-check
#                     Check bounded no-cloud integration flows across split seams
#   make crab-types-admission
#                     Check crab-types public-surface admission ledger
#   make replica-feature-matrix
#                     Check enterprise replica feature slices
#   make replica-feature-matrix-locked
#                     Check enterprise replica feature slices with Cargo.lock
#   make replica-live-preflight
#                     Check live enterprise replica evidence env before cloud tests
#   make replica-release-evidence
#                     Verify retained enterprise live evidence for release
#   make cache-service-release-gate
#                     Verify retained cache-service RustFS smoke evidence for release
#   make nfs-release-gate
#                     Verify retained macOS/Linux/Windows NFS smoke evidence for release
#   make nfs-release-evidence-ci
#                     Dispatch hosted NFS Mount Evidence workflow for release
#                     Optional: NFS_RELEASE_EVIDENCE_WAIT=1 waits and prints
#                     the exact run id and run-attempt suffix for release gates
#                     Optional: NFS_RELEASE_EVIDENCE_OUTPUT=nfs-evidence.env
#                     writes sourceable variables after a successful wait
#   make nfs-release-evidence-dispatch-self-test
#                     Exercise NFS release evidence dispatch wait/output flow
#   make nfs-evidence-summary-self-test
#                     Exercise NFS evidence summary and threshold rendering
#   make nfs-threshold-suggestions
#                     Suggest NFS verify/compare threshold args from retained
#                     benchmark and native smoke evidence. Optional:
#                     NFS_THRESHOLD_BENCHMARK_REPORTS="a.json b.json"
#                     NFS_THRESHOLD_SMOKE_SUMMARIES="a.json b.json"
#                     NFS_THRESHOLD_BENCHMARK_DIRS="run-a run-b"
#                     NFS_THRESHOLD_SMOKE_DIRS="run-a run-b"
#                     NFS_THRESHOLD_MIN_BENCHMARK_REPORTS=2
#                     NFS_THRESHOLD_MIN_SMOKE_SUMMARIES=2
#                     NFS_THRESHOLD_REQUIRE_CALIBRATION_READY=1
#                     NFS_THRESHOLD_REQUIRE_RELEASE_GRADE=1
#   make mount-large-rustfs-smoke
#                     Verify native Linux NFS large-file reads, concurrent
#                     overlay writes, and Git/Xet writeback against RustFS/S3.
#   make mount-nfs-linux-smoke
#                     Verify native Linux NFS `crab mount` through Docker.
#   make mount-nfs-macos-smoke
#                     Verify native macOS NFS `crab mount`.
#   make mount-nfs-windows-smoke
#                     Verify native Windows NFS `crab mount` through Client for NFS.
#   make nfs-smoke-report-verify
#                     Verify a retained native NFS smoke JSON report and artifacts.
#                     Optional: NFS_SMOKE_VERIFY_ARGS="--max-native-read-rpcs-per-mib 32"
#   make nfs-smoke-report-verify-dir
#                     Verify retained Linux/macOS/Windows NFS smoke reports.
#                     Optional: NFS_SMOKE_EXPECTED_RUN_SUFFIX="<github-run-id>-<attempt>"
#                     Optional: NFS_SMOKE_EXPECTED_GIT_COMMIT="<full-git-object-id>"
#   make nfs-smoke-report-compare
#                     Compare retained baseline/current native NFS smoke reports.
#                     Optional: NFS_SMOKE_COMPARE_ARGS="--max-native-read-throughput-regression-pct 20"
#   make nfs-smoke-report-compare-dir
#                     Compare retained baseline/current native NFS smoke directories.
#   make nfs-smoke-report-verify-self-test
#                     Exercise native NFS smoke report verifier regressions.
#   make nfs-smoke-script-check
#                     Syntax-check native NFS smoke wrappers where local tools exist.
#   make nfs-feature-gate
#                     Verify NFS-only builds do not depend on FUSE IPC and
#                     FUSE IPC contracts still run when FUSE is enabled.
#   make nfs-read-path-bench-report-verify
#                     Verify a retained NFS read-path benchmark report.
#                     Optional: NFS_READ_PATH_BENCH_VERIFY_ARGS="--min-pointer-sequential-lease-ratio 1"
#                     Optional: NFS_READ_PATH_BENCH_EXPECTED_GIT_COMMIT="<full-git-object-id>"
#   make nfs-read-path-bench-report-compare
#                     Compare retained NFS read-path benchmark reports.
#                     Optional: NFS_READ_PATH_BENCH_COMPARE_ARGS="--max-throughput-regression-pct 10"
#   make nfs-read-path-bench-report-self-test
#                     Exercise NFS read-path benchmark report verifier regressions.
#   make mount-large-macos-rustfs-smoke
#                     Verify large-file `crab mount` on native macOS/macFUSE
#                     against Docker RustFS/S3.
#   make mount-large-macos-nfs-rustfs-smoke
#                     Verify large files and a 10k-entry directory through
#                     native macOS NFS against Docker RustFS/S3.
#   make cache-service-rustfs-smoke
#                     Prove onboarding bundle wiring and traffic reduction against RustFS/S3
#   make cache-service-onboarding-rustfs-smoke
#                     Alias for the onboarding-backed RustFS/S3 smoke
#   make cache-service-verify-smoke-report
#                     Verify retained cache-service RustFS smoke evidence
#   make cache-service-validate-manifests
#                     Validate cache-service Kubernetes observability manifests
#   make clean        Remove build artifacts
#   make uninstall    Remove installed binaries and symlinks
#
# Version bumping:
#   make bump-patch       Bump patch (0.1.0 → 0.1.1)
#   make bump-minor       Bump minor (0.1.0 → 0.2.0)
#   make bump-major       Bump major (0.1.0 → 1.0.0)
#   make bump-set VERSION=2.3.4   Set explicit version
#   make release-patch    Bump patch + build + install
#   make release-minor    Bump minor + build + install
#   make release-major    Bump major + build + install
#
# Release & Homebrew:
#   make release          Dispatch the hosted full-matrix GitHub release
#   make release-build    Build local release archives, no upload
#   make release-target TARGET=linux-x86_64
#   make release-list     Show supported release targets
#   make release-ci       Dispatch the hosted GitHub release workflow
#                         CLI artifacts only by default. Set
#                         RELEASE_REQUIRE_ENTERPRISE_EVIDENCE=1 to require
#                         retained replica/cache-service evidence,
#                         RELEASE_REQUIRE_NFS_EVIDENCE=1 for native mount
#                         evidence, or RELEASE_REQUIRE_CLOUD_EVIDENCE=1 for
#                         live AWS and cross-platform protocol evidence.
#   make release-full     Release + update Homebrew tap
#   make homebrew         Update tap from latest GitHub release
#   make homebrew-local   Update tap using local dist/ checksums
#   make homebrew-dry     Render formula without pushing

CARGO       ?= cargo
POWERSHELL  ?= pwsh
PYTHON      ?= python3
PREFIX      ?= $(HOME)/.cargo/bin
CARGO_BIN   := $(HOME)/.cargo/bin
BINARY_NAME := crab
CACHE_SERVER_NAME := crab-cache-server
FUSE_MOUNT_NAME := crab-fuse-mount
NFS_MOUNT_NAME := crab-nfs-mount
REMOTE_LINK := git-remote-crab
REPLICA_LIVE_SUITE ?= enterprise
REPLICA_LIVE_STORAGE_PROVIDER ?= all
REPLICA_LIVE_COORDINATOR ?= all
REPLICA_LIVE_HYDRATE_PROVIDER ?= all
REPLICA_LIVE_EVIDENCE_PROFILE ?= enterprise
REPLICA_LIVE_REQUIRE_CLOUD_CREDENTIALS ?= 1
REPLICA_RELEASE_EVIDENCE_DIR ?= ../replica-live-evidence
REPLICA_RELEASE_EVIDENCE_OUTPUT ?= replica-release-evidence-verify.json
REPLICA_RELEASE_EVIDENCE_RUN_ID ?=
REPLICA_RELEASE_EVIDENCE_ARTIFACT ?=
REPLICA_RELEASE_EVIDENCE_EXPECTED_RUN_ID ?=
REPLICA_RELEASE_REQUIRE_EVIDENCE ?= 1
REPLICA_RELEASE_REQUIRE_EXPECTED_RUN_ID ?= 1
CACHE_SERVICE_RUSTFS_ENDPOINT ?= http://127.0.0.1:9000
CACHE_SERVICE_RUSTFS_BUCKET ?= crab
CACHE_SERVICE_RUSTFS_ACCESS_KEY ?= $(if $(AWS_ACCESS_KEY_ID),$(AWS_ACCESS_KEY_ID),crab)
CACHE_SERVICE_RUSTFS_SECRET_KEY ?= $(if $(AWS_SECRET_ACCESS_KEY),$(AWS_SECRET_ACCESS_KEY),crab)
CACHE_SERVICE_RUSTFS_REGION ?= $(if $(AWS_REGION),$(AWS_REGION),us-east-1)
CACHE_SERVICE_RUSTFS_ROOT ?= /tmp/crab-cache-service-smoke
CACHE_SERVICE_RUSTFS_EXTRA_ARGS ?=
CACHE_SERVICE_RUSTFS_REPORT ?=
CACHE_SERVICE_RUSTFS_VERIFY_OUTPUT ?=
CACHE_SERVICE_RELEASE_EVIDENCE_DIR ?= ../cache-service-release-evidence
CACHE_SERVICE_RELEASE_EVIDENCE_RUN_ID ?=
CACHE_SERVICE_RELEASE_EVIDENCE_ARTIFACT ?=
CACHE_SERVICE_RELEASE_EXPECTED_RUN_ID ?=
CACHE_SERVICE_RELEASE_VERIFY_OUTPUT ?= cache-service-release-evidence-verify.json
CACHE_SERVICE_RELEASE_SUMMARY_OUTPUT ?= cache-service-release-evidence-summary.json
CACHE_SERVICE_RELEASE_DOCTOR_OUTPUT ?= cache-service-release-evidence-doctor.json
CACHE_SERVICE_RELEASE_DOCTOR_TEXT_OUTPUT ?= cache-service-release-evidence-doctor.txt
CACHE_SERVICE_RELEASE_REQUIRE_EVIDENCE ?= 1
CACHE_SERVICE_RELEASE_REQUIRE_EXPECTED_RUN_ID ?= 1
NFS_READ_PATH_BENCH_BASELINE_REPORT ?=
NFS_READ_PATH_BENCH_COMPARE_ARGS ?=
NFS_READ_PATH_BENCH_COMPARE_OUTPUT ?=
NFS_READ_PATH_BENCH_EXPECTED_GIT_COMMIT ?=
NFS_READ_PATH_BENCH_EXPECTED_RUN_ID ?=
NFS_READ_PATH_BENCH_REPORT ?= nfs-read-path-bench-report.json
NFS_READ_PATH_BENCH_VERIFY_ARGS ?=
NFS_SMOKE_BASELINE_REPORT ?=
NFS_SMOKE_BASELINE_REPORT_DIR ?=
NFS_SMOKE_COMPARE_ARGS ?=
NFS_SMOKE_COMPARE_OUTPUT ?=
NFS_SMOKE_COMPARE_SUMMARY ?=
NFS_SMOKE_REPORT ?=
NFS_SMOKE_REPORT_DIR ?=
NFS_SMOKE_VERIFY_ARGS ?=
NFS_SMOKE_VERIFY_SUMMARY ?=
NFS_SMOKE_EXPECTED_RUN_SUFFIX ?=
NFS_SMOKE_EXPECTED_GIT_COMMIT ?=
NFS_THRESHOLD_BENCHMARK_REPORT ?=
NFS_THRESHOLD_BENCHMARK_REPORTS ?=
NFS_THRESHOLD_BENCHMARK_DIR ?=
NFS_THRESHOLD_BENCHMARK_DIRS ?=
NFS_THRESHOLD_SMOKE_SUMMARY ?=
NFS_THRESHOLD_SMOKE_SUMMARIES ?=
NFS_THRESHOLD_SMOKE_DIR ?=
NFS_THRESHOLD_SMOKE_DIRS ?=
NFS_THRESHOLD_SUGGESTION_OUTPUT ?= nfs-threshold-suggestions.env
NFS_THRESHOLD_SUGGESTION_JSON ?= nfs-threshold-suggestions.json
NFS_THRESHOLD_BENCHMARK_MARGIN_PCT ?= 20
NFS_THRESHOLD_SMOKE_MARGIN_PCT ?= 25
NFS_THRESHOLD_BENCHMARK_REGRESSION_PCT ?= 20
NFS_THRESHOLD_SMOKE_REGRESSION_PCT ?= 20
NFS_THRESHOLD_MIN_BENCHMARK_REPORTS ?= 1
NFS_THRESHOLD_MIN_SMOKE_SUMMARIES ?= 1
NFS_THRESHOLD_REQUIRE_CALIBRATION_READY ?= 0
NFS_THRESHOLD_REQUIRE_RELEASE_GRADE ?= 0
NFS_RELEASE_EVIDENCE_DIR ?= ../nfs-release-evidence
NFS_RELEASE_EVIDENCE_RUN_ID ?=
NFS_RELEASE_EVIDENCE_REF ?=
NFS_RELEASE_EVIDENCE_WAIT ?= 0
NFS_RELEASE_EVIDENCE_WAIT_TIMEOUT_SECS ?= 7200
NFS_RELEASE_EVIDENCE_POLL_SECS ?= 30
NFS_RELEASE_EVIDENCE_OUTPUT ?=
NFS_RELEASE_EXPECTED_RUN_SUFFIX ?=
NFS_RELEASE_EXPECTED_GIT_COMMIT ?= $(shell git -C .. rev-parse HEAD 2>/dev/null)
NFS_RELEASE_SUMMARY_OUTPUT ?= nfs-release-smoke-summary.json
NFS_RELEASE_VERIFY_ARGS ?=
NFS_RELEASE_REQUIRE_EVIDENCE ?= 1
NFS_RELEASE_REQUIRE_EXPECTED_RUN_SUFFIX ?= 1
NFS_RELEASE_REQUIRE_EXPECTED_GIT_COMMIT ?= 1
RELEASE_REQUIRE_ENTERPRISE_EVIDENCE ?= 0
RELEASE_REQUIRE_NFS_EVIDENCE ?= 0
RELEASE_REQUIRE_CLOUD_EVIDENCE ?= 0

# Release build flags
RELEASE_FLAGS := --release
UNAME_S := $(shell uname -s)
CRAB_CLI_FEATURES_NO_FUSE := simd-accel,tier,replication-s3-control-plane,replication-gcs-control-plane,replication-azure-control-plane,coordinator-dynamodb,coordinator-spanner,coordinator-cosmosdb,watch,nfs,gix-pathmatch
CRAB_CLI_FEATURES_WITH_FUSE := $(CRAB_CLI_FEATURES_NO_FUSE),fuse

# Cargo workspace puts artifacts in the workspace root target dir,
# not the crate-local one. Resolve via `cargo metadata`.
TARGET_DIR  := $(shell $(CARGO) metadata --format-version=1 --no-deps 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin)['target_directory'])" 2>/dev/null || echo "../target")
RELEASE_BIN := $(TARGET_DIR)/release/$(BINARY_NAME)
FUSE_MOUNT_RELEASE_BIN := $(TARGET_DIR)/release/$(FUSE_MOUNT_NAME)
NFS_MOUNT_RELEASE_BIN := $(TARGET_DIR)/release/$(NFS_MOUNT_NAME)
CACHE_SERVER_RELEASE_BIN := $(TARGET_DIR)/release/$(CACHE_SERVER_NAME)
DEBUG_BIN := $(TARGET_DIR)/debug/$(BINARY_NAME)
FUSE_MOUNT_DEBUG_BIN := $(TARGET_DIR)/debug/$(FUSE_MOUNT_NAME)
NFS_MOUNT_DEBUG_BIN := $(TARGET_DIR)/debug/$(NFS_MOUNT_NAME)
CACHE_SERVER_DEBUG_BIN := $(TARGET_DIR)/debug/$(CACHE_SERVER_NAME)
FUSE_MOUNT_INSTALL_ARGS :=
NFS_MOUNT_INSTALL_ARGS := --nfs-mount-bin "$(NFS_MOUNT_RELEASE_BIN)" --nfs-mount-name "$(NFS_MOUNT_NAME)"
ifeq ($(UNAME_S),Darwin)
FUSE_MOUNT_INSTALL_ARGS := --fuse-mount-bin "$(FUSE_MOUNT_RELEASE_BIN)" --fuse-mount-name "$(FUSE_MOUNT_NAME)"
endif
ifeq ($(UNAME_S),Linux)
FUSE_MOUNT_INSTALL_ARGS := --fuse-mount-bin "$(FUSE_MOUNT_RELEASE_BIN)" --fuse-mount-name "$(FUSE_MOUNT_NAME)"
endif

SPLIT_CRATE_PACKAGES := \
	-p crab-types \
	-p crab-xet \
	-p crab-auth \
	-p crab-auth-server \
	-p crab-auth-store \
	-p crab-cache \
	-p crab-cache-server \
	-p crab-cache-store \
	-p crab-coordination \
	-p crab-diff \
	-p crab-git \
	-p crab-lfs \
	-p crab-metadata \
	-p crab-read \
	-p crab-staging \
	-p crab-storage \
	-p crab-workflow

.PHONY: all build debug test clippy split-crate-clippy-check split-crate-test-check fmt check architecture-check crate-interface-check crate-behavior-check shipped-binary-version-check install-layout-check auth-helper-packaging-check release-archive-contents-check final-integration-check crab-types-admission replica-feature-matrix replica-feature-matrix-all replica-feature-matrix-locked replica-feature-matrix-all-locked replica-live-preflight replica-release-evidence replica-release-gate mount-large-rustfs-smoke mount-nfs-linux-smoke mount-nfs-macos-smoke mount-nfs-windows-smoke nfs-smoke-report-verify nfs-smoke-report-verify-dir nfs-smoke-report-compare nfs-smoke-report-compare-dir nfs-smoke-report-verify-self-test nfs-smoke-script-check nfs-release-evidence-dispatch-self-test nfs-evidence-summary-self-test nfs-feature-gate nfs-threshold-suggestions nfs-read-path-bench-report-verify nfs-read-path-bench-report-compare nfs-read-path-bench-report-self-test mount-large-macos-rustfs-smoke mount-large-macos-nfs-rustfs-smoke cache-service-release-gate cache-service-validate-manifests cache-service-rustfs-smoke cache-service-onboarding-rustfs-smoke cache-service-verify-smoke-report clean install uninstall release release-build release-target release-list release-metadata-check release-ci release-full homebrew homebrew-local homebrew-dry
.PHONY: nfs-release-gate nfs-release-evidence-ci

all: build

# --- Build ---

build:
	@set -e; if [ "$(UNAME_S)" = "Darwin" ]; then \
		$(CARGO) build $(RELEASE_FLAGS) -p crab --bin $(BINARY_NAME) --no-default-features --features "$(CRAB_CLI_FEATURES_WITH_FUSE)"; \
		cp "$(RELEASE_BIN)" "$(FUSE_MOUNT_RELEASE_BIN)"; \
		$(CARGO) build $(RELEASE_FLAGS) -p crab --bin $(BINARY_NAME) --no-default-features --features "$(CRAB_CLI_FEATURES_NO_FUSE)"; \
		ln -sf "$(BINARY_NAME)" "$(NFS_MOUNT_RELEASE_BIN)"; \
		$(CARGO) build $(RELEASE_FLAGS) -p crab-cache-server --bin $(CACHE_SERVER_NAME); \
	else \
		$(CARGO) build $(RELEASE_FLAGS) -p crab --bin $(BINARY_NAME) --no-default-features --features "$(CRAB_CLI_FEATURES_WITH_FUSE)"; \
		cp "$(RELEASE_BIN)" "$(FUSE_MOUNT_RELEASE_BIN)"; \
		$(CARGO) build $(RELEASE_FLAGS) -p crab --bin $(BINARY_NAME) --no-default-features --features "$(CRAB_CLI_FEATURES_NO_FUSE)"; \
		ln -sf "$(BINARY_NAME)" "$(NFS_MOUNT_RELEASE_BIN)"; \
		$(CARGO) build $(RELEASE_FLAGS) -p crab-cache-server --bin $(CACHE_SERVER_NAME); \
	fi

debug:
	@if [ "$(UNAME_S)" = "Darwin" ]; then \
		$(CARGO) build -p crab --bin $(BINARY_NAME) --no-default-features --features "$(CRAB_CLI_FEATURES_WITH_FUSE)"; \
		cp "$(DEBUG_BIN)" "$(FUSE_MOUNT_DEBUG_BIN)"; \
		$(CARGO) build -p crab --bin $(BINARY_NAME) --no-default-features --features "$(CRAB_CLI_FEATURES_NO_FUSE)"; \
		ln -sf "$(BINARY_NAME)" "$(NFS_MOUNT_DEBUG_BIN)"; \
		$(CARGO) build -p crab-cache-server --bin $(CACHE_SERVER_NAME); \
	else \
		$(CARGO) build -p crab --bin $(BINARY_NAME) --no-default-features --features "$(CRAB_CLI_FEATURES_WITH_FUSE)"; \
		cp "$(DEBUG_BIN)" "$(FUSE_MOUNT_DEBUG_BIN)"; \
		$(CARGO) build -p crab --bin $(BINARY_NAME) --no-default-features --features "$(CRAB_CLI_FEATURES_NO_FUSE)"; \
		ln -sf "$(BINARY_NAME)" "$(NFS_MOUNT_DEBUG_BIN)"; \
		$(CARGO) build -p crab-cache-server --bin $(CACHE_SERVER_NAME); \
	fi

# --- Quality ---

test:
	$(CARGO) test
	$(CARGO) test --test error_codes

clippy:
	$(CARGO) clippy -- -D warnings

split-crate-clippy-check:
	$(CARGO) clippy $(SPLIT_CRATE_PACKAGES) -- -D warnings

split-crate-test-check:
	CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 $(CARGO) test $(SPLIT_CRATE_PACKAGES)

fmt:
	$(CARGO) fmt

fmt-check:
	$(CARGO) fmt --check

check:
	$(CARGO) check

architecture-check: crab-types-admission
	@$(PYTHON) scripts/check-architecture-gates.py --cargo "$(CARGO)"

crate-interface-check:
	@$(PYTHON) scripts/check-crate-interface-builds.py --cargo "$(CARGO)"

crate-behavior-check:
	@$(PYTHON) scripts/check-crate-behavior.py --cargo "$(CARGO)"

shipped-binary-version-check:
	@$(PYTHON) scripts/release/check-shipped-binary-versions.py --cargo "$(CARGO)"

install-layout-check:
	@$(PYTHON) scripts/release/check-install-layout.py --cargo "$(CARGO)"

auth-helper-packaging-check:
	@$(PYTHON) scripts/release/check-auth-helper-packaging.py

release-archive-contents-check:
	@$(PYTHON) scripts/release/check-release-archive-contents.py
	@$(PYTHON) scripts/release/generate-release-manifest.py --self-test

final-integration-check:
	@$(PYTHON) scripts/check-final-integration.py --cargo "$(CARGO)"

crab-types-admission:
	@$(PYTHON) scripts/check-crab-types-admission.py --cargo "$(CARGO)"

replica-feature-matrix:
	@CARGO="$(CARGO)" ./scripts/check-replica-feature-matrix.sh

replica-feature-matrix-all:
	@CARGO="$(CARGO)" ./scripts/check-replica-feature-matrix.sh --include-all-cloud

replica-feature-matrix-locked:
	@CARGO="$(CARGO)" ./scripts/check-replica-feature-matrix.sh --locked

replica-feature-matrix-all-locked:
	@CARGO="$(CARGO)" ./scripts/check-replica-feature-matrix.sh --locked --include-all-cloud

replica-live-preflight:
	@extra_flags=""; \
	if [ "$(REPLICA_LIVE_REQUIRE_CLOUD_CREDENTIALS)" = "1" ]; then \
		extra_flags="--require-cloud-credentials"; \
	fi; \
	./scripts/check-replica-live-env.sh \
		--suite "$(REPLICA_LIVE_SUITE)" \
		--storage-provider "$(REPLICA_LIVE_STORAGE_PROVIDER)" \
		--coordinator "$(REPLICA_LIVE_COORDINATOR)" \
		--hydrate-provider "$(REPLICA_LIVE_HYDRATE_PROVIDER)" \
		--evidence-profile "$(REPLICA_LIVE_EVIDENCE_PROFILE)" \
		--mutate \
		--require-evidence \
		--require-redacted \
		$$extra_flags

replica-release-evidence:
	@./scripts/release/verify-replica-release-evidence.sh \
		"$(REPLICA_RELEASE_EVIDENCE_DIR)" \
		"$(REPLICA_RELEASE_EVIDENCE_OUTPUT)" \
		"$(REPLICA_RELEASE_EVIDENCE_EXPECTED_RUN_ID)"

replica-release-gate:
	@if [ "$(REPLICA_RELEASE_REQUIRE_EVIDENCE)" = "1" ]; then \
		if [ "$(REPLICA_RELEASE_REQUIRE_EXPECTED_RUN_ID)" = "1" ] && [ -z "$(REPLICA_RELEASE_EVIDENCE_EXPECTED_RUN_ID)" ]; then \
			echo "REPLICA_RELEASE_EVIDENCE_EXPECTED_RUN_ID is required for local release archives"; \
			echo "Use the live workflow run-attempt ID: replica-live-<github-run-id>-<attempt>."; \
			echo "Set REPLICA_RELEASE_REQUIRE_EVIDENCE=0 only for non-release smoke builds."; \
			exit 2; \
		fi; \
		$(MAKE) --no-print-directory replica-release-evidence; \
	else \
		echo "warning: skipping replica release evidence gate; this build is not release-grade" >&2; \
	fi

mount-large-rustfs-smoke:
	@./scripts/docker/run-mount-large-rustfs-smoke.sh

mount-nfs-linux-smoke:
	@./scripts/docker/run-mount-nfs-linux-smoke.sh

mount-nfs-macos-smoke:
	@./scripts/run-mount-nfs-macos-smoke.sh

mount-nfs-windows-smoke:
	@$(POWERSHELL) -NoProfile -ExecutionPolicy Bypass -File scripts/run-mount-nfs-windows-smoke.ps1

nfs-smoke-report-verify:
	@if [ -z "$(NFS_SMOKE_REPORT)" ]; then \
		echo "NFS_SMOKE_REPORT is required"; \
		exit 2; \
	fi
	@expected_commit_args=""; \
	if [ -n "$(NFS_SMOKE_EXPECTED_GIT_COMMIT)" ]; then \
		expected_commit_args="--expected-git-commit $(NFS_SMOKE_EXPECTED_GIT_COMMIT)"; \
	fi; \
	$(PYTHON) scripts/verify-nfs-smoke-report.py "$(NFS_SMOKE_REPORT)" \
		--require-artifacts \
		$$expected_commit_args \
		$(NFS_SMOKE_VERIFY_ARGS)

nfs-smoke-report-verify-dir:
	@if [ -z "$(NFS_SMOKE_REPORT_DIR)" ]; then \
		echo "NFS_SMOKE_REPORT_DIR is required"; \
		exit 2; \
	fi
	@summary_args=""; \
	expected_run_args=""; \
	expected_commit_args=""; \
	if [ -n "$(NFS_SMOKE_VERIFY_SUMMARY)" ]; then \
		summary_args="--summary-output $(NFS_SMOKE_VERIFY_SUMMARY)"; \
	fi; \
	if [ -n "$(NFS_SMOKE_EXPECTED_RUN_SUFFIX)" ]; then \
		expected_run_args="--expected-run-suffix $(NFS_SMOKE_EXPECTED_RUN_SUFFIX)"; \
	fi; \
	if [ -n "$(NFS_SMOKE_EXPECTED_GIT_COMMIT)" ]; then \
		expected_commit_args="--expected-git-commit $(NFS_SMOKE_EXPECTED_GIT_COMMIT)"; \
	fi; \
	$(PYTHON) scripts/verify-nfs-smoke-report.py verify-dir "$(NFS_SMOKE_REPORT_DIR)" \
		--require-artifacts \
		--require-all-platforms \
		$$expected_run_args \
		$$expected_commit_args \
		$$summary_args \
		$(NFS_SMOKE_VERIFY_ARGS)

nfs-smoke-report-compare:
	@if [ -z "$(NFS_SMOKE_BASELINE_REPORT)" ]; then \
		echo "NFS_SMOKE_BASELINE_REPORT is required"; \
		exit 2; \
	fi
	@if [ -z "$(NFS_SMOKE_REPORT)" ]; then \
		echo "NFS_SMOKE_REPORT is required"; \
		exit 2; \
	fi
	@output_args=""; \
	if [ -n "$(NFS_SMOKE_COMPARE_OUTPUT)" ]; then \
		output_args="--output $(NFS_SMOKE_COMPARE_OUTPUT)"; \
	fi; \
	$(PYTHON) scripts/verify-nfs-smoke-report.py compare \
		"$(NFS_SMOKE_BASELINE_REPORT)" \
		"$(NFS_SMOKE_REPORT)" \
		$$output_args \
		$(NFS_SMOKE_COMPARE_ARGS)

nfs-smoke-report-compare-dir:
	@if [ -z "$(NFS_SMOKE_BASELINE_REPORT_DIR)" ]; then \
		echo "NFS_SMOKE_BASELINE_REPORT_DIR is required"; \
		exit 2; \
	fi
	@if [ -z "$(NFS_SMOKE_REPORT_DIR)" ]; then \
		echo "NFS_SMOKE_REPORT_DIR is required"; \
		exit 2; \
	fi
	@summary_args=""; \
	if [ -n "$(NFS_SMOKE_COMPARE_SUMMARY)" ]; then \
		summary_args="--summary-output $(NFS_SMOKE_COMPARE_SUMMARY)"; \
	fi; \
	$(PYTHON) scripts/verify-nfs-smoke-report.py compare-dir \
		"$(NFS_SMOKE_BASELINE_REPORT_DIR)" \
		"$(NFS_SMOKE_REPORT_DIR)" \
		--require-all-platforms \
		$$summary_args \
		$(NFS_SMOKE_COMPARE_ARGS)

nfs-smoke-report-verify-self-test:
	@$(PYTHON) scripts/verify-nfs-smoke-report.py self-test

nfs-smoke-script-check:
	@bash -n scripts/docker/run-mount-large-rustfs-smoke.sh
	@bash -n scripts/docker/run-mount-nfs-linux-smoke.sh
	@bash -n scripts/run-mount-nfs-macos-smoke.sh
	@$(PYTHON) scripts/check-nfs-smoke-scripts.py --self-test
	@if command -v "$(POWERSHELL)" >/dev/null 2>&1; then \
		"$(POWERSHELL)" -NoLogo -NoProfile -NonInteractive -Command '$$tokens = $$null; $$errors = $$null; [System.Management.Automation.Language.Parser]::ParseFile("scripts/run-mount-nfs-windows-smoke.ps1", [ref]$$tokens, [ref]$$errors) | Out-Null; if ($$errors.Count -gt 0) { $$errors | ForEach-Object { Write-Error $$_ }; exit 1 }'; \
	else \
		echo "$(POWERSHELL) not found; skipping Windows smoke PowerShell parse"; \
	fi

nfs-release-gate:
	@set -e; \
	if [ "$(NFS_RELEASE_REQUIRE_EVIDENCE)" = "1" ]; then \
		if [ "$(NFS_RELEASE_REQUIRE_EXPECTED_RUN_SUFFIX)" = "1" ] && [ -z "$(NFS_RELEASE_EXPECTED_RUN_SUFFIX)" ]; then \
			echo "NFS_RELEASE_EXPECTED_RUN_SUFFIX is required for release-grade NFS evidence"; \
			echo "Use the NFS Mount Evidence workflow run-attempt suffix: <github-run-id>-<attempt>."; \
			echo "Set NFS_RELEASE_REQUIRE_EVIDENCE=0 only for non-release smoke builds."; \
			exit 2; \
		fi; \
		if [ "$(NFS_RELEASE_REQUIRE_EXPECTED_GIT_COMMIT)" = "1" ] && [ -z "$(NFS_RELEASE_EXPECTED_GIT_COMMIT)" ]; then \
			echo "NFS_RELEASE_EXPECTED_GIT_COMMIT is required for release-grade NFS evidence"; \
			echo "Use the full release commit object id from git rev-parse HEAD."; \
			echo "Set NFS_RELEASE_REQUIRE_EVIDENCE=0 only for non-release smoke builds."; \
			exit 2; \
		fi; \
		expected_run_args=""; \
		expected_commit_args=""; \
		summary_args=""; \
		if [ -n "$(NFS_RELEASE_EXPECTED_RUN_SUFFIX)" ]; then \
			expected_run_args="--expected-run-suffix $(NFS_RELEASE_EXPECTED_RUN_SUFFIX)"; \
		fi; \
		if [ -n "$(NFS_RELEASE_EXPECTED_GIT_COMMIT)" ]; then \
			expected_commit_args="--expected-git-commit $(NFS_RELEASE_EXPECTED_GIT_COMMIT)"; \
		fi; \
		if [ -n "$(NFS_RELEASE_SUMMARY_OUTPUT)" ]; then \
			summary_args="--summary-output $(NFS_RELEASE_SUMMARY_OUTPUT)"; \
		fi; \
		$(PYTHON) scripts/verify-nfs-smoke-report.py verify-dir "$(NFS_RELEASE_EVIDENCE_DIR)" \
			--require-artifacts \
			--require-all-platforms \
			$$expected_run_args \
			$$expected_commit_args \
			$$summary_args \
			$(NFS_RELEASE_VERIFY_ARGS); \
	else \
		echo "warning: skipping NFS release evidence gate; this build is not release-grade" >&2; \
	fi

nfs-release-evidence-ci:
	@NFS_RELEASE_EVIDENCE_REF="$(NFS_RELEASE_EVIDENCE_REF)" \
	NFS_RELEASE_EVIDENCE_WAIT="$(NFS_RELEASE_EVIDENCE_WAIT)" \
	NFS_RELEASE_EVIDENCE_WAIT_TIMEOUT_SECS="$(NFS_RELEASE_EVIDENCE_WAIT_TIMEOUT_SECS)" \
	NFS_RELEASE_EVIDENCE_POLL_SECS="$(NFS_RELEASE_EVIDENCE_POLL_SECS)" \
	NFS_RELEASE_EVIDENCE_OUTPUT="$(NFS_RELEASE_EVIDENCE_OUTPUT)" \
	NFS_SMOKE_BASELINE_RUN_ID="$(NFS_SMOKE_BASELINE_RUN_ID)" \
	NFS_SMOKE_VERIFY_ARGS="$(NFS_SMOKE_VERIFY_ARGS)" \
	NFS_SMOKE_COMPARE_ARGS="$(NFS_SMOKE_COMPARE_ARGS)" \
	NFS_THRESHOLD_MIN_SMOKE_SUMMARIES="$(NFS_THRESHOLD_MIN_SMOKE_SUMMARIES)" \
	NFS_THRESHOLD_REQUIRE_CALIBRATION_READY="$(NFS_THRESHOLD_REQUIRE_CALIBRATION_READY)" \
	NFS_THRESHOLD_REQUIRE_RELEASE_GRADE="$(NFS_THRESHOLD_REQUIRE_RELEASE_GRADE)" \
		./scripts/release/dispatch-nfs-release-evidence.sh

nfs-release-evidence-dispatch-self-test:
	@bash scripts/release/dispatch-nfs-release-evidence.sh self-test

nfs-evidence-summary-self-test:
	@$(PYTHON) scripts/nfs-evidence-summary.py self-test

nfs-threshold-suggestions:
	@set -e; \
	benchmark_report="$(NFS_THRESHOLD_BENCHMARK_REPORT)"; \
	benchmark_reports="$(NFS_THRESHOLD_BENCHMARK_REPORTS)"; \
	benchmark_dir="$(NFS_THRESHOLD_BENCHMARK_DIR)"; \
	benchmark_dirs="$(NFS_THRESHOLD_BENCHMARK_DIRS)"; \
	smoke_summary="$(NFS_THRESHOLD_SMOKE_SUMMARY)"; \
	smoke_summaries="$(NFS_THRESHOLD_SMOKE_SUMMARIES)"; \
	smoke_dir="$(NFS_THRESHOLD_SMOKE_DIR)"; \
	smoke_dirs="$(NFS_THRESHOLD_SMOKE_DIRS)"; \
	if [ -z "$$benchmark_report" ] && [ -z "$$benchmark_reports" ] && [ -z "$$benchmark_dir" ] && [ -z "$$benchmark_dirs" ] && [ -f "$(NFS_READ_PATH_BENCH_REPORT)" ]; then \
		benchmark_report="$(NFS_READ_PATH_BENCH_REPORT)"; \
	fi; \
	if [ -z "$$smoke_summary" ] && [ -z "$$smoke_summaries" ] && [ -z "$$smoke_dir" ] && [ -z "$$smoke_dirs" ] && [ -n "$(NFS_SMOKE_VERIFY_SUMMARY)" ]; then \
		smoke_summary="$(NFS_SMOKE_VERIFY_SUMMARY)"; \
	fi; \
	if [ -z "$$benchmark_report" ] && [ -z "$$benchmark_reports" ] && [ -z "$$benchmark_dir" ] && [ -z "$$benchmark_dirs" ] && [ -z "$$smoke_summary" ] && [ -z "$$smoke_summaries" ] && [ -z "$$smoke_dir" ] && [ -z "$$smoke_dirs" ]; then \
		echo "Set NFS_THRESHOLD_BENCHMARK_REPORT(S)/DIR(S) and/or NFS_THRESHOLD_SMOKE_SUMMARY/SUMMARIES/DIR(S)"; \
		echo "Existing $(NFS_READ_PATH_BENCH_REPORT) is also used automatically."; \
		exit 2; \
	fi; \
	benchmark_args=""; \
	smoke_args=""; \
	output_args=""; \
	json_args=""; \
	tier_args=""; \
	if [ -n "$$benchmark_report" ]; then \
		benchmark_args="$$benchmark_args --benchmark-report $$benchmark_report"; \
	fi; \
	for report in $$benchmark_reports; do \
		benchmark_args="$$benchmark_args --benchmark-report $$report"; \
	done; \
	if [ -n "$$benchmark_dir" ]; then \
		benchmark_args="$$benchmark_args --benchmark-dir $$benchmark_dir"; \
	fi; \
	for directory in $$benchmark_dirs; do \
		benchmark_args="$$benchmark_args --benchmark-dir $$directory"; \
	done; \
	if [ -n "$$smoke_summary" ]; then \
		smoke_args="$$smoke_args --smoke-summary $$smoke_summary"; \
	fi; \
	for summary in $$smoke_summaries; do \
		smoke_args="$$smoke_args --smoke-summary $$summary"; \
	done; \
	if [ -n "$$smoke_dir" ]; then \
		smoke_args="$$smoke_args --smoke-dir $$smoke_dir"; \
	fi; \
	for directory in $$smoke_dirs; do \
		smoke_args="$$smoke_args --smoke-dir $$directory"; \
	done; \
	if [ -n "$(NFS_THRESHOLD_SUGGESTION_OUTPUT)" ]; then \
		output_args="--output $(NFS_THRESHOLD_SUGGESTION_OUTPUT)"; \
	fi; \
	if [ -n "$(NFS_THRESHOLD_SUGGESTION_JSON)" ]; then \
		json_args="--json-output $(NFS_THRESHOLD_SUGGESTION_JSON)"; \
	fi; \
	if [ "$(NFS_THRESHOLD_REQUIRE_RELEASE_GRADE)" = "1" ]; then \
		tier_args="$$tier_args --require-release-grade"; \
	fi; \
	if [ "$(NFS_THRESHOLD_REQUIRE_CALIBRATION_READY)" = "1" ]; then \
		tier_args="$$tier_args --require-calibration-ready"; \
	fi; \
	$(PYTHON) scripts/nfs-evidence-summary.py thresholds \
		$$benchmark_args \
		$$smoke_args \
		--benchmark-margin-pct "$(NFS_THRESHOLD_BENCHMARK_MARGIN_PCT)" \
		--smoke-margin-pct "$(NFS_THRESHOLD_SMOKE_MARGIN_PCT)" \
		--benchmark-regression-pct "$(NFS_THRESHOLD_BENCHMARK_REGRESSION_PCT)" \
		--smoke-regression-pct "$(NFS_THRESHOLD_SMOKE_REGRESSION_PCT)" \
		--min-benchmark-reports "$(NFS_THRESHOLD_MIN_BENCHMARK_REPORTS)" \
		--min-smoke-summaries "$(NFS_THRESHOLD_MIN_SMOKE_SUMMARIES)" \
		$$tier_args \
		$$output_args \
		$$json_args

nfs-feature-gate:
	@$(MAKE) --no-print-directory nfs-smoke-script-check
	@$(PYTHON) scripts/release/check-release-archive-contents.py
	@$(CARGO) check -p crab --bin crab --no-default-features --features nfs,gix-all
	@$(CARGO) test -p crab-vfs --features nfs,gix-facade
	@$(CARGO) test -p crab --test prop_coordinator response_contains --features fuse
	@$(PYTHON) scripts/verify-nfs-smoke-report.py self-test
	@$(PYTHON) scripts/nfs-read-path-bench-report.py self-test
	@$(PYTHON) scripts/nfs-evidence-summary.py self-test
	@bash scripts/release/dispatch-nfs-release-evidence.sh self-test

nfs-read-path-bench-report-verify:
	@if [ -z "$(NFS_READ_PATH_BENCH_REPORT)" ]; then \
		echo "NFS_READ_PATH_BENCH_REPORT is required"; \
		exit 2; \
	fi
	@expected_commit_args=""; \
	if [ -n "$(NFS_READ_PATH_BENCH_EXPECTED_GIT_COMMIT)" ]; then \
		expected_commit_args="--expected-git-commit $(NFS_READ_PATH_BENCH_EXPECTED_GIT_COMMIT)"; \
	fi; \
	expected_run_args=""; \
	if [ -n "$(NFS_READ_PATH_BENCH_EXPECTED_RUN_ID)" ]; then \
		expected_run_args="--expected-run-id $(NFS_READ_PATH_BENCH_EXPECTED_RUN_ID)"; \
	fi; \
	$(PYTHON) scripts/nfs-read-path-bench-report.py verify "$(NFS_READ_PATH_BENCH_REPORT)" \
		$$expected_commit_args \
		$$expected_run_args \
		$(NFS_READ_PATH_BENCH_VERIFY_ARGS)

nfs-read-path-bench-report-compare:
	@if [ -z "$(NFS_READ_PATH_BENCH_BASELINE_REPORT)" ]; then \
		echo "NFS_READ_PATH_BENCH_BASELINE_REPORT is required"; \
		exit 2; \
	fi
	@if [ -z "$(NFS_READ_PATH_BENCH_REPORT)" ]; then \
		echo "NFS_READ_PATH_BENCH_REPORT is required"; \
		exit 2; \
	fi
	@output_args=""; \
	if [ -n "$(NFS_READ_PATH_BENCH_COMPARE_OUTPUT)" ]; then \
		output_args="--output $(NFS_READ_PATH_BENCH_COMPARE_OUTPUT)"; \
	fi; \
	$(PYTHON) scripts/nfs-read-path-bench-report.py compare \
		"$(NFS_READ_PATH_BENCH_BASELINE_REPORT)" \
		"$(NFS_READ_PATH_BENCH_REPORT)" \
		$$output_args \
		$(NFS_READ_PATH_BENCH_COMPARE_ARGS)

nfs-read-path-bench-report-self-test:
	@$(PYTHON) scripts/nfs-read-path-bench-report.py self-test

mount-large-macos-rustfs-smoke:
	@./scripts/run-mount-large-macos-rustfs-smoke.sh

mount-large-macos-nfs-rustfs-smoke:
	@CRAB_MOUNT_MACOS_BACKEND=nfs \
	CRAB_MOUNT_MACOS_DIRECTORY_ENTRIES=10000 \
	./scripts/run-mount-large-macos-rustfs-smoke.sh

cache-service-validate-manifests:
	@./scripts/validate-cache-service-manifests.sh

cache-service-release-gate:
	@set -e; \
	if [ "$(CACHE_SERVICE_RELEASE_REQUIRE_EVIDENCE)" = "1" ]; then \
		if [ "$(CACHE_SERVICE_RELEASE_REQUIRE_EXPECTED_RUN_ID)" = "1" ] && [ -z "$(CACHE_SERVICE_RELEASE_EXPECTED_RUN_ID)" ]; then \
			echo "CACHE_SERVICE_RELEASE_EXPECTED_RUN_ID is required for local release archives"; \
			echo "Use the cache-service workflow run-attempt ID: gha-<github-run-id>-<attempt>."; \
			echo "Set CACHE_SERVICE_RELEASE_REQUIRE_EVIDENCE=0 only for non-release smoke builds."; \
			exit 2; \
		fi; \
		$(CARGO) build -p crab-cache-server --bin $(CACHE_SERVER_NAME); \
		"$(CACHE_SERVER_DEBUG_BIN)" evidence gate \
			--evidence-dir "$(CACHE_SERVICE_RELEASE_EVIDENCE_DIR)" \
			--expected-run-id "$(CACHE_SERVICE_RELEASE_EXPECTED_RUN_ID)" \
			--output "$(CACHE_SERVICE_RELEASE_VERIFY_OUTPUT)" \
			--summary-output "$(CACHE_SERVICE_RELEASE_SUMMARY_OUTPUT)" \
			--doctor-output "$(CACHE_SERVICE_RELEASE_DOCTOR_OUTPUT)" \
			--doctor-text-output "$(CACHE_SERVICE_RELEASE_DOCTOR_TEXT_OUTPUT)"; \
	else \
		echo "warning: skipping cache-service release evidence gate; this build is not release-grade" >&2; \
	fi

cache-service-rustfs-smoke:
	$(CARGO) build -p crab --bin $(BINARY_NAME)
	$(CARGO) build -p crab-cache-server --bin $(CACHE_SERVER_NAME)
	@AWS_ACCESS_KEY_ID="$(CACHE_SERVICE_RUSTFS_ACCESS_KEY)" \
	AWS_SECRET_ACCESS_KEY="$(CACHE_SERVICE_RUSTFS_SECRET_KEY)" \
	AWS_REGION="$(CACHE_SERVICE_RUSTFS_REGION)" \
	python3 scripts/e2e/run_cache_service_rustfs_smoke.py \
		--root "$(CACHE_SERVICE_RUSTFS_ROOT)" \
		--endpoint-url "$(CACHE_SERVICE_RUSTFS_ENDPOINT)" \
		--bucket "$(CACHE_SERVICE_RUSTFS_BUCKET)" \
		--crab-bin "$(DEBUG_BIN)" \
		--cache-server-bin "$(CACHE_SERVER_DEBUG_BIN)" \
		$(CACHE_SERVICE_RUSTFS_EXTRA_ARGS)

cache-service-onboarding-rustfs-smoke: cache-service-rustfs-smoke

cache-service-verify-smoke-report:
	@if [ -z "$(CACHE_SERVICE_RUSTFS_REPORT)" ]; then \
		echo "CACHE_SERVICE_RUSTFS_REPORT is required"; \
		exit 2; \
	fi
	@if [ -n "$(CACHE_SERVICE_RUSTFS_VERIFY_OUTPUT)" ]; then \
		python3 scripts/verify-cache-service-smoke-report.py "$(CACHE_SERVICE_RUSTFS_REPORT)" \
			--output "$(CACHE_SERVICE_RUSTFS_VERIFY_OUTPUT)"; \
	else \
		python3 scripts/verify-cache-service-smoke-report.py "$(CACHE_SERVICE_RUSTFS_REPORT)"; \
	fi

# --- Install ---

install: build
	@$(PYTHON) scripts/release/install-binaries.py \
		--prefix "$(PREFIX)" \
		--cargo-bin "$(CARGO_BIN)" \
		--crab-bin "$(RELEASE_BIN)" \
		--cache-server-bin "$(CACHE_SERVER_RELEASE_BIN)" \
		--binary-name "$(BINARY_NAME)" \
		--cache-server-name "$(CACHE_SERVER_NAME)" \
		--remote-link "$(REMOTE_LINK)" \
		$(NFS_MOUNT_INSTALL_ARGS) \
		$(FUSE_MOUNT_INSTALL_ARGS)
	@echo ""
	@echo "verify: $(BINARY_NAME) version"

uninstall:
	@rm -f $(PREFIX)/$(BINARY_NAME)
	@rm -f $(PREFIX)/$(CACHE_SERVER_NAME)
	@rm -f $(PREFIX)/$(FUSE_MOUNT_NAME)
	@rm -f $(PREFIX)/$(NFS_MOUNT_NAME)
	@rm -f $(PREFIX)/$(REMOTE_LINK)
	@echo "removed $(BINARY_NAME), $(CACHE_SERVER_NAME), $(FUSE_MOUNT_NAME), $(NFS_MOUNT_NAME), and $(REMOTE_LINK) from $(PREFIX)"
	@if [ "$(PREFIX)" != "$(CARGO_BIN)" ] && [ -d "$(CARGO_BIN)" ]; then \
		rm -f $(CARGO_BIN)/$(BINARY_NAME); \
		rm -f $(CARGO_BIN)/$(CACHE_SERVER_NAME); \
		rm -f $(CARGO_BIN)/$(FUSE_MOUNT_NAME); \
		rm -f $(CARGO_BIN)/$(NFS_MOUNT_NAME); \
		rm -f $(CARGO_BIN)/$(REMOTE_LINK); \
		echo "removed $(BINARY_NAME), $(CACHE_SERVER_NAME), $(FUSE_MOUNT_NAME), $(NFS_MOUNT_NAME), and $(REMOTE_LINK) from $(CARGO_BIN)"; \
	fi

# --- Version bump ---

bump-patch:
	@./scripts/release/bump-version.sh patch

bump-minor:
	@./scripts/release/bump-version.sh minor

bump-major:
	@./scripts/release/bump-version.sh major

bump-set:
	@./scripts/release/bump-version.sh set $(VERSION)

# Bump + install in one shot:
#   make release-patch
#   make release-minor
#   make release-major
release-patch: bump-patch install
release-minor: bump-minor install
release-major: bump-major install

# --- Release (publish to GitHub) ---

release: release-ci

release-build:
	@./scripts/release/release.sh --allow-partial

release-target:
	@if [ -z "$(TARGET)" ]; then \
		echo "usage: make release-target TARGET=linux-x86_64"; \
		./scripts/release/release.sh --list-targets; \
		exit 2; \
	fi
	@./scripts/release/release.sh --target "$(TARGET)"

release-list:
	@./scripts/release/release.sh --list-targets

release-metadata-check:
	@set -eu; \
	version=$$(grep -m1 '^version' Cargo.toml | cut -d '"' -f2); \
	for manifest in Cargo.toml ../crates/crab-auth-server/Cargo.toml ../crates/crab-cache-server/Cargo.toml; do \
		manifest_version=$$(grep -m1 '^version' "$$manifest" | cut -d '"' -f2); \
		if [ "$$manifest_version" != "$$version" ]; then \
			echo "$$manifest has $$manifest_version, expected $$version" >&2; \
			exit 1; \
		fi; \
	done; \
	for package in crab crab-auth-server crab-cache-server; do \
		lock_version=$$(awk -v package="$$package" '$$0 == "name = \"" package "\"" { found = 1; next } found && /^version = / { gsub(/^version = \"|\"$$/, ""); print; exit }' ../Cargo.lock); \
		if [ "$$lock_version" != "$$version" ]; then \
			echo "Cargo.lock has $$package $$lock_version, expected $$version" >&2; \
			exit 1; \
		fi; \
	done; \
	if ! grep -Eq "^## $$version - [0-9]{4}-[0-9]{2}-[0-9]{2}$$" ../CHANGELOG.md; then \
		echo "CHANGELOG.md has no dated $$version release section" >&2; \
		exit 1; \
	fi; \
	echo "release metadata is consistent for v$$version"

release-ci: release-metadata-check
	@if ! command -v gh >/dev/null 2>&1; then \
		echo "gh CLI is required for make release-ci"; \
		exit 127; \
	fi
	@if [ "$(RELEASE_REQUIRE_ENTERPRISE_EVIDENCE)" = "1" ] && [ -z "$(REPLICA_RELEASE_EVIDENCE_RUN_ID)" ]; then \
		echo "REPLICA_RELEASE_EVIDENCE_RUN_ID is required for make release-ci"; \
		echo "Run the Replica Live Evidence workflow with suite=enterprise, then pass its GitHub run ID."; \
		exit 2; \
	fi
	@if [ "$(RELEASE_REQUIRE_ENTERPRISE_EVIDENCE)" = "1" ] && [ -z "$(CACHE_SERVICE_RELEASE_EVIDENCE_RUN_ID)" ]; then \
		echo "CACHE_SERVICE_RELEASE_EVIDENCE_RUN_ID is required for make release-ci"; \
		echo "Run the Cache Service Tests workflow on the exact release commit, then pass its GitHub run ID."; \
		exit 2; \
	fi
	@if [ "$(RELEASE_REQUIRE_NFS_EVIDENCE)" = "1" ] && [ -z "$(NFS_RELEASE_EVIDENCE_RUN_ID)" ]; then \
		echo "NFS_RELEASE_EVIDENCE_RUN_ID is required for make release-ci"; \
		echo "Run the NFS Mount Evidence workflow on the exact release commit, then pass its GitHub run ID."; \
		exit 2; \
	fi
	@TAG=v$$(grep -m1 '^version' Cargo.toml | cut -d '"' -f2); \
	echo "dispatching GitHub release workflow for $$TAG"; \
	require_enterprise_evidence=false; \
	require_nfs_evidence=false; \
	require_cloud_evidence=false; \
	if [ "$(RELEASE_REQUIRE_ENTERPRISE_EVIDENCE)" = "1" ]; then \
		require_enterprise_evidence=true; \
	fi; \
	if [ "$(RELEASE_REQUIRE_NFS_EVIDENCE)" = "1" ]; then \
		require_nfs_evidence=true; \
	fi; \
	if [ "$(RELEASE_REQUIRE_CLOUD_EVIDENCE)" = "1" ]; then \
		require_cloud_evidence=true; \
	fi; \
		set -- \
			-f "tag=$$TAG" \
			-f "require_enterprise_evidence=$$require_enterprise_evidence" \
			-f "require_nfs_evidence=$$require_nfs_evidence" \
			-f "require_cloud_evidence=$$require_cloud_evidence"; \
		if [ -n "$(REPLICA_RELEASE_EVIDENCE_RUN_ID)" ]; then \
			set -- "$$@" -f "replica_evidence_run_id=$(REPLICA_RELEASE_EVIDENCE_RUN_ID)"; \
		fi; \
		if [ -n "$(CACHE_SERVICE_RELEASE_EVIDENCE_RUN_ID)" ]; then \
			set -- "$$@" -f "cache_service_evidence_run_id=$(CACHE_SERVICE_RELEASE_EVIDENCE_RUN_ID)"; \
		fi; \
		if [ -n "$(NFS_RELEASE_EVIDENCE_RUN_ID)" ]; then \
			set -- "$$@" -f "nfs_evidence_run_id=$(NFS_RELEASE_EVIDENCE_RUN_ID)"; \
		fi; \
		if [ -n "$(REPLICA_RELEASE_EVIDENCE_ARTIFACT)" ]; then \
			set -- "$$@" -f "replica_evidence_artifact=$(REPLICA_RELEASE_EVIDENCE_ARTIFACT)"; \
		fi; \
		if [ -n "$(CACHE_SERVICE_RELEASE_EVIDENCE_ARTIFACT)" ]; then \
			set -- "$$@" -f "cache_service_evidence_artifact=$(CACHE_SERVICE_RELEASE_EVIDENCE_ARTIFACT)"; \
		fi; \
		if [ -n "$(NFS_RELEASE_VERIFY_ARGS)" ]; then \
			set -- "$$@" -f "nfs_release_verify_args=$(NFS_RELEASE_VERIFY_ARGS)"; \
		fi; \
		gh workflow run release.yml "$$@"

# --- Homebrew tap ---

homebrew:
	@./scripts/release/update-homebrew.sh

homebrew-local:
	@./scripts/release/update-homebrew.sh --local

homebrew-dry:
	@./scripts/release/update-homebrew.sh --dry-run

# Full release pipeline: build + publish + update Homebrew tap
release-full: release-ci

# --- Clean ---

clean:
	$(CARGO) clean
