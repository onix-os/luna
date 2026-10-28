SHELL := /bin/bash

# Parsed out of Cargo.toml rather than kept in a second place that can disagree with it. `name` is
# unique to [package]; `version` is the first bare assignment, which is [workspace.package].
PROJECT_NAME := $(shell sed -n 's/^name = "\(.*\)"/\1/p' Cargo.toml | head -1)
PROJECT_VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ifeq ($(PROJECT_NAME),)
    $(error Error: could not parse a package name out of Cargo.toml)
endif

TOP_DIR := $(CURDIR)
CARGO := cargo
EXAMPLE ?= interpreter
JIT_MODE ?= off
TARGET ?=
TARGET_ARG := $(if $(TARGET),--target $(TARGET),)
ACTIONLINT ?= actionlint
FUZZ_TARGET ?= all
FUZZ_CASES ?= 256
FUZZ_SEEDS ?= 0,1,0xdeadbeef,0xffffffffffffffff
CG_TARGET ?= scalar
CG_RUNS ?= 256
CG_SECONDS ?= 60
CG_DIR ?= target/jit-evidence/coverage-guided
CG_INPUT ?=
JIT_BENCH_OPT ?= 3
JIT_BENCH_BINARY ?= $(TOP_DIR)/target/$(if $(TARGET),$(TARGET)/,)release/examples/jit_bench
JIT_ASSEMBLY_BINARY ?= target/release/examples/jit_bench
JIT_ASSEMBLY_DIR ?= target/jit-evidence
JIT_METRICS_BINARY ?= $(TOP_DIR)/target/$(if $(TARGET),$(TARGET)/,)release/examples/jit_metrics
JIT_DUMP_DIR ?= target/jit-evidence/native/$(if $(TARGET),$(TARGET),host)
SIZE_PROFILE ?= shipping
COST_SAMPLES ?= 11
COST_ITERATIONS ?= 20
COST_STRIP ?= true
PROFILE_CASE ?= float_loop
PROFILE_MODE ?= off
PROFILE_CPU ?= $(shell awk '/^Cpus_allowed_list:/{split($$2, cpus, /[-,]/); print cpus[1]}' /proc/self/status)
BENCH_PROFILE_DIR ?= target/jit-evidence/bench-profile/$(PROFILE_CASE)/$(PROFILE_MODE)
PAIRED_PROFILE_DIR ?= target/jit-evidence/paired-profile
COST_DIR := target/jit-evidence/feature-cost/$(SIZE_PROFILE)$(if $(TARGET),-$(TARGET),)$(if $(filter false,$(COST_STRIP)),-symbols,)
COST_PROFILE_DIR := $(COST_DIR)/callgrind/$(PROFILE_CASE)/$(PROFILE_MODE)
COST_RELEASE_DIR := target/$(if $(TARGET),$(TARGET)/,)release/examples
COST_OPT := $(if $(filter shipping,$(SIZE_PROFILE)),s,3)
MIRI_TARGET ?= x86_64-unknown-linux-gnu
MIRI_DIR := target/jit-evidence/miri/$(MIRI_TARGET)
REGION_PROFILE_DIR ?= target/jit-evidence/short-slice-performance/native-region-profile
SPLIT_SCRATCH_DIR ?= target/jit-evidence/short-slice-performance/split-scratch-profile

HAS_REL := $(shell command -v git-rel 2>/dev/null)

$(info ------------------------------------------)
$(info Project: $(PROJECT_NAME) v$(PROJECT_VERSION))
$(info ------------------------------------------)

.PHONY: build b dev compile c run r repl test t test-all test-doc check check-all clippy clippy-strict rustdoc fmt fmt-check print-name clean verify publish release help h
.PHONY: jit-deps jit-check jit-test jit-test-all jit-test-doc jit-rustdoc jit-verify jit-tree jit-reference jit-backend jit-boundary jit-native jit-numeric jit-heap jit-registers jit-policy jit-resources jit-example jit-bench jit-bench-paired jit-performance jit-profile jit-profile-build jit-rust-assembly jit-fuzz-smoke jit-fuzz jit-evidence environment
.PHONY: ci-check jit-platform
.PHONY: jit-size jit-size-build jit-cost-tests jit-cost-native jit-shipping
.PHONY: jit-size-run jit-cost-native-run
.PHONY: jit-cost-profile
.PHONY: jit-disassembly
.PHONY: jit-bench-build jit-bench-run
.PHONY: jit-metrics jit-metrics-build jit-metrics-run jit-metrics-tests
.PHONY: jit-miri jit-miri-wrapper-tests jit-relocations jit-errors
.PHONY: jit-helpers jit-abi jit-config jit-registry jit-suspension jit-ir jit-generic-for jit-exits jit-access
.PHONY: jit-tags
.PHONY: jit-input jit-float-input jit-arithmetic jit-truth jit-comparison jit-comparison-backend
.PHONY: jit-loop-source jit-loop-source-backend
.PHONY: jit-transfer-source jit-transfer-source-backend
.PHONY: jit-helper-flow jit-helper-flow-backend jit-helper-flow-miri
.PHONY: jit-exit-flow jit-exit-flow-backend
.PHONY: jit-entry-flow jit-entry-flow-backend
.PHONY: jit-region-flow jit-region-flow-backend
.PHONY: jit-source-binding jit-source-binding-backend
.PHONY: jit-fuzz-heap
.PHONY: jit-heap-campaign-tests
.PHONY: jit-predecessors jit-predecessors-backend
.PHONY: jit-dominance jit-dominance-backend
.PHONY: jit-host-memory jit-host-ledger jit-host-native jit-host-finish
.PHONY: jit-host-finish-async jit-host-reference
.PHONY: jit-owner jit-owner-installation jit-owner-miri
.PHONY: jit-memory-status jit-memory-status-pure jit-memory-status-native jit-memory-status-miri
.PHONY: jit-atomic-owner-ordering-miri
.PHONY: jit-memory-status-boundary
.PHONY: jit-provider-box jit-provider-box-pure jit-provider-box-boundary jit-provider-box-native jit-provider-box-miri
.PHONY: jit-provider-box-admission
.PHONY: jit-segments jit-segments-pure jit-segments-native jit-segments-miri
.PHONY: jit-segments-reclamation
.PHONY: jit-mapping-counter jit-mapping-counter-pure jit-mapping-counter-native jit-mapping-counter-miri
.PHONY: jit-ledger-owner jit-ledger-owner-pure jit-ledger-owner-miri
.PHONY: jit-runtime-owner jit-runtime-owner-pure jit-runtime-owner-miri
.PHONY: jit-frontend-arrays jit-frontend-arrays-miri
.PHONY: jit-handoff jit-handoff-miri jit-image
.PHONY: jit-image-retention
.PHONY: jit-upvalue-current-frame
.PHONY: jit-helpers-miri
.PHONY: jit-coverage-environment jit-coverage-lock jit-coverage-check jit-coverage-test
.PHONY: jit-coverage-help jit-coverage-fmt jit-coverage-fmt-check jit-coverage-build jit-coverage-run
.PHONY: jit-coverage-replay jit-coverage-wrapper-tests
.PHONY: jit-clippy table-access table-access-miri

.PHONY: table-capacity table-capacity-miri
table-capacity:
	@$(CARGO) test --locked -p luna --lib $(TARGET_ARG) table::raw::capacity_tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) table::raw::capacity_tests

table-capacity-miri:
	@$(CARGO) miri test --locked -p luna --lib --target '$(MIRI_TARGET)' table::raw::capacity_tests -- --test-threads=1

.PHONY: jit-array-window jit-array-window-miri
.PHONY: jit-unlinked-identity
.PHONY: jit-compiler-wire jit-compiler-wire-miri jit-compiler-process
.PHONY: jit-compiler-artifact jit-compiler-artifact-parent jit-compiler-artifact-wire-miri
.PHONY: jit-compiler-code-owner
jit-compiler-code-owner:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::artifact::assemble::tests -- --test-threads=1 | awk '/^test result: ok[.] 4 passed; 0 failed; 0 ignored;/ { valid=1 } { print } END { exit !valid }'
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) worker_artifact_ -- --test-threads=1 | awk '/^test result: ok[.] 12 passed; 0 failed; 0 ignored;/ { valid=1 } { print } END { exit !valid }'

jit-compiler-artifact:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::artifact:: -- --test-threads=1 | awk '/^test result: ok[.] 18 passed; 0 failed; 0 ignored;/ { valid=1 } { print } END { exit !valid }'

jit-compiler-artifact-wire-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::artifact::wire::tests -- --test-threads=1 | awk '/^test result: ok[.] 5 passed; 0 failed; 0 ignored;/ { valid=1 } { print } END { exit !valid }'

jit-compiler-artifact-parent:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::compiler_process::tests::child_generated_artifacts_run_all_production_entries_in_the_parent -- --exact --test-threads=1 --nocapture | awk '/^test result: ok[.] 1 passed; 0 failed; 0 ignored;/ { valid=1 } { print } END { exit !valid }'

jit-compiler-process:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::compiler_process -- --test-threads=1 | awk '/^test result: ok[.] 11 passed; 0 failed; 0 ignored;/ { valid=1 } { print } END { exit !valid }'

jit-compiler-wire:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::compiler_wire | awk '/^test result: ok[.] 8 passed; 0 failed; 0 ignored;/ { valid=1 } { print } END { exit !valid }'

jit-compiler-wire-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::compiler_wire -- --test-threads=1 | awk '/^test result: ok[.] 8 passed; 0 failed; 0 ignored;/ { valid=1 } { print } END { exit !valid }'

jit-unlinked-identity:
	@case "$$(uname -sm)" in 'Linux x86_64'|'Linux aarch64') ;; *) echo 'Unlinked identity gate requires supported hardware'; exit 2;; esac
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::unlinked:: -- --test-threads=1 | awk '/^test result: ok[.] 1 passed; 0 failed; 0 ignored;/ { valid=1 } { print } END { exit !valid }'
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::model::tests::effect_cached_getter_free_loops_keep_original_kernels -- --exact --test-threads=1 | awk '/^test result: ok[.] 1 passed; 0 failed; 0 ignored;/ { valid=1 } { print } END { exit !valid }'

.PHONY: jit-array-write-roots
jit-array-write-roots:
	@case "$$(uname -sm)" in 'Linux x86_64'|'Linux aarch64') ;; *) echo 'Native array roots gate requires supported hardware'; exit 2;; esac
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::array_window::kernel::tests::reference_roots::native_writes_preserve_reference_cells_at_every_pc_and_budget -- --exact --test-threads=1 --nocapture | awk '/^test result: ok[.] 1 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

.PHONY: jit-code-index jit-code-index-miri
jit-code-index:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::code_index:: $(ARGS)

jit-code-index-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::code_index:: -- --test-threads=1

jit-array-window:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::array_window:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::array::tests $(ARGS)

jit-array-window-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::array_window:: -- --test-threads=1

table-access:
	@$(CARGO) test --locked -p luna --lib $(TARGET_ARG) table::raw::access_tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) table::raw::access_tests

table-access-miri:
	@$(CARGO) miri test --locked -p luna --lib --target '$(MIRI_TARGET)' table::raw::access_tests -- --test-threads=1

.PHONY: jit-prehash-model jit-prehash-model-miri jit-prehash-profile-build jit-prehash-profile-run
PREHASH_DIR ?= target/jit-evidence/short-slice-performance/prehashed-lookup
jit-prehash-model:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) table::raw::prehashed_tests $(ARGS) | awk '/^test result: ok[.] 6 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-prehash-model-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' table::raw::prehashed_tests -- --test-threads=1 | awk '/^test result: ok[.] 6 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-prehash-profile-build:
	@mkdir -p '$(PREHASH_DIR)'
	@CARGO_PROFILE_RELEASE_OPT_LEVEL=$(JIT_BENCH_OPT) CARGO_PROFILE_RELEASE_STRIP=false $(CARGO) test --locked --release -p luna --features jit --lib $(TARGET_ARG) --no-run --message-format=json-render-diagnostics > '$(PREHASH_DIR)/build.jsonl'
	@jq -r 'select(.reason == "compiler-artifact" and .target.name == "luna" and .profile.test and .executable != null) | .executable' '$(PREHASH_DIR)/build.jsonl' > '$(PREHASH_DIR)/binary-path'
	@test "$$(wc -l < '$(PREHASH_DIR)/binary-path')" -eq 1
	@cp "$$(cat '$(PREHASH_DIR)/binary-path')" '$(PREHASH_DIR)/test-binary'
	@sha256sum '$(PREHASH_DIR)/test-binary' > '$(PREHASH_DIR)/binary.sha256'
	@{ find src -type f -name '*.rs' -print0; printf '%s\0' Cargo.toml Cargo.lock Makefile; } | sort -z | xargs -0 sha256sum > '$(PREHASH_DIR)/source.sha256'
	@git diff --binary > '$(PREHASH_DIR)/source.patch'
	@printf 'opt_level=%s\nreads=100000\ncollection=*prehashed_tests::profile_reads*\n' '$(JIT_BENCH_OPT)' > '$(PREHASH_DIR)/configuration'

jit-prehash-profile-run: jit-profile-cpu-check
	@sha256sum -c '$(PREHASH_DIR)/binary.sha256' '$(PREHASH_DIR)/source.sha256' > '$(PREHASH_DIR)/verification.log'
	@valgrind --version > '$(PREHASH_DIR)/profiler.log'
	@set -e; for separate in 0 1; do for mode in 0 1 2 3; do \
		LUNA_PREHASH_MODE=$$mode LUNA_PREHASH_SEPARATE=$$separate taskset -c '$(PROFILE_CPU)' valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*prehashed_tests::profile_reads*' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file='$(PREHASH_DIR)'/$$separate-$$mode.callgrind \
			'$(PREHASH_DIR)/test-binary' table::raw::prehashed_tests::prepared_lookup_profile --ignored --exact --nocapture --test-threads=1 > '$(PREHASH_DIR)'/$$separate-$$mode.log 2>&1; \
		grep -Fq "mode=$$mode separate=$$([ $$separate = 1 ] && echo true || echo false) reads=100000 sum=1700000" '$(PREHASH_DIR)'/$$separate-$$mode.log; \
		grep -Fq 'test result: ok. 1 passed; 0 failed' '$(PREHASH_DIR)'/$$separate-$$mode.log; \
		grep -Eq '^summary: [1-9][0-9]*' '$(PREHASH_DIR)'/$$separate-$$mode.callgrind; \
		callgrind_annotate --inclusive=no --threshold=99 '$(PREHASH_DIR)'/$$separate-$$mode.callgrind > '$(PREHASH_DIR)'/$$separate-$$mode-annotation.log; \
	done; done
	@sha256sum -c '$(PREHASH_DIR)/binary.sha256' '$(PREHASH_DIR)/source.sha256' > '$(PREHASH_DIR)/verification-after.log'

.PHONY: jit-prepared-keys jit-prepared-keys-miri
.PHONY: jit-inline-loop-plan jit-inline-loop-plan-miri jit-inline-loop-native jit-inline-loop-bridge jit-inline-loop-owner
jit-inline-loop-owner:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::inline_loop::tests | awk '/^test result: ok[.] 4 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-inline-loop-plan:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::inline_loop::tests | awk '/^test result: ok[.] 4 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-inline-loop-plan-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::inline_loop::tests -- --test-threads=1 | awk '/^test result: ok[.] 4 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-inline-loop-native:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::inline_loop::native::tests | awk '/^test result: ok[.] 5 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-inline-loop-bridge:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::inline_loop::native::bridge::tests | awk '/^test result: ok[.] 4 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

.PHONY: jit-hashless-read jit-hashless-read-miri
.PHONY: jit-canonical-compiler-resources
jit-canonical-compiler-resources:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helpers::hashless_tests::native_canonical_reads_reduce_real_workload_snapshot_peaks -- --exact --nocapture | awk '/^test result: ok[.] 1 passed; 0 failed;/ {valid=1} {print} END {exit !valid}'

.PHONY: jit-canonical-workspace jit-canonical-workspace-miri
jit-canonical-workspace:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::tags::workspace_accounting::canonical_read_constant_reduction_bounds_verifier_workspace -- --exact | awk '/^test result: ok[.] 1 passed; 0 failed;/ {valid=1} {print} END {exit !valid}'

jit-canonical-workspace-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::tags::workspace_accounting::canonical_read_constant_reduction_bounds_verifier_workspace -- --exact --test-threads=1 | awk '/^test result: ok[.] 1 passed; 0 failed;/ {valid=1} {print} END {exit !valid}'

jit-hashless-read:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helpers::hashless_tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) table::raw::code_read_hint_tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::entry_bytes::tests

jit-hashless-read-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::helpers::hashless_tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' table::raw::code_read_hint_tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::entry_bytes::tests -- --test-threads=1

jit-prepared-keys:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::prepared_keys::tests | awk '/^test result: ok[.] 9 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-prepared-keys-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::prepared_keys::tests -- --test-threads=1 | awk '/^test result: ok[.] 5 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

.PHONY: jit-import-model jit-import-model-miri jit-import-profile-build jit-import-profile-run
IMPORT_DIR ?= target/jit-evidence/short-slice-performance/scheduled-import-replay
jit-import-model:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::scheduled_import_tests $(ARGS) | awk '/^test result: ok[.] 3 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-import-model-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::scheduled_import_tests -- --test-threads=1 | awk '/^test result: ok[.] 2 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-import-profile-build:
	@mkdir -p '$(IMPORT_DIR)'
	@CARGO_PROFILE_RELEASE_OPT_LEVEL=$(JIT_BENCH_OPT) CARGO_PROFILE_RELEASE_STRIP=false $(CARGO) test --locked --release -p luna --features jit --lib $(TARGET_ARG) --no-run --message-format=json-render-diagnostics > '$(IMPORT_DIR)/build.jsonl'
	@jq -er 'select(.reason == "compiler-artifact" and .target.name == "luna" and .profile.test and .executable != null) | .executable' '$(IMPORT_DIR)/build.jsonl' > '$(IMPORT_DIR)/binary-path'
	@test "$$(wc -l < '$(IMPORT_DIR)/binary-path')" -eq 1
	@cp "$$(cat '$(IMPORT_DIR)/binary-path')" '$(IMPORT_DIR)/test-binary'
	@sha256sum '$(IMPORT_DIR)/test-binary' > '$(IMPORT_DIR)/binary.sha256'
	@{ find src -type f -name '*.rs' -print0; printf '%s\0' Cargo.toml Cargo.lock Makefile; } | sort -z | xargs -0 sha256sum > '$(IMPORT_DIR)/source.sha256'
	@git diff --binary > '$(IMPORT_DIR)/source.patch'
	@printf 'opt_level=%s\nentries=100000\ncollection=*scheduled_import_tests::native::profile_entry*\n' '$(JIT_BENCH_OPT)' > '$(IMPORT_DIR)/configuration'

jit-import-profile-run: jit-profile-cpu-check
	@sha256sum -c '$(IMPORT_DIR)/binary.sha256' '$(IMPORT_DIR)/source.sha256' > '$(IMPORT_DIR)/verification.log'
	@valgrind --version > '$(IMPORT_DIR)/profiler.log'
	@set -e; for width in 1 3 5 7; do for mode in full scheduled bulk; do \
		case $$mode in full) number=0;; scheduled) number=1;; bulk) number=2;; esac; \
		LUNA_IMPORT_WIDTH=$$width LUNA_IMPORT_MODE=$$mode taskset -c '$(PROFILE_CPU)' valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*scheduled_import_tests::native::profile_entry*' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file='$(IMPORT_DIR)'/$$width-$$mode.callgrind \
			'$(IMPORT_DIR)/test-binary' jit::scheduled_import_tests::native::scheduled_import_profile --ignored --exact --nocapture --test-threads=1 > '$(IMPORT_DIR)'/$$width-$$mode.log 2>&1; \
		grep -Fq "mode=$$number width=$$width entries=100000 verified=1" '$(IMPORT_DIR)'/$$width-$$mode.log; \
		grep -Fq 'test result: ok. 1 passed; 0 failed; 0 ignored;' '$(IMPORT_DIR)'/$$width-$$mode.log; \
		grep -Eq '^summary: [1-9][0-9]*' '$(IMPORT_DIR)'/$$width-$$mode.callgrind; \
	done; done
	@sha256sum -c '$(IMPORT_DIR)/binary.sha256' '$(IMPORT_DIR)/source.sha256' > '$(IMPORT_DIR)/verification-after.log'

.PHONY: metamethod-keys metamethod-keys-miri
metamethod-keys:
	@$(CARGO) test --locked -p luna --no-default-features --test metamethod_keys $(TARGET_ARG)
	@$(CARGO) test --locked -p luna --features jit --test metamethod_keys $(TARGET_ARG)

metamethod-keys-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --test metamethod_keys --target '$(MIRI_TARGET)' -- --test-threads=1

ci-check:
	@$(ACTIONLINT) .github/workflows/tests.yml

jit-miri-wrapper-tests:
	@bash tests/test-jit-miri.sh

jit-miri:
	@mkdir -p '$(MIRI_DIR)'
	@set -o pipefail; { rustc -vV; $(CARGO) miri --version; printf 'MIRIFLAGS=%s\ntarget=%s\n' "$${MIRIFLAGS:-}" '$(MIRI_TARGET)'; } 2>&1 | tee '$(MIRI_DIR)/environment.log'
	@set -o pipefail; $(CARGO) miri setup --target '$(MIRI_TARGET)' 2>&1 | tee '$(MIRI_DIR)/setup.log'
	@set -o pipefail; $(MAKE) --no-print-directory jit-compiler-wire-miri 2>&1 | tee '$(MIRI_DIR)/jit-compiler_wire.log'
	@set -o pipefail; $(MAKE) --no-print-directory jit-compiler-artifact-wire-miri 2>&1 | tee '$(MIRI_DIR)/jit-backend-artifact-wire-tests.log'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::resources::tests -- --test-threads=1 --skip jit::resources::tests::installation_refusal_reclaims_generated_mappings 2>&1 | tee '$(MIRI_DIR)/jit-resources-tests.log'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::memory_tests::block_map_quota_refuses_before_host_setup_and_releases_storage -- --exact --test-threads=1 2>&1 | tee '$(MIRI_DIR)/block-map-quota.log'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::memory_tests::entry_path_quota_refuses_before_host_setup_and_releases_storage -- --exact --test-threads=1 2>&1 | tee '$(MIRI_DIR)/entry-path-quota.log'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::memory_tests::status_refusal_precedes_host_setup_and_releases_entry_storage -- --exact --test-threads=1 2>&1 | tee '$(MIRI_DIR)/memory-status-quota.log'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::memory_tests::provider_box_refusal_precedes_host_setup_and_releases_entry_storage -- --exact --test-threads=1 2>&1 | tee '$(MIRI_DIR)/provider-box-quota.log'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::memory_tests::segment_record_underlying_refusal_precedes_mapping -- --exact --test-threads=1 2>&1 | tee '$(MIRI_DIR)/segment-record-quota.log'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::specialized::tests -- --test-threads=1 2>&1 | tee '$(MIRI_DIR)/jit-backend-specialized-tests.log'
	@set -e -o pipefail; for filter in jit::handoff::tests jit::arrays::tests jit::array_window:: jit::runtime_owner_tests jit::global_owner::tests jit::resources::bootstrap_tests jit::resources::mapping_tests jit::abi::tests jit::abi::transport_inputs::tests jit::entry_admission::tests jit::readonly_lookup::tests jit::readonly_lookup::scalar_entry jit::readonly_lookup::binding::tests jit::readonly_lookup::binding::integer_resume::tests table::raw::code_read_hint_tests::strong_peek jit::helpers::tests jit::helpers::existing_entry_tests jit::scoped_helpers::tests jit::registry::tests jit::ir::tests jit::flow::tests jit::work::tests jit::preds::tests jit::dominance::tests jit::owner::tests jit::atomic_owner::tests jit::memory_status::tests jit::global_box::tests jit::segments::tests jit::tags::tests jit::shape::tests jit::backend::comparison_tests jit::backend::loop_tests jit::backend::transfer_tests jit::backend::helper_flow_tests jit::backend::ownership_tests jit::access::tests jit::backend::access_tests jit::entry_flow::tests jit::entry_flow::region_tests jit::exit_flow::tests jit::exits::tests jit::backend::exit_tests jit::policy_tests finalizers::tests lua::memory_tests; do \
		$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' "$$filter" -- --test-threads=1 2>&1 | tee '$(MIRI_DIR)'/"$${filter//::/-}".log; \
	done

jit-platform:
	@case '$(TARGET)' in \
		x86_64-unknown-linux-gnu|x86_64-unknown-linux-musl) arch=x86_64 ;; \
		aarch64-unknown-linux-gnu) arch=aarch64 ;; \
		*) echo 'Set TARGET to a declared native release platform' >&2; exit 2 ;; \
	esac; \
	if test "$$(uname -sm)" != "Linux $$arch"; then \
		echo "Native gate requires Linux $$arch hardware, not a cross-build" >&2; exit 2; \
	fi
	@$(MAKE) --no-print-directory environment jit-verify jit-example jit-disassembly

jit-evidence:
	@mkdir -p target/jit-evidence
	@for log in /tmp/luna-jit-*.log; do \
		if test -f "$$log"; then cp "$$log" target/jit-evidence/; fi; \
	done
	@$(MAKE) --no-print-directory environment > target/jit-evidence/environment.log

jit-bench-build:
	@mkdir -p target/jit-evidence
	@CARGO_TARGET_DIR='$(TOP_DIR)/target' $(MAKE) --no-print-directory environment > target/jit-evidence/bench-environment.log
	@set -o pipefail; CARGO_TARGET_DIR='$(TOP_DIR)/target' LUNA_BENCH_OPT_LEVEL=$(JIT_BENCH_OPT) CARGO_PROFILE_RELEASE_OPT_LEVEL=$(JIT_BENCH_OPT) $(CARGO) build --release --example jit_bench --features jit $(TARGET_ARG) 2>&1 | tee target/jit-evidence/bench-build.log

jit-bench-run:
	@mkdir -p target/jit-evidence
	@test -x '$(JIT_BENCH_BINARY)'
	@set -o pipefail; '$(JIT_BENCH_BINARY)' $(ARGS) 2>&1 | tee target/jit-evidence/bench.log

jit-bench: jit-bench-build
	@$(MAKE) --no-print-directory jit-bench-run

.PHONY: jit-pgo-pilot jit-pgo-script-check jit-pgo-coverage-tests
jit-pgo-coverage-tests:
	@bash tests/test-jit-pgo-coverage.sh

jit-pgo-script-check: jit-pgo-coverage-tests
	@bash -n examples/jit_support/pgo_pilot.sh
	@bash -n tests/test-jit-pgo-coverage.sh

jit-pgo-pilot: jit-pgo-script-check
	@bash examples/jit_support/pgo_pilot.sh '$(PGO_DIR)'

.PHONY: jit-bench-paired-profile-run jit-bench-paired-profile-verify jit-bench-paired-profile-annotate jit-bench-paired-profile-tests
jit-bench-paired-profile-run:
	@bash examples/jit_support/paired_profile.sh '$(JIT_BENCH_BINARY)' '$(PAIRED_PROFILE_DIR)'

jit-bench-paired-profile-verify:
	@bash examples/jit_support/verify_paired_profile.sh '$(PAIRED_PROFILE_DIR)'

jit-bench-paired-profile-annotate:
	@bash examples/jit_support/annotate_paired_profile.sh '$(PAIRED_PROFILE_DIR)'

jit-bench-paired-profile-tests:
	@bash tests/test-jit-paired-profile.sh

.PHONY: jit-bench-profile-run jit-profile-cpu-check jit-profile-cpu-tests
jit-profile-cpu-check:
	@case '$(PROFILE_CPU)' in ''|*[!0-9]*) echo 'PROFILE_CPU must select one CPU' >&2; exit 2;; esac
	@taskset -c '$(PROFILE_CPU)' true

jit-profile-cpu-tests:
	@bash tests/test-jit-profile-cpu.sh

jit-bench-profile-run: jit-profile-cpu-check
	@case '$(PROFILE_CASE)' in integer_loop|float_loop|array_table|closure_upvalue|polymorphic_metamethod|rust_callbacks|allocation_gc|oslo_predicate) ;; *) echo 'Unknown warm benchmark case' >&2; exit 2;; esac
	@case '$(PROFILE_MODE)' in off|auto) ;; *) echo 'PROFILE_MODE must be off|auto' >&2; exit 2;; esac
	@test -x '$(JIT_BENCH_BINARY)'
	@mkdir -p '$(BENCH_PROFILE_DIR)'
	@{ rustc -vV; $(CARGO) --version; uname -sm; } > '$(BENCH_PROFILE_DIR)/environment.log'
	@sha256sum '$(JIT_BENCH_BINARY)' > '$(BENCH_PROFILE_DIR)/binary-sha256.log'
	@valgrind --version > '$(BENCH_PROFILE_DIR)/profiler.log'
	@printf 'cpu=%s\n' '$(PROFILE_CPU)' >> '$(BENCH_PROFILE_DIR)/profiler.log'
	@printf 'case=%s\nmode=%s\nsamples=11\nwarmups=2\ncollection=*Executor>::step\n' '$(PROFILE_CASE)' '$(PROFILE_MODE)' >> '$(BENCH_PROFILE_DIR)/profiler.log'
	@taskset -c '$(PROFILE_CPU)' valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*Executor>::step' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file='$(BENCH_PROFILE_DIR)/profile.callgrind' '$(JIT_BENCH_BINARY)' --mode '$(PROFILE_MODE)' --case '$(PROFILE_CASE)' --samples 11 > '$(BENCH_PROFILE_DIR)/run.log' 2>&1
	@grep -Eq '^summary: [1-9][0-9]*' '$(BENCH_PROFILE_DIR)/profile.callgrind'
	@grep -Eq '^case=$(PROFILE_CASE) mode=$(PROFILE_MODE) samples=11 ' '$(BENCH_PROFILE_DIR)/run.log'
	@callgrind_annotate --inclusive=no --threshold=99 '$(BENCH_PROFILE_DIR)/profile.callgrind' > '$(BENCH_PROFILE_DIR)/annotation.log'
	@sha256sum --check '$(BENCH_PROFILE_DIR)/binary-sha256.log'

jit-metrics-build:
	@mkdir -p target/jit-evidence/metrics
	@printf 'opt_level=3\nlto=true\ncodegen_units=1\nstrip=true\nfeatures=jit,async\nRUSTFLAGS=%s\nCARGO_ENCODED_RUSTFLAGS=%s\n' "$${RUSTFLAGS:-}" "$${CARGO_ENCODED_RUSTFLAGS:-}" > target/jit-evidence/metrics/build-configuration.log
	@CARGO_TARGET_DIR='$(TOP_DIR)/target' LUNA_METRICS_OPT_LEVEL=3 CARGO_PROFILE_RELEASE_OPT_LEVEL=3 CARGO_PROFILE_RELEASE_LTO=true CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=true $(CARGO) build --locked --release --example jit_metrics --features jit,async $(TARGET_ARG)

jit-metrics-run:
	@mkdir -p target/jit-evidence/metrics
	@$(MAKE) --no-print-directory environment > target/jit-evidence/metrics/environment.log
	@rustc -vV >> target/jit-evidence/metrics/environment.log
	@if command -v lscpu >/dev/null; then lscpu >> target/jit-evidence/metrics/environment.log; fi
	@test -x '$(JIT_METRICS_BINARY)'
	@sha256sum '$(JIT_METRICS_BINARY)' > target/jit-evidence/metrics/binary-sha256.log
	@set -o pipefail; '$(JIT_METRICS_BINARY)' $(ARGS) 2>&1 | tee target/jit-evidence/metrics/observations.log

jit-metrics: jit-metrics-build
	@$(MAKE) --no-print-directory jit-metrics-run ARGS='$(ARGS)'

jit-metrics-tests:
	@$(CARGO) test --locked --example jit_metrics --features jit $(TARGET_ARG)
	@$(CARGO) test --locked --example jit_metrics --features jit,async $(TARGET_ARG)

jit-bench-paired:
	@$(MAKE) --no-print-directory jit-bench ARGS='--mode paired --samples 11 $(ARGS)'

PERF ?= perf
CYCLES_DIR ?= target/jit-evidence/warmed-cycles
CYCLES_IMAGE ?= $(TOP_DIR)/target/$(if $(TARGET),$(TARGET)/,)release/examples/jit_cycles
CYCLES_CASE ?= rust_callbacks
CYCLES_MODE ?= auto
CYCLES_ITERATIONS ?= 1000
CYCLES_EVENT ?= cycles:u
.PHONY: jit-cycles-tests jit-cycles-build jit-cycles-run
jit-cycles-tests:
	@$(CARGO) test --locked -p luna --features jit --example jit_cycles $(TARGET_ARG) $(ARGS)

jit-cycles-build:
	@CARGO_PROFILE_RELEASE_OPT_LEVEL=3 CARGO_PROFILE_RELEASE_STRIP=false $(CARGO) build --locked -p luna --features jit --release --example jit_cycles $(TARGET_ARG)

jit-cycles-run: jit-profile-cpu-check
	@test -x '$(CYCLES_IMAGE)'
	@mkdir '$(CYCLES_DIR)'
	@set -e; ctl='$(CYCLES_DIR)/control'; ack='$(CYCLES_DIR)/ack'; \
		mkfifo "$$ctl" "$$ack"; trap 'rm -f "$$ctl" "$$ack"' EXIT; \
		timeout 180s taskset -c '$(PROFILE_CPU)' '$(PERF)' record -D -1 -e '$(CYCLES_EVENT)' -F 997 \
			--control="fifo:$$ctl,$$ack" -o '$(CYCLES_DIR)/perf.data' -- \
			'$(CYCLES_IMAGE)' '$(CYCLES_MODE)' '$(CYCLES_CASE)' '$(CYCLES_ITERATIONS)' "$$ctl" "$$ack" \
			> '$(CYCLES_DIR)/run.log' 2> '$(CYCLES_DIR)/perf.log'
	@$(PERF) report --stdio --no-children --percent-limit 0 --sort symbol,dso -i '$(CYCLES_DIR)/perf.data' > '$(CYCLES_DIR)/report.log'
	@$(PERF) script -F ip,sym,dso,period -i '$(CYCLES_DIR)/perf.data' > '$(CYCLES_DIR)/samples.log'
	@grep -F 'cycles_protocol=1 mode=$(CYCLES_MODE) case=$(CYCLES_CASE) warmups=2 iterations=$(CYCLES_ITERATIONS) verified=1 compilation_delta=0' '$(CYCLES_DIR)/run.log'

jit-performance:
	@$(MAKE) --no-print-directory jit-bench JIT_BENCH_OPT=3 ARGS='--mode paired --samples 11 --check $(ARGS)'

jit-shipping:
	@$(MAKE) --no-print-directory jit-bench JIT_BENCH_OPT=s ARGS='--mode paired --samples 11 $(ARGS)'

jit-cost-tests:
	@$(CARGO) test --example jit_feature_cost --no-default-features $(TARGET_ARG)
	@$(CARGO) test --example jit_feature_cost --no-default-features --features jit $(TARGET_ARG)
	@$(MAKE) --no-print-directory jit-artifact-tests

JIT_COST_SELECTOR_DIR ?= target/jit-evidence/cost-selector
JIT_COST_SELECTOR_BUILD = $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)/$(if $(TARGET),$(TARGET)/,)debug/examples
.PHONY: jit-cost-selector-tests
jit-cost-selector-tests:
	@mkdir -p '$(JIT_COST_SELECTOR_DIR)'
	@$(CARGO) build --locked --example jit_feature_cost --no-default-features $(TARGET_ARG)
	@cp '$(JIT_COST_SELECTOR_BUILD)/jit_feature_cost' '$(JIT_COST_SELECTOR_DIR)/no-jit'
	@$(CARGO) build --locked --example jit_feature_cost --no-default-features --features jit $(TARGET_ARG)
	@cp '$(JIT_COST_SELECTOR_BUILD)/jit_feature_cost' '$(JIT_COST_SELECTOR_DIR)/jit'
	@bash tests/test-jit-cost-selector.sh '$(JIT_COST_SELECTOR_DIR)/no-jit' '$(JIT_COST_SELECTOR_DIR)/jit'

.PHONY: jit-oslo-profile-tests
jit-oslo-profile-tests:
	@$(CARGO) test --locked --example jit_oslo_profile --no-default-features $(TARGET_ARG)
	@$(CARGO) test --locked --example jit_oslo_profile --no-default-features --features jit $(TARGET_ARG)

OSLO_CONSUMER_SOURCE ?= /home/bresilla/data/code/tools/oslo
OSLO_CONSUMER_DIR ?= target/jit-evidence/oslo-consumer-$(shell git rev-parse --short HEAD)
.PHONY: jit-oslo-consumer-tests jit-oslo-consumer-setup jit-oslo-consumer-build jit-oslo-consumer-check jit-oslo-consumer-capture
jit-oslo-consumer-tests:
	@bash -n examples/jit_support/oslo_consumer.sh
	@perl -c examples/jit_support/oslo_consumer_run.pl
	@perl tests/test-jit-oslo-consumer.pl

jit-oslo-consumer-setup:
	@mkdir -p '$(dir $(abspath $(OSLO_CONSUMER_DIR)))'
	@bash examples/jit_support/oslo_consumer.sh setup '$(abspath $(OSLO_CONSUMER_DIR))' '$(abspath $(OSLO_CONSUMER_SOURCE))'

jit-oslo-consumer-build:
	@test -z "$${RUSTFLAGS:-}$${CARGO_ENCODED_RUSTFLAGS:-}$${RUSTC_WRAPPER:-}$${RUSTC_WORKSPACE_WRAPPER:-}"
	@{ rustc -vV; $(CARGO) --version; uname -sm; printf 'profiles=3,s\nlto=true\ncodegen_units=1\nstrip=true\n'; } >'$(abspath $(OSLO_CONSUMER_DIR))/build-environment.log'
	@bash examples/jit_support/oslo_consumer.sh build '$(abspath $(OSLO_CONSUMER_DIR))' '$(abspath $(OSLO_CONSUMER_SOURCE))'

jit-oslo-consumer-check:
	@bash examples/jit_support/oslo_consumer.sh check '$(abspath $(OSLO_CONSUMER_DIR))' '$(abspath $(OSLO_CONSUMER_SOURCE))'

jit-oslo-consumer-capture:
	@set -eu; for pid in $$(ps -eo pid=,comm= | awk '$$2 ~ /^(cargo|rustc|valgrind|callgrind|miri)/ {print $$1}'); do \
		directory=$$(readlink /proc/$$pid/cwd 2>/dev/null) || continue; \
		case "$$directory" in '$(TOP_DIR)'|'$(TOP_DIR)'/*|'$(abspath $(OSLO_CONSUMER_DIR))'|'$(abspath $(OSLO_CONSUMER_DIR))'/*) echo "Owned compute remains: $$pid $$directory" >&2; exit 2;; esac; \
	done
	@ps -eo pid,comm,args >'$(abspath $(OSLO_CONSUMER_DIR))/capture-processes-before.log'
	@{ date -u --iso-8601=seconds; cat /proc/stat /proc/pressure/cpu; } >'$(abspath $(OSLO_CONSUMER_DIR))/capture-telemetry-before.log'
	@bash examples/jit_support/oslo_consumer.sh capture '$(abspath $(OSLO_CONSUMER_DIR))' '$(abspath $(OSLO_CONSUMER_SOURCE))'
	@{ date -u --iso-8601=seconds; cat /proc/stat /proc/pressure/cpu; } >'$(abspath $(OSLO_CONSUMER_DIR))/capture-telemetry-after.log'
	@ps -eo pid,comm,args >'$(abspath $(OSLO_CONSUMER_DIR))/capture-processes-after.log'

.PHONY: lua-memory-limit lua-memory-limit-miri
lua-memory-limit:
	@$(CARGO) test --locked --no-default-features --lib $(TARGET_ARG) lua::memory_tests
	@$(CARGO) test --locked --no-default-features --test memory_accounting $(TARGET_ARG)
	@$(CARGO) test --locked --features jit --lib $(TARGET_ARG) lua::memory_tests
	@$(CARGO) test --locked --all-features --test memory_accounting --test jit_memory $(TARGET_ARG)
lua-memory-limit-miri:
	@$(CARGO) miri test --locked --no-default-features --lib --target '$(MIRI_TARGET)' lua::memory_tests -- --test-threads=1
	@$(CARGO) miri test --locked --features jit --lib --target '$(MIRI_TARGET)' lua::memory_tests -- --test-threads=1

.PHONY: jit-cost-verify jit-artifact-tests jit-environment-tests
jit-cost-verify:
	@bash examples/jit_support/verify_artifacts.sh '$(COST_DIR)'

jit-artifact-tests:
	@bash tests/test-jit-artifacts.sh
	@$(MAKE) --no-print-directory jit-environment-tests

jit-environment-tests:
	@bash tests/test-jit-environment.sh '$(SHELL)'

ELAPSED_AUDIT_ROOT ?=
.PHONY: jit-elapsed-audit jit-elapsed-audit-tests
jit-elapsed-audit:
	@test -n '$(ELAPSED_AUDIT_ROOT)'
	@cd '$(abspath $(ELAPSED_AUDIT_ROOT))/source' && sha256sum -c '$(abspath $(ELAPSED_AUDIT_ROOT))/elapsed/tools.sha256' > '$(abspath $(ELAPSED_AUDIT_ROOT))/elapsed-audit-v2-provenance.log'
	@sha256sum -c '$(abspath $(ELAPSED_AUDIT_ROOT))/baseline/binary.sha256' '$(abspath $(ELAPSED_AUDIT_ROOT))/candidate/binary.sha256' >> '$(abspath $(ELAPSED_AUDIT_ROOT))/elapsed-audit-v2-provenance.log'
	@cd '$(abspath $(ELAPSED_AUDIT_ROOT))/source' && sha256sum -c '$(abspath $(ELAPSED_AUDIT_ROOT))/candidate/source.sha256' >> '$(abspath $(ELAPSED_AUDIT_ROOT))/elapsed-audit-v2-provenance.log'
	@cd '$(abspath $(ELAPSED_AUDIT_ROOT))/baseline-source' && sha256sum -c '$(abspath $(ELAPSED_AUDIT_ROOT))/baseline/source.sha256' >> '$(abspath $(ELAPSED_AUDIT_ROOT))/elapsed-audit-v2-provenance.log'
	@sha256sum examples/jit_support/elapsed_audit.pl Makefile >> '$(abspath $(ELAPSED_AUDIT_ROOT))/elapsed-audit-v2-provenance.log'
	@perl examples/jit_support/elapsed_audit.pl '$(abspath $(ELAPSED_AUDIT_ROOT))' > '$(abspath $(ELAPSED_AUDIT_ROOT))/elapsed-audit-v2.json'

jit-elapsed-audit-tests:
	@perl tests/test-jit-elapsed-audit.pl

JIT_BENCH_LOG ?=
.PHONY: jit-bench-raw-audit jit-bench-raw-tests
jit-bench-raw-audit:
	@test -n '$(JIT_BENCH_LOG)'
	@perl examples/jit_support/paired_audit.pl '$(JIT_BENCH_LOG)'

jit-bench-raw-tests:
	@perl tests/test-jit-paired-raw.pl

jit-size-build:
	@case '$(SIZE_PROFILE)' in shipping|speed) ;; *) echo 'SIZE_PROFILE must be shipping|speed' >&2; exit 2;; esac
	@mkdir -p $(COST_DIR)
	@CARGO_TARGET_DIR='$(TOP_DIR)/target' $(MAKE) --no-print-directory environment > $(COST_DIR)/environment.log
	@printf 'profile=%s\nopt_level=%s\nlto=true\ncodegen_units=1\nstrip=%s\nRUSTFLAGS=%s\nCARGO_ENCODED_RUSTFLAGS=%s\n' '$(SIZE_PROFILE)' '$(COST_OPT)' '$(COST_STRIP)' "$${RUSTFLAGS:-}" "$${CARGO_ENCODED_RUSTFLAGS:-}" >> $(COST_DIR)/environment.log
	@rustc -vV >> $(COST_DIR)/environment.log
	@if command -v lscpu >/dev/null; then lscpu >> $(COST_DIR)/environment.log; fi
	@CARGO_TARGET_DIR='$(TOP_DIR)/target' CARGO_PROFILE_RELEASE_OPT_LEVEL=$(COST_OPT) CARGO_PROFILE_RELEASE_LTO=true CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=$(COST_STRIP) $(CARGO) build --locked --release --no-default-features --example jit_feature_cost $(TARGET_ARG)
	@cp $(COST_RELEASE_DIR)/jit_feature_cost $(COST_DIR)/no-jit
	@$(CARGO) tree -p luna --no-default-features -e normal $(TARGET_ARG) > $(COST_DIR)/no-jit-dependencies.log
	@CARGO_TARGET_DIR='$(TOP_DIR)/target' CARGO_PROFILE_RELEASE_OPT_LEVEL=$(COST_OPT) CARGO_PROFILE_RELEASE_LTO=true CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=$(COST_STRIP) $(CARGO) build --locked --release --no-default-features --features jit --example jit_feature_cost $(TARGET_ARG)
	@cp $(COST_RELEASE_DIR)/jit_feature_cost $(COST_DIR)/jit-off
	@$(CARGO) tree -p luna --no-default-features --features jit -e normal $(TARGET_ARG) > $(COST_DIR)/jit-dependencies.log
	@sha256sum $(COST_DIR)/no-jit $(COST_DIR)/jit-off > $(COST_DIR)/binary-sha256.log
	@size $(COST_DIR)/no-jit $(COST_DIR)/jit-off | tee $(COST_DIR)/sections.log

jit-cost-native: jit-size-build
	@$(MAKE) --no-print-directory jit-cost-native-run

jit-cost-native-run:
	@test -x '$(COST_DIR)/jit-off'
	@set -o pipefail; $(COST_DIR)/jit-off --mode auto --iterations 1 2>&1 | tee $(COST_DIR)/native-proof.log

jit-size: jit-size-build
	@$(MAKE) --no-print-directory jit-size-run

jit-size-run:
	@test -x '$(COST_DIR)/jit-off'
	@test -x '$(COST_DIR)/no-jit'
	@$(MAKE) --no-print-directory jit-cost-verify
	@$(MAKE) --no-print-directory jit-cost-native-run
	@$(MAKE) --no-print-directory jit-cost-verify
	@set -o pipefail; $(COST_DIR)/no-jit --compare $(COST_DIR)/no-jit $(COST_DIR)/jit-off --samples $(COST_SAMPLES) --iterations $(COST_ITERATIONS) --check 2>&1 | tee $(COST_DIR)/cost.log; result=$$?; $(MAKE) --no-print-directory jit-cost-verify || exit $$?; exit $$result

jit-cost-profile:
	@$(MAKE) --no-print-directory jit-size-build SIZE_PROFILE=speed COST_STRIP=false
	@$(MAKE) --no-print-directory jit-cost-profile-run SIZE_PROFILE=speed COST_STRIP=false

.PHONY: jit-cost-profile-run
jit-cost-profile-run: jit-profile-cpu-check
	@case '$(PROFILE_CASE)' in integer_loop|float_loop|array_table|closure_upvalue|polymorphic_metamethod|rust_callbacks|allocation_gc|all) ;; *) echo 'Unknown profiling case' >&2; exit 2;; esac
	@case '$(PROFILE_MODE)' in off|auto) ;; *) echo 'PROFILE_MODE must be off|auto' >&2; exit 2;; esac
	@mkdir -p $(COST_PROFILE_DIR)
	@cp $(COST_DIR)/environment.log $(COST_DIR)/binary-sha256.log $(COST_PROFILE_DIR)/
	@valgrind --version > $(COST_PROFILE_DIR)/profiler.log
	@printf 'cpu=%s\n' '$(PROFILE_CPU)' >> $(COST_PROFILE_DIR)/profiler.log
	@printf 'case=%s\niterations=3\nwarmups=2\ncollection=*Executor>::step\nno_jit_mode=off\njit_mode=%s\n' '$(PROFILE_CASE)' '$(PROFILE_MODE)' >> $(COST_PROFILE_DIR)/profiler.log
	@set -e; selected=(); if test '$(PROFILE_CASE)' != all; then selected=(--case '$(PROFILE_CASE)'); fi; for variant in no-jit jit-off; do \
		mode=off; if test "$$variant" = jit-off; then mode='$(PROFILE_MODE)'; fi; \
		taskset -c '$(PROFILE_CPU)' valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*Executor>::step' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file=$(COST_PROFILE_DIR)/$$variant.callgrind \
			$(COST_DIR)/$$variant --worker --mode "$$mode" "$${selected[@]}" --iterations 3 > $(COST_PROFILE_DIR)/$$variant-profile.log 2>&1; \
		if test '$(PROFILE_CASE)' = all; then \
			for name in integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc oslo_predicate cold_config; do \
				grep -Eq "^case=$$name ns=[0-9]+ iterations=3 verified=1 native_instructions=[0-9]+$$" $(COST_PROFILE_DIR)/$$variant-profile.log; \
			done; \
		else grep -Eq '^case=$(PROFILE_CASE) ns=[0-9]+ iterations=3 verified=1 native_instructions=[0-9]+$$' $(COST_PROFILE_DIR)/$$variant-profile.log; fi; \
		grep -Eq '^summary: [1-9][0-9]*' $(COST_PROFILE_DIR)/$$variant.callgrind; \
		callgrind_annotate --inclusive=no --threshold=99 $(COST_PROFILE_DIR)/$$variant.callgrind > $(COST_PROFILE_DIR)/$$variant-annotation.log; \
	done

jit-profile-build:
	@CARGO_PROFILE_RELEASE_OPT_LEVEL=3 CARGO_PROFILE_RELEASE_STRIP=false $(CARGO) build --release --example jit_bench --features jit

.PHONY: jit-clean-release-package
jit-clean-release-package:
	@$(CARGO) clean --release -p luna $(TARGET_ARG)

jit-profile: jit-profile-build
	@mkdir -p target/jit-evidence
	@perf record -g -o target/jit-evidence/perf.data -- target/release/examples/jit_bench --mode auto --samples 1000 --case closure_upvalue
	@perf report --stdio -i target/jit-evidence/perf.data > target/jit-evidence/perf-report.log

.PHONY: jit-rust-assembly-run
jit-rust-assembly: jit-profile-build
	@$(MAKE) --no-print-directory jit-rust-assembly-run

jit-rust-assembly-run:
	@test -x '$(JIT_ASSEMBLY_BINARY)'
	@mkdir -p '$(JIT_ASSEMBLY_DIR)'
	@sha256sum '$(JIT_ASSEMBLY_BINARY)' > '$(JIT_ASSEMBLY_DIR)/binary-sha256.log'
	@set -o pipefail; objdump -Cd '$(JIT_ASSEMBLY_BINARY)' | awk '/<luna::jit::Runtime::(run|lookup)>:|<<luna::jit::Runtime>::(run|lookup)>:|<<luna::jit::Runtime>::invoke.*>:|<luna::jit::helpers::call.*>:|<luna::thread::vm::run_vm.*>:/ { emit=1 } emit { print } /^$$/ { emit=0 }' > '$(JIT_ASSEMBLY_DIR)/rust-assembly.log'
	@test -s '$(JIT_ASSEMBLY_DIR)/rust-assembly.log'

jit-disassembly:
	@case "$$(uname -sm)" in 'Linux x86_64') ;; 'Linux aarch64') ;; *) echo 'Native diagnostics require supported Linux host'; exit 2;; esac
	@mkdir -p '$(JIT_DUMP_DIR)'
	@$(MAKE) --no-print-directory environment > '$(JIT_DUMP_DIR)/environment.log'
	@objdump --version > '$(JIT_DUMP_DIR)/objdump-version.log'
	@set -o pipefail; LUNA_JIT_DIAGNOSTIC_DIR='$(abspath $(JIT_DUMP_DIR))' $(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::dump_finalized_native_kernels -- --exact --ignored --nocapture 2>&1 | tee '$(JIT_DUMP_DIR)/test.log'
	@grep -Fq 'test jit::backend::memory_tests::dump_finalized_native_kernels ... ok' '$(JIT_DUMP_DIR)/test.log'
	@set -e; case "$$(uname -m)" in x86_64) machine='i386:x86-64';; aarch64) machine=aarch64;; esac; \
	for fixture in scalar table callback callback_direction; do \
		grep -Fxq "arch=$$(uname -m)" '$(JIT_DUMP_DIR)'/$$fixture.metadata; \
		grep -Eq '^helper=canonical_get_up_table_c3 address=0x[[:xdigit:]]+$$' '$(JIT_DUMP_DIR)'/$$fixture.metadata; \
		address=$$(sed -n 's/^entry_address=//p' '$(JIT_DUMP_DIR)'/$$fixture.metadata); \
		test -n "$$address"; test -s '$(JIT_DUMP_DIR)'/$$fixture.bin; \
		objdump -D -b binary -m "$$machine" --adjust-vma="$$address" '$(JIT_DUMP_DIR)'/$$fixture.bin > '$(JIT_DUMP_DIR)'/$$fixture.asm; \
		grep -Eq '^[[:space:]]*[[:xdigit:]]+:[[:space:]]' '$(JIT_DUMP_DIR)'/$$fixture.asm; \
		sha256sum '$(JIT_DUMP_DIR)'/$$fixture.bin '$(JIT_DUMP_DIR)'/$$fixture.metadata > '$(JIT_DUMP_DIR)'/$$fixture.sha256; \
	done

jit-reference:
	@$(CARGO) test -p luna --test fuel_reference $(TARGET_ARG)

.PHONY: jit-accounting
jit-accounting: jit-stats
	@$(CARGO) test --locked -p luna --features jit --test fuel_reference $(TARGET_ARG) $(ARGS)

.PHONY: jit-atomic-stats jit-atomic-stats-miri
jit-atomic-stats:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::atomic_stats::tests $(ARGS)
	@$(MAKE) --no-print-directory jit-atomic-call jit-rooted-call jit-accounting

jit-atomic-stats-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::atomic_stats::tests -- --test-threads=1

.PHONY: jit-chain
jit-chain:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::chains:: $(ARGS)

jit-backend:
	@$(CARGO) test -p luna --features jit --test jit_backend $(TARGET_ARG)

.PHONY: jit-entry-guard jit-builtin-fusion jit-builtin-identity jit-builtin-miri
.PHONY: jit-wide-helper
.PHONY: jit-wide-compiler
jit-wide-compiler:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::prepared_keys::wide_tests:: -- --list | grep -F 'wide_compiler_matches_all_entries_budgets_and_helper_outcomes: test'
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::prepared_keys::wide_tests:: -- --nocapture
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::prepared_keys::tests:: -- --nocapture

WIDE_HELPER_DIR ?= target/jit-evidence/short-slice-performance/wide-helper-profile
jit-wide-helper:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helpers::wide:: -- --list | grep -F 'generated_wide_calls_match_current_values_guards_and_panics: test'
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helpers::wide:: -- --nocapture
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::prepared_keys::tests::prepared_rust_helper_matches_generic_mutation_and_decline -- --exact --nocapture

.PHONY: jit-wide-helper-miri jit-wide-helper-profile-build jit-wide-helper-profile-run
jit-wide-helper-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::helpers::wide:: -- --test-threads=1 | awk '/^test result: ok[.] 1 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::prepared_keys::tests::prepared_rust_helper_matches_generic_mutation_and_decline -- --exact --test-threads=1 | awk '/^test result: ok[.] 1 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-wide-helper-profile-build:
	@mkdir -p '$(WIDE_HELPER_DIR)'
	@CARGO_PROFILE_RELEASE_OPT_LEVEL=3 CARGO_PROFILE_RELEASE_STRIP=false $(CARGO) test --locked --release -p luna --features jit --lib $(TARGET_ARG) --no-run --message-format=json-render-diagnostics > '$(WIDE_HELPER_DIR)/build.jsonl'
	@jq -r 'select(.reason == "compiler-artifact" and .target.name == "luna" and .profile.test and .executable != null) | .executable' '$(WIDE_HELPER_DIR)/build.jsonl' > '$(WIDE_HELPER_DIR)/binary-path'
	@test "$$(wc -l < '$(WIDE_HELPER_DIR)/binary-path')" -eq 1
	@cp "$$(cat '$(WIDE_HELPER_DIR)/binary-path')" '$(WIDE_HELPER_DIR)/test-binary'
	@sha256sum '$(WIDE_HELPER_DIR)/test-binary' > '$(WIDE_HELPER_DIR)/binary.sha256'
	@{ find src -type f -name '*.rs' -print0; printf '%s\0' examples/jit_support/workloads.rs Cargo.toml Cargo.lock Makefile; } | sort -z | xargs -0 sha256sum > '$(WIDE_HELPER_DIR)/source.sha256'
	@{ rustc -vV; $(CARGO) --version; printf 'opt_level=3\nstrip=false\ncalls=100000\ncollection=*wide::native::profile_calls*\n'; } > '$(WIDE_HELPER_DIR)/configuration'

jit-wide-helper-profile-run: jit-profile-cpu-check
	@sha256sum -c '$(WIDE_HELPER_DIR)/binary.sha256' '$(WIDE_HELPER_DIR)/source.sha256' > '$(WIDE_HELPER_DIR)/verification-before.log'
	@valgrind --version > '$(WIDE_HELPER_DIR)/profiler.log'
	@set -e; for shape in scalar callback; do for wide in 0 1; do \
		LUNA_WIDE_SOURCE=$$shape LUNA_WIDE_HELPER=$$wide LUNA_WIDE_DIR='$(WIDE_HELPER_DIR)' taskset -c '$(PROFILE_CPU)' valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*wide::native::profile_calls*' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file='$(WIDE_HELPER_DIR)'/$$shape-$$wide.callgrind \
			'$(WIDE_HELPER_DIR)/test-binary' jit::helpers::wide::native::profile_generated_helper_calls --ignored --exact --nocapture --test-threads=1 > '$(WIDE_HELPER_DIR)'/$$shape-$$wide.log 2>&1; \
		grep -Fq "shape=$$shape wide=$$([ $$wide = 1 ] && echo true || echo false) calls=100000 completed=100000 validated=true" '$(WIDE_HELPER_DIR)'/$$shape-$$wide.log; \
		grep -Fq 'test result: ok. 1 passed; 0 failed' '$(WIDE_HELPER_DIR)'/$$shape-$$wide.log; \
		grep -Eq '^summary: [1-9][0-9]*' '$(WIDE_HELPER_DIR)'/$$shape-$$wide.callgrind; \
		callgrind_annotate --inclusive=no --threshold=99 '$(WIDE_HELPER_DIR)'/$$shape-$$wide.callgrind > '$(WIDE_HELPER_DIR)'/$$shape-$$wide-annotation.log; \
	done; done
	@sha256sum -c '$(WIDE_HELPER_DIR)/binary.sha256' '$(WIDE_HELPER_DIR)/source.sha256' > '$(WIDE_HELPER_DIR)/verification-after.log'

jit-builtin-fusion:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::thread::builtin::tests:: -- --nocapture
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::executor::builtin_tests:: -- --list | grep -F 'builtin_fusion_matches_every_step_and_collects_between_steps: test'
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::executor::builtin_tests:: -- --nocapture

jit-builtin-identity:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::registry::intrinsic_tests:: -- --list | grep -F 'builtin_identity_is_instance_based_and_survives_registry_reset: test'
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::registry::intrinsic_tests:: -- --nocapture

jit-builtin-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::thread::builtin::tests:: -- --test-threads=1 | awk '/^test result: ok[.] 3 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::registry::intrinsic_tests:: -- --test-threads=1 | awk '/^test result: ok[.] 2 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-loop-direction:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::direction::tests:: -- --list | grep -F 'native_loop_direction_matches_control_and_model_at_all_boundaries: test'
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::direction::tests:: -- --nocapture

jit-entry-guard:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::entry_guard_tests -- --list | grep -Fx 'jit::backend::entry_guard_tests::arithmetic_entry_guards_preserve_values_and_budget_priority: test'
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::entry_guard_tests $(ARGS)

.PHONY: jit-split-scratch jit-split-scratch-miri
jit-split-scratch:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::split:: -- --list | grep -Fx 'jit::abi::split::tests::planes_have_bounded_storage_and_aligned_payloads: test'
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::split:: $(ARGS)

jit-split-scratch-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::split:: -- --test-threads=1 | awk '/^test result: ok[.] 4 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

.PHONY: jit-split-scratch-profile-build jit-split-scratch-profile-run
jit-split-scratch-profile-build:
	@mkdir -p '$(SPLIT_SCRATCH_DIR)'
	@CARGO_PROFILE_RELEASE_OPT_LEVEL=3 CARGO_PROFILE_RELEASE_STRIP=false $(CARGO) test --locked --release -p luna --features jit --lib $(TARGET_ARG) --no-run --message-format=json-render-diagnostics > '$(SPLIT_SCRATCH_DIR)/build.jsonl'
	@jq -r 'select(.reason == "compiler-artifact" and .target.name == "luna" and .profile.test and .executable != null) | .executable' '$(SPLIT_SCRATCH_DIR)/build.jsonl' > '$(SPLIT_SCRATCH_DIR)/binary-path'
	@test "$$(wc -l < '$(SPLIT_SCRATCH_DIR)/binary-path')" -eq 1
	@cp "$$(cat '$(SPLIT_SCRATCH_DIR)/binary-path')" '$(SPLIT_SCRATCH_DIR)/test-binary'
	@sha256sum '$(SPLIT_SCRATCH_DIR)/test-binary' > '$(SPLIT_SCRATCH_DIR)/binary.sha256'
	@{ find src -type f -name '*.rs' -print0; printf '%s\0' Cargo.toml Cargo.lock Makefile; } | sort -z | xargs -0 sha256sum > '$(SPLIT_SCRATCH_DIR)/source.sha256'
	@{ rustc -vV; $(CARGO) --version; printf 'opt_level=3\nstrip=false\nsteps=100000\ncollection=*split::probe::transport_probe_*\n'; } > '$(SPLIT_SCRATCH_DIR)/configuration'

jit-split-scratch-profile-run: jit-profile-cpu-check
	@sha256sum -c '$(SPLIT_SCRATCH_DIR)/binary.sha256' '$(SPLIT_SCRATCH_DIR)/source.sha256' > '$(SPLIT_SCRATCH_DIR)/verification-before.log'
	@valgrind --version > '$(SPLIT_SCRATCH_DIR)/profiler.log'
	@set -e; for mixed in 0 1; do for split in 0 1; do \
		LUNA_SPLIT_SCRATCH=$$split LUNA_SPLIT_MIXED=$$mixed taskset -c '$(PROFILE_CPU)' valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*split::probe::transport_probe_*' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file='$(SPLIT_SCRATCH_DIR)'/$$mixed-$$split.callgrind \
			'$(SPLIT_SCRATCH_DIR)/test-binary' jit::abi::split::probe::profile_roundtrip_transport --ignored --exact --nocapture --test-threads=1 > '$(SPLIT_SCRATCH_DIR)'/$$mixed-$$split.log 2>&1; \
		grep -Fq "split=$$([ $$split = 1 ] && echo true || echo false) mixed=$$([ $$mixed = 1 ] && echo true || echo false) steps=100000 result=100000" '$(SPLIT_SCRATCH_DIR)'/$$mixed-$$split.log; \
		grep -Fq 'test result: ok. 1 passed; 0 failed' '$(SPLIT_SCRATCH_DIR)'/$$mixed-$$split.log; \
		grep -Eq '^summary: [1-9][0-9]*' '$(SPLIT_SCRATCH_DIR)'/$$mixed-$$split.callgrind; \
		callgrind_annotate --inclusive=no --threshold=99 '$(SPLIT_SCRATCH_DIR)'/$$mixed-$$split.callgrind > '$(SPLIT_SCRATCH_DIR)'/$$mixed-$$split-annotation.log; \
	done; done
	@sha256sum -c '$(SPLIT_SCRATCH_DIR)/binary.sha256' '$(SPLIT_SCRATCH_DIR)/source.sha256' > '$(SPLIT_SCRATCH_DIR)/verification-after.log'

jit-helpers:
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::helpers::tests

.PHONY: jit-existing-table-entry
jit-existing-table-entry:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helpers::

.PHONY: jit-leaf jit-leaf-miri
jit-leaf:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::leaf::tests $(ARGS)

jit-leaf-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::leaf::tests -- --test-threads=1

.PHONY: jit-integer jit-integer-miri
.PHONY: jit-continuations jit-continuations-miri
jit-continuations:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::continuations::tests $(ARGS)

jit-continuations-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::continuations::tests -- --test-threads=1 $(ARGS)

.PHONY: jit-call-plans jit-call-plans-miri
jit-call-plans:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::calls::tests $(ARGS)

jit-call-plans-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::calls::tests -- --test-threads=1 $(ARGS)

.PHONY: jit-call-native
jit-call-native:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::calls::tests $(ARGS)

.PHONY: jit-compact-callee
jit-compact-callee:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::compact::tests $(ARGS)

COMPACT_CALLEE_DIR ?= target/jit-evidence/compact-callee
COMPACT_CALLEE_FILTER ?= jit::backend::compact::tests
.PHONY: jit-compact-callee-memcheck
jit-compact-callee-memcheck:
	@mkdir -p '$(COMPACT_CALLEE_DIR)'
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) --no-run --message-format=json-render-diagnostics > '$(COMPACT_CALLEE_DIR)/build.jsonl'
	@jq -r 'select(.reason == "compiler-artifact" and .target.name == "luna" and .profile.test and .executable != null) | .executable' '$(COMPACT_CALLEE_DIR)/build.jsonl' > '$(COMPACT_CALLEE_DIR)/binary-path'
	@test "$$(wc -l < '$(COMPACT_CALLEE_DIR)/binary-path')" -eq 1
	@valgrind --tool=memcheck --error-exitcode=99 --leak-check=full --show-leak-kinds=all --errors-for-leak-kinds=definite,indirect "$$(cat '$(COMPACT_CALLEE_DIR)/binary-path')" '$(COMPACT_CALLEE_FILTER)' --test-threads=1 > '$(COMPACT_CALLEE_DIR)/memcheck.log' 2>&1

.PHONY: jit-call-canonical
jit-call-canonical:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::canonical::tests $(ARGS)

.PHONY: jit-atomic-call
.PHONY: jit-direct-capture jit-direct-capture-miri
jit-direct-capture:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::thread::tests::direct_capture
	@$(MAKE) --no-print-directory jit-rooted-call jit-atomic-call jit-accounting

jit-direct-capture-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::thread::tests::direct_capture -- --test-threads=1

jit-atomic-call: jit-snapshot-fuel
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::canonical::tests::atomic $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::activation::atomic_call $(ARGS)

.PHONY: jit-rooted-call jit-rooted-call-miri
.PHONY: jit-preserved-roots jit-preserved-roots-miri
jit-preserved-roots:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::roots::call::preserved $(ARGS)

jit-preserved-roots-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::roots::call::preserved -- --test-threads=1

.PHONY: jit-snapshot-fuel jit-snapshot-fuel-miri
jit-snapshot-fuel:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::activation::snapshot_fuel $(ARGS)

jit-snapshot-fuel-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::activation::snapshot_fuel -- --test-threads=1

jit-rooted-call:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::roots::call $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::canonical::tests::rooted $(ARGS)

jit-rooted-call-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::roots::call -- --test-threads=1

.PHONY: jit-scoped-helpers
jit-scoped-helpers:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::scoped_helpers::tests $(ARGS)

.PHONY: jit-native-region
jit-native-region:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::region::tests $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::native_region::tests $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::native_region::resume::tests $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::native_region::cache::tests $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::canonical::tests::native_region_ $(ARGS)

.PHONY: jit-native-region-cost jit-native-region-cost-run
.PHONY: jit-regions
jit-regions:
	@$(CARGO) test --locked -p luna --features jit --test jit_regions $(TARGET_ARG) $(ARGS)
	@$(CARGO) test --locked -p luna --features jit,async --test jit_regions $(TARGET_ARG) $(ARGS)

jit-native-region-cost:
	@CARGO_PROFILE_RELEASE_OPT_LEVEL=$(JIT_BENCH_OPT) $(CARGO) test --locked --release -p luna --features jit --lib $(TARGET_ARG) jit::native_region::tests::cost::paired_region_cost -- --ignored --exact --nocapture --test-threads=1

jit-native-region-cost-run:
	@test -n '$(REGION_COST_BINARY)' && test -x '$(REGION_COST_BINARY)'
	@'$(REGION_COST_BINARY)' jit::native_region::tests::cost::paired_region_cost --ignored --exact --nocapture --test-threads=1

.PHONY: jit-native-region-profile-build jit-native-region-profile-run
jit-native-region-profile-build:
	@mkdir -p '$(REGION_PROFILE_DIR)'
	@CARGO_PROFILE_RELEASE_OPT_LEVEL=$(JIT_BENCH_OPT) CARGO_PROFILE_RELEASE_STRIP=false $(CARGO) test --locked --release -p luna --features jit --lib $(TARGET_ARG) --no-run --message-format=json-render-diagnostics > '$(REGION_PROFILE_DIR)/build.jsonl'
	@jq -r 'select(.reason == "compiler-artifact" and .target.name == "luna" and .profile.test and .executable != null) | .executable' '$(REGION_PROFILE_DIR)/build.jsonl' > '$(REGION_PROFILE_DIR)/binary-path'
	@test "$$(wc -l < '$(REGION_PROFILE_DIR)/binary-path')" -eq 1
	@cp "$$(cat '$(REGION_PROFILE_DIR)/binary-path')" '$(REGION_PROFILE_DIR)/test-binary'
	@sha256sum '$(REGION_PROFILE_DIR)/test-binary' > '$(REGION_PROFILE_DIR)/binary.sha256'
	@{ find src -type f -name '*.rs' -print0; printf '%s\0' Cargo.toml Cargo.lock Makefile examples/jit_support/workloads.rs; } | sort -z | xargs -0 sha256sum > '$(REGION_PROFILE_DIR)/source.sha256'
	@git rev-parse HEAD > '$(REGION_PROFILE_DIR)/revision'
	@git diff --binary > '$(REGION_PROFILE_DIR)/source.patch'
	@printf 'opt_level=%s\nstrip=false\ncollection=*cost::profile_region\niterations=3\ncase=%s\n' '$(JIT_BENCH_OPT)' "$${LUNA_REGION_CASE:-closure_upvalue}" > '$(REGION_PROFILE_DIR)/configuration'

jit-native-region-profile-run:
	@sha256sum -c '$(REGION_PROFILE_DIR)/binary.sha256' '$(REGION_PROFILE_DIR)/source.sha256' > '$(REGION_PROFILE_DIR)/verification.log'
	@valgrind --version > '$(REGION_PROFILE_DIR)/profiler.log'
	@set -e; for mode in off auto region; do \
		LUNA_REGION_PROFILE_MODE=$$mode LUNA_REGION_ITERATIONS=3 valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*cost::profile_region' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file='$(REGION_PROFILE_DIR)'/$$mode.callgrind \
			'$(REGION_PROFILE_DIR)/test-binary' jit::native_region::tests::cost::region_cost_profile --ignored --exact --nocapture --test-threads=1 > '$(REGION_PROFILE_DIR)'/$$mode.log 2>&1; \
		grep -Eq '^summary: [1-9][0-9]*' '$(REGION_PROFILE_DIR)'/$$mode.callgrind; \
		test "$$(grep -c 'profile_verified=true' '$(REGION_PROFILE_DIR)'/$$mode.log)" -eq 3; \
		callgrind_annotate --inclusive=no --threshold=99 '$(REGION_PROFILE_DIR)'/$$mode.callgrind > '$(REGION_PROFILE_DIR)'/$$mode-annotation.log; \
	done

.PHONY: jit-scoped-helpers-miri
.PHONY: jit-writeback-model jit-writeback-model-miri
jit-writeback-model:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::writeback::tests $(ARGS)

jit-writeback-model-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::writeback::tests -- --test-threads=1

jit-scoped-helpers-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::scoped_helpers::tests -- --test-threads=1

.PHONY: jit-call-pairs
jit-call-pairs:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::pairs::tests $(ARGS)

.PHONY: jit-call-observation
jit-call-observation:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::pairs::tests::vm_call_observation_preserves_count_across_slice_policies -- --exact

.PHONY: jit-call-runtime
jit-call-runtime:
	@$(CARGO) test --locked -p luna --features jit --test jit_call_pairs $(TARGET_ARG) $(ARGS)

jit-integer:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::integer::tests $(ARGS)

jit-integer-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::integer::tests -- --test-threads=1 $(ARGS)

.PHONY: jit-projection jit-projection-miri
.PHONY: jit-projection-mode jit-projection-mode-miri
.PHONY: jit-invoke-dispatch jit-invoke-dispatch-miri
jit-invoke-dispatch:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::invoke_dispatch:: $(ARGS)

jit-invoke-dispatch-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::invoke_dispatch:: -- --test-threads=1 $(ARGS)

jit-projection-mode:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::projection_mode:: $(ARGS)

jit-projection-mode-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::projection_mode:: -- --test-threads=1 $(ARGS)

jit-projection:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::projection:: $(ARGS)

jit-projection-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::projection:: -- --test-threads=1 $(ARGS)

.PHONY: jit-projection-runtime-miri
.PHONY: jit-projection-helper-miri
jit-projection-helper-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::projection::lowering::tests::helper_grammar -- --test-threads=1

.PHONY: jit-runtime-projection
jit-runtime-projection:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::runtime_projection_tests $(ARGS)

.PHONY: vm-activation-tests jit-activation-tests vm-activation-miri
.PHONY: vm-unwind vm-unwind-miri
vm-unwind:
	@set -o pipefail; $(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::vm::unwind::interpreter_panics_unwind_through_the_vm_boundary -- --exact --test-threads=1 --nocapture | awk '/^test result: ok[.] 1 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'
	@set -o pipefail; $(CARGO) test --locked -p luna --no-default-features --features jit --lib $(TARGET_ARG) thread::vm::unwind::interpreter_panics_unwind_through_the_vm_boundary -- --exact --test-threads=1 --nocapture | awk '/^test result: ok[.] 1 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

vm-unwind-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' thread::vm::unwind::interpreter_panics_unwind_through_the_vm_boundary -- --exact --test-threads=1 --nocapture | awk '/^test result: ok[.] 1 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --no-default-features --features jit --lib --target '$(MIRI_TARGET)' thread::vm::unwind::interpreter_panics_unwind_through_the_vm_boundary -- --exact --test-threads=1 --nocapture | awk '/^test result: ok[.] 1 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

.PHONY: vm-upvalue-returns
vm-upvalue-returns:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::thread::tests $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::thread::tests $(ARGS)
	@$(CARGO) test --locked -p luna --no-default-features --test close_attribute --test tail_call_stack_panic $(TARGET_ARG) $(ARGS)
	@for mode in off auto force; do LUNA_TEST_JIT_MODE=$$mode $(CARGO) test --locked -p luna --features jit --test close_attribute --test tail_call_stack_panic $(TARGET_ARG) $(ARGS) || exit $$?; done

.PHONY: vm-upvalue-returns-miri
vm-upvalue-returns-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' thread::thread::tests -- --test-threads=1

.PHONY: vm-call-layout vm-call-layout-miri
vm-call-layout:
	@for checked in true false; do \
		CARGO_PROFILE_DEV_OVERFLOW_CHECKS=$$checked $(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::thread::call_layout $(ARGS) || exit $$?; \
		CARGO_PROFILE_DEV_OVERFLOW_CHECKS=$$checked $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::thread::call_layout $(ARGS) || exit $$?; \
	done

vm-call-layout-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' thread::thread::call_layout -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::thread::call_layout -- --test-threads=1

.PHONY: vm-call-resolution vm-callback-outcomes-miri vm-callback-setup
vm-callback-setup:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::thread::callback_setup:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::thread::callback_setup:: $(ARGS)

.PHONY: jit-helper-counts jit-helper-counts-miri
.PHONY: jit-helper-publication jit-helper-publication-miri
jit-helper-publication:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helper_publication_tests:: | awk '/^test result: ok[.] 2 passed; 0 failed;/ {valid=1} {print} END {exit !valid}'

jit-helper-publication-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::helper_publication_tests:: -- --test-threads=1 | awk '/^test result: ok[.] 2 passed; 0 failed;/ {valid=1} {print} END {exit !valid}'

jit-helper-counts:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helper_counts:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::helper_flow_tests::read_effect_classification_covers_every_helper_and_position $(ARGS)

jit-helper-counts-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::helper_counts:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::helper_flow_tests::read_effect_classification_covers_every_helper_and_position -- --exact --test-threads=1

vm-callback-outcomes-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --test callback --target '$(MIRI_TARGET)' nested_callback_outcomes_preserve_payloads_and_single_drops -- --exact --test-threads=1

vm-call-resolution:
	@$(CARGO) test --locked -p luna --no-default-features --test callback --test metamethods --test tail_call_stack_panic $(TARGET_ARG)
	@for mode in off auto force; do \
		LUNA_TEST_JIT_MODE=$$mode $(CARGO) test --locked -p luna --features jit --test callback --test metamethods --test tail_call_stack_panic $(TARGET_ARG) || exit $$?; \
	done

vm-activation-tests:
	@$(CARGO) test --locked -p luna --lib $(TARGET_ARG) thread::executor::activation_tests $(ARGS)

.PHONY: vm-mode-checks vm-mode-checks-miri
.PHONY: vm-static-off vm-static-off-miri
vm-static-off:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::vm::static_off $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::executor::static_off_tests $(ARGS)
	@$(MAKE) --no-print-directory vm-hook-resume

vm-static-off-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::vm::static_off -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::vm::hook_resume -- --test-threads=1

.PHONY: vm-callback-chains
vm-callback-chains:
	@set -o pipefail; $(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::executor::callback_chain_tests $(ARGS) | awk '/^test result: ok[.] 1 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::executor::callback_chain_tests $(ARGS) | awk '/^test result: ok[.] 1 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

vm-mode-checks:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::executor::mode_tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::executor::mode_tests

vm-mode-checks-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' thread::executor::mode_tests -- --test-threads=1

jit-activation-tests:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::executor::activation_tests $(ARGS)

.PHONY: jit-in-place-call
jit-in-place-call:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::activation::call_tests $(ARGS)

.PHONY: jit-in-place-return
jit-in-place-return:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::activation::return_tests $(ARGS)

.PHONY: jit-payload-model jit-payload-model-miri
.PHONY: jit-payload-terminal jit-payload-terminal-miri
.PHONY: jit-payload-functions jit-payload-functions-miri
.PHONY: jit-payload-local-table jit-payload-local-table-miri
.PHONY: jit-payload-table-write jit-payload-table-write-miri
.PHONY: jit-payload-new-table jit-payload-new-table-miri
.PHONY: jit-payload-upvalue-read jit-payload-upvalue-read-miri
jit-payload-upvalue-read:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::runtime::upvalue_read_tests:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::terminal:: $(ARGS)

jit-payload-upvalue-read-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::runtime::upvalue_read_tests:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::terminal:: -- --test-threads=1

jit-payload-new-table:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::runtime::new_table_tests:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::terminal:: $(ARGS)

jit-payload-new-table-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::runtime::new_table_tests:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::terminal:: -- --test-threads=1

jit-payload-table-write:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::runtime::table_write_tests:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::terminal:: $(ARGS)

jit-payload-table-write-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::runtime::table_write_tests:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::terminal:: -- --test-threads=1

jit-payload-local-table:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::runtime::local_table_tests:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::terminal::tests::local_table_ $(ARGS)

jit-payload-local-table-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::runtime::local_table_tests:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::terminal::tests::local_table_ -- --test-threads=1

.PHONY: jit-payload-tags jit-payload-tags-miri
.PHONY: jit-payload-bound jit-payload-bound-miri
.PHONY: jit-payload-control jit-payload-control-miri
jit-payload-control:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::runtime::bound_tests::bound_entry_retains_control $(ARGS)

jit-payload-control-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::runtime::bound_tests::bound_entry_retains_control -- --test-threads=1

.PHONY: jit-payload-read-audit
jit-payload-read-audit:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::read_audit::original_workload_reads -- --exact --nocapture

.PHONY: jit-payload-set-list jit-payload-set-list-miri
jit-payload-set-list:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::runtime::set_list_tests:: $(ARGS)

jit-payload-set-list-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::runtime::set_list_tests:: -- --test-threads=1

.PHONY: jit-array-transport jit-array-transport-miri jit-array-transport-runtime
jit-array-transport:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::array_window::transport_tests:: $(ARGS)

jit-array-transport-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::array_window::transport_tests:: -- --test-threads=1

jit-array-transport-runtime:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::array_window::runtime_tests::hybrid_array_ $(ARGS)

.PHONY: jit-payload-generic-writes jit-payload-generic-writes-miri
jit-payload-generic-writes:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::writes:: $(ARGS)

jit-payload-generic-writes-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::writes:: -- --test-threads=1

jit-payload-bound:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::runtime::bound_tests:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::bound_ $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::numeric_reads::tests::bound_ $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::reads::bound_tests:: $(ARGS)

jit-payload-bound-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::runtime::bound_tests:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::bound_ -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::numeric_reads::tests::bound_ -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::reads::bound_tests:: -- --test-threads=1

jit-payload-tags:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::same_tag:: $(ARGS)

jit-payload-tags-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::same_tag:: -- --test-threads=1

.PHONY: jit-tag-effects jit-tag-effects-miri
jit-tag-effects:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::tag_effects:: $(ARGS)

jit-tag-effects-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::tag_effects:: -- --test-threads=1

jit-payload-functions:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::runtime::bridge_function_table $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::runtime::capacity_tests:: $(ARGS)

jit-payload-functions-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::runtime::bridge_function_table -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::runtime::capacity_tests:: -- --test-threads=1

jit-payload-terminal:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::runtime::terminal_tests:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::terminal:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::writes::terminal_ $(ARGS)

.PHONY: jit-payload-terminal-copy jit-payload-terminal-copy-miri
.PHONY: jit-payload-copy-bypass jit-payload-copy-bypass-miri
jit-payload-copy-bypass:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::copy_bypass:: $(ARGS)

jit-payload-copy-bypass-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::copy_bypass:: -- --test-threads=1

jit-payload-terminal-copy:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::runtime::terminal_copy_tests:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::copy:: $(ARGS)

jit-payload-terminal-copy-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::runtime::terminal_copy_tests:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::copy:: -- --test-threads=1

jit-payload-terminal-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::runtime::terminal_tests:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::terminal:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::writes::terminal_ -- --test-threads=1

.PHONY: jit-typed-payload jit-typed-payload-miri
jit-typed-payload:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::typed:: $(ARGS)

jit-typed-payload-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::typed:: -- --test-threads=1

.PHONY: jit-payload-relocations
jit-payload-relocations:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::relocation_budget_counts_emitted_direct_and_indirect_helper_calls -- --exact --nocapture

.PHONY: jit-helper-transports jit-helper-transports-miri
jit-helper-transports:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helpers:: $(ARGS)

jit-helper-transports-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::helpers:: -- --test-threads=1

.PHONY: jit-payload-packed jit-payload-packed-miri
.PHONY: jit-payload-packed-fast jit-payload-packed-fast-miri
jit-payload-packed-fast:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::packed::scoped::fast:: $(ARGS)

jit-payload-packed-fast-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::packed::scoped::fast:: -- --test-threads=1 $(ARGS)

jit-payload-packed:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::packed:: $(ARGS)

jit-payload-packed-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::packed:: -- --test-threads=1

.PHONY: jit-payload-packed-calls jit-payload-packed-calls-miri
jit-payload-packed-calls:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::packed_calls:: $(ARGS)

jit-payload-packed-calls-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::packed_calls:: -- --test-threads=1

.PHONY: jit-payload-packed-reads jit-payload-packed-reads-miri
.PHONY: jit-payload-packed-tag-checks jit-payload-packed-tag-checks-miri
.PHONY: jit-payload-binding-requirements jit-payload-binding-requirements-miri
.PHONY: jit-payload-sparse-runtime
.PHONY: jit-payload-write-binding-runtime
jit-payload-write-binding-runtime:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::payload_runtime_tests::write_binding_runtime_ $(ARGS)

jit-payload-sparse-runtime:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::payload_runtime_tests::sparse_runtime_ $(ARGS)

jit-payload-binding-requirements:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::binding_requirements:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::runtime::sparse_binding_tests:: $(ARGS)

jit-payload-binding-requirements-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::binding_requirements:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::runtime::sparse_binding_tests:: -- --test-threads=1

jit-payload-packed-tag-checks:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::packed_tag_checks:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::packed_lower::tests::packed_whole_tag_checks $(ARGS)

jit-payload-packed-tag-checks-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::packed_tag_checks:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::packed_lower::tests::packed_whole_tag_checks -- --test-threads=1

jit-payload-packed-reads:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::packed_reads:: $(ARGS)

jit-payload-packed-reads-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::packed_reads:: -- --test-threads=1

.PHONY: jit-payload-packed-writes jit-payload-packed-writes-miri
jit-payload-packed-writes:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::packed_writes:: $(ARGS)

jit-payload-packed-writes-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::packed_writes:: -- --test-threads=1

.PHONY: jit-payload-packed-lower jit-payload-packed-lower-miri
.PHONY: jit-payload-packed-entry
.PHONY: jit-payload-packed-runtime
jit-payload-packed-runtime:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::packed_runtime:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::payload_runtime_tests::packed_ $(ARGS)

jit-payload-packed-entry:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::packed_entry_tests:: $(ARGS)

jit-payload-packed-lower:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::packed_lower:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::packed_progress:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::integer_loop::checkpoint_tests:: $(ARGS)

jit-payload-packed-lower-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::packed_lower:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::packed_progress:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::integer_loop::checkpoint_tests:: -- --test-threads=1

jit-payload-model:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::payload_runtime_tests:: $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::packed_runtime:: $(ARGS)

.PHONY: jit-payload-reference-store jit-payload-reference-store-miri
.PHONY: jit-payload-full-binding jit-payload-full-binding-miri
jit-payload-full-binding:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::runtime::full_binding_tests:: $(ARGS)

jit-payload-full-binding-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::runtime::full_binding_tests:: -- --test-threads=1

.PHONY: jit-canonical-cells jit-canonical-cells-miri
jit-canonical-cells:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::canonical_cells:: $(ARGS)

jit-canonical-cells-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::canonical_cells:: -- --test-threads=1

jit-payload-reference-store:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::payload::binding_tests:: $(ARGS)

jit-payload-reference-store-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload::binding_tests:: -- --test-threads=1

jit-payload-model-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::payload:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload:: -- --test-threads=1

.PHONY: vm-jump-offset
vm-jump-offset:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) compiler::compiler::jump_offset_tests $(ARGS)
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::vm::offset_tests $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::vm::offset_tests $(ARGS)

.PHONY: jit-activation-host-miri
.PHONY: jit-interpreted-tail jit-interpreted-tail-miri
jit-interpreted-tail:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::executor::interpreted_tail $(ARGS)

jit-interpreted-tail-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::executor::interpreted_tail -- --test-threads=1

jit-activation-host-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::executor::activation_tests::scoped_activation_host_releases_before_nested_executor_callbacks -- --exact --test-threads=1

vm-activation-miri:
	@$(CARGO) miri test --locked -p luna --lib --target '$(MIRI_TARGET)' thread::executor::activation_tests::native_and_interpreted_nested_executors_preserve_open_upvalues -- --exact --test-threads=1

jit-projection-runtime-miri:
	@set -e; for filter in jit::projection::native::tests jit::projection::tests jit::helpers::tests jit::abi::tests::original_scratch_entry; do \
		$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' "$$filter" -- --test-threads=1; \
	done

.PHONY: jit-slice-policy
jit-slice-policy:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::slice_policy_tests::

jit-registry:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::registry::

.PHONY: jit-registry-miri
jit-registry-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::registry:: -- --test-threads=1

jit-ir:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::ir::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::flow::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::work::tests
	@$(MAKE) --no-print-directory jit-tags
	@$(MAKE) --no-print-directory jit-access
	@$(MAKE) --no-print-directory jit-exits

jit-tags:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::tags::tests
	@$(MAKE) --no-print-directory jit-known-bits
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::corrupted_scalar_store_is_refused_before_codegen_and_mapping
	@$(MAKE) --no-print-directory jit-input
	@$(MAKE) --no-print-directory jit-read-cache
	@$(MAKE) --no-print-directory jit-integer-loop

.PHONY: jit-known-bits jit-known-bits-miri
jit-known-bits:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::tags::known_bits::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::tags::tests::combined_numeric_guards_

jit-known-bits-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::tags::known_bits::tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::tags::tests::combined_numeric_guards_ -- --test-threads=1

.PHONY: jit-read-cache
.PHONY: jit-integer-loop
.PHONY: jit-native-transport jit-native-transport-miri jit-native-transport-runtime
jit-native-transport:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::transport_tests:: $(ARGS)

jit-native-transport-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::transport_tests:: -- --test-threads=1

jit-native-transport-runtime:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::payload_runtime_tests::hybrid_runtime_ $(ARGS)

.PHONY: jit-table-loop
jit-table-loop:
	@$(CARGO) test --locked -p luna --features jit --test jit_table_loop $(TARGET_ARG)

jit-integer-loop:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::integer_loop::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::model::tests::integer_loop_

jit-read-cache:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::read_cache::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::model::tests::cached_scalar_regions_match_the_boundary_model
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::model::tests::writeback_scalar_regions_match_the_boundary_model
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::model::tests::writeback_helpers_match_canonical_execution_at_every_boundary

.PHONY: jit-read-effects
.PHONY: jit-compact-integer-pairs
.PHONY: jit-compact-integer-pairs-miri
jit-compact-integer-pairs-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::tags::tests::compact_integer_pairs_ -- --test-threads=1

jit-compact-integer-pairs:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) compact_integer_pairs -- --nocapture
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::entry_guard_tests::
	@$(MAKE) --no-print-directory jit-arithmetic

jit-read-effects: jit-read-cache
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helper_flow::read_effect_tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::model::tests::effect_cached_
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) read_effects_runtime_

jit-input:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) numeric_guards
	@$(MAKE) --no-print-directory jit-float-input
	@$(MAKE) --no-print-directory jit-arithmetic
	@$(MAKE) --no-print-directory jit-truth
	@$(MAKE) --no-print-directory jit-comparison
	@$(MAKE) --no-print-directory jit-loop-source
	@$(MAKE) --no-print-directory jit-transfer-source
	@$(MAKE) --no-print-directory jit-helper-flow
	@$(MAKE) --no-print-directory jit-source-binding

jit-helper-flow:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::helper_flow_tests
	@$(MAKE) --no-print-directory jit-helper-flow-backend

jit-helper-flow-backend:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::corrupted_helper_flow_

jit-helper-flow-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::helper_flow_tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::lifetime_tests::symbol_layout_counts_registration_declarations_and_lookup_temporary -- --exact --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::lifetime_tests::signature_layout_counts_initial_declaration_and_import_vectors -- --exact --test-threads=1

jit-transfer-source:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::transfer_tests
	@$(MAKE) --no-print-directory jit-transfer-source-backend

jit-transfer-source-backend:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::corrupted_transfer_

jit-loop-source:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::loop_tests
	@$(MAKE) --no-print-directory jit-loop-source-backend

jit-loop-source-backend:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::corrupted_loop_

jit-comparison:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::comparison_tests
	@$(MAKE) --no-print-directory jit-comparison-backend

jit-comparison-backend:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::corrupted_comparison_

jit-truth:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::shape::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::corrupted_truth_
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::block_map_quota_refuses_before_host_setup_and_releases_storage

jit-arithmetic:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::corrupted_arithmetic_

jit-float-input:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::corrupted_float_

jit-access:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::access_tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::access::tests

jit-exits:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::exit_tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::exits::tests
	@$(MAKE) --no-print-directory jit-exit-flow
	@$(MAKE) --no-print-directory jit-entry-flow
	@$(MAKE) --no-print-directory jit-region-flow

jit-exit-flow:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::exit_flow::tests
	@$(MAKE) --no-print-directory jit-exit-flow-backend

jit-exit-flow-backend:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::corrupted_exit_flow_

jit-entry-flow:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::entry_flow::tests
	@$(MAKE) --no-print-directory jit-entry-flow-backend

jit-entry-flow-backend:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::corrupted_entry_flow_

.PHONY: jit-bound-entry
jit-bound-entry:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::entry_flow::bound:: -- --test-threads=1 | awk '/^test result: ok[.] 11 passed; 0 failed; 1 ignored;/ { valid=1 } { print } END { exit !valid }'

.PHONY: jit-bound-pair
jit-bound-pair:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::bound_pair:: -- --test-threads=1 | awk '/^test result: ok[.] 3 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::payload_runtime_tests::bound_entry:: -- --test-threads=1 | awk '/^test result: ok[.] 4 passed; 0 failed; 2 ignored;/ { valid=1 } { print } END { exit !valid }'

BOUND_ENTRY_DIR ?= target/jit-evidence/bound-entry
.PHONY: jit-bound-entry-profile-build jit-bound-entry-profile-run
jit-bound-entry-profile-build:
	@mkdir -p '$(BOUND_ENTRY_DIR)'
	@test ! -e '$(BOUND_ENTRY_DIR)/test-binary'
	@CARGO_PROFILE_RELEASE_OPT_LEVEL=3 CARGO_PROFILE_RELEASE_STRIP=false $(CARGO) test --locked --release -p luna --features jit --lib $(TARGET_ARG) --no-run --message-format=json > '$(BOUND_ENTRY_DIR)/build.jsonl'
	@jq -r 'select(.reason == "compiler-artifact" and .target.name == "luna" and .profile.test and .executable != null) | .executable' '$(BOUND_ENTRY_DIR)/build.jsonl' > '$(BOUND_ENTRY_DIR)/binary-path'
	@test "$$(wc -l < '$(BOUND_ENTRY_DIR)/binary-path')" -eq 1
	@cp "$$(cat '$(BOUND_ENTRY_DIR)/binary-path')" '$(BOUND_ENTRY_DIR)/test-binary'
	@sha256sum '$(BOUND_ENTRY_DIR)/test-binary' > '$(BOUND_ENTRY_DIR)/binary.sha256'
	@{ find src -type f -name '*.rs' -print0; printf '%s\0' Cargo.toml Cargo.lock Makefile examples/jit_support/workloads.rs examples/jit_support/callgrind_exclusive.awk; } | sort -z | xargs -0 sha256sum > '$(BOUND_ENTRY_DIR)/source.sha256'
	@{ rustc -vV; $(CARGO) --version; printf 'opt_level=3\nstrip=false\nscope=native-component-only\nwarmups=2\nmeasured_repetitions=1000\n'; } > '$(BOUND_ENTRY_DIR)/configuration'
	@set -o pipefail; '$(BOUND_ENTRY_DIR)/test-binary' jit::entry_flow::bound:: --test-threads=1 | tee '$(BOUND_ENTRY_DIR)/contracts.log' | awk '/^test result: ok[.] 11 passed; 0 failed; 1 ignored;/ { valid=1 } END { exit !valid }'

jit-bound-entry-profile-run: jit-profile-cpu-check
	@sha256sum -c '$(BOUND_ENTRY_DIR)/binary.sha256' '$(BOUND_ENTRY_DIR)/source.sha256' > '$(BOUND_ENTRY_DIR)/verification-before.log'
	@valgrind --version > '$(BOUND_ENTRY_DIR)/profiler.log'
	@set -e; for round in 0 1; do for bound in 0 1 2; do \
		out='$(BOUND_ENTRY_DIR)/'$$round-$$bound; test ! -e "$$out.callgrind"; \
		LUNA_BOUND_ENTRY=$$bound LUNA_JIT_DIAGNOSTIC_DIR="$$out-native" taskset -c '$(PROFILE_CPU)' valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*bound::native::profile::profile_execute*' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file="$$out.callgrind" \
			'$(BOUND_ENTRY_DIR)/test-binary' jit::entry_flow::bound::native::profile::profile_bound_resumption --ignored --exact --nocapture --test-threads=1 > "$$out.log" 2>&1; \
		grep -Fq "bound=$$bound repetitions=1000" "$$out.log"; \
		grep -Fq 'validated=true scope=native-component-only' "$$out.log"; \
		grep -Fq 'test result: ok. 1 passed; 0 failed' "$$out.log"; \
		grep -Eq '^summary: [1-9][0-9]*' "$$out.callgrind"; \
		awk -f examples/jit_support/callgrind_exclusive.awk "$$out.callgrind" > "$$out.exclusive.callgrind" 2> "$$out.exclusive.log"; \
	done; \
		sed -n 's/.*totals=\(\[[^]]*\]\).*/\1/p' '$(BOUND_ENTRY_DIR)/'$$round-0.log > '$(BOUND_ENTRY_DIR)/'$$round-0.counts; \
		sed -n 's/.*totals=\(\[[^]]*\]\).*/\1/p' '$(BOUND_ENTRY_DIR)/'$$round-1.log > '$(BOUND_ENTRY_DIR)/'$$round-1.counts; \
		test "$$(wc -l < '$(BOUND_ENTRY_DIR)/'$$round-0.counts)" -eq 1; \
		cmp '$(BOUND_ENTRY_DIR)/'$$round-0.counts '$(BOUND_ENTRY_DIR)/'$$round-1.counts; \
		sed -n 's/.*totals=\(\[[^]]*\]\).*/\1/p' '$(BOUND_ENTRY_DIR)/'$$round-2.log > '$(BOUND_ENTRY_DIR)/'$$round-2.counts; \
		cmp '$(BOUND_ENTRY_DIR)/'$$round-0.counts '$(BOUND_ENTRY_DIR)/'$$round-2.counts; \
	done
	@sha256sum -c '$(BOUND_ENTRY_DIR)/binary.sha256' '$(BOUND_ENTRY_DIR)/source.sha256' > '$(BOUND_ENTRY_DIR)/verification-after.log'

BOUND_PAIR_DIR ?= target/jit-evidence/bound-pair
.PHONY: jit-bound-pair-profile-build jit-bound-pair-profile-run
jit-bound-pair-profile-build:
	@$(MAKE) --no-print-directory jit-bound-entry-profile-build BOUND_ENTRY_DIR='$(BOUND_PAIR_DIR)'
	@{ rustc -vV; $(CARGO) --version; printf 'opt_level=3\nstrip=false\nscope=full-frozen-callback-runtime\nwarmups=2\nmeasured_executions=1\n'; } > '$(BOUND_PAIR_DIR)/configuration'
	@set -o pipefail; '$(BOUND_PAIR_DIR)/test-binary' jit::backend::bound_pair:: --test-threads=1 | tee '$(BOUND_PAIR_DIR)/pair-contracts.log' | awk '/^test result: ok[.] 3 passed; 0 failed;/ { valid=1 } END { exit !valid }'
	@set -o pipefail; '$(BOUND_PAIR_DIR)/test-binary' jit::payload_runtime_tests::bound_entry:: --test-threads=1 | tee '$(BOUND_PAIR_DIR)/runtime-contracts.log' | awk '/^test result: ok[.] 4 passed; 0 failed; 2 ignored;/ { valid=1 } END { exit !valid }'
	@printf '%s\n' '$(CURDIR)' > '$(BOUND_PAIR_DIR)/source-root'

jit-bound-pair-profile-run: jit-profile-cpu-check
	@sha256sum -c '$(BOUND_PAIR_DIR)/binary.sha256' '$(BOUND_PAIR_DIR)/source.sha256' > '$(BOUND_PAIR_DIR)/verification-before.log'
	@valgrind --version > '$(BOUND_PAIR_DIR)/profiler.log'
	@set -e; for round in 0 1 2 3; do for selected in 0 1; do \
		out='$(BOUND_PAIR_DIR)/'$$round-$$selected; test ! -e "$$out.callgrind"; \
		LUNA_BOUND_PAIR=$$selected LUNA_JIT_DIAGNOSTIC_DIR="$$out-native" taskset -c '$(PROFILE_CPU)' valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*bound_entry::profile_execute*' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file="$$out.callgrind" \
			'$(BOUND_PAIR_DIR)/test-binary' jit::payload_runtime_tests::bound_entry::profile_bound_pair_callback --ignored --exact --nocapture --test-threads=1 > "$$out.log" 2>&1; \
		grep -Fq "case=rust_callbacks pair=$$([ $$selected = 1 ] && echo true || echo false) validated=true" "$$out.log"; \
		grep -Fq 'test result: ok. 1 passed; 0 failed' "$$out.log"; \
		grep -Eq '^summary: [1-9][0-9]*' "$$out.callgrind"; \
		awk -f examples/jit_support/callgrind_exclusive.awk "$$out.callgrind" > "$$out.exclusive.callgrind" 2> "$$out.exclusive.log"; \
		sed -n 's/.*counters=\(\[[^]]*\]\).*/\1/p' "$$out.log" > "$$out.counts"; \
		test "$$(wc -l < "$$out.counts")" -eq 1; \
	done; cmp '$(BOUND_PAIR_DIR)/'$$round-0.counts '$(BOUND_PAIR_DIR)/'$$round-1.counts; done
	@sha256sum -c '$(BOUND_PAIR_DIR)/binary.sha256' '$(BOUND_PAIR_DIR)/source.sha256' > '$(BOUND_PAIR_DIR)/verification-after.log'

BOUND_PAIR_TIMING_IMAGE ?= target/jit-evidence/bound-pair-timing-image
BOUND_PAIR_TIMING_DIR ?= target/jit-evidence/bound-pair-timing
BOUND_PAIR_BASELINE ?=
BOUND_PAIR_WINDOWS ?= 204
.PHONY: jit-bound-pair-time-build jit-bound-pair-time-run jit-bound-pair-time-audit
jit-bound-pair-time-build:
	@$(MAKE) --no-print-directory jit-bound-pair-profile-build BOUND_PAIR_DIR='$(BOUND_PAIR_TIMING_IMAGE)'
	@sha256sum examples/jit_support/strided_timing.sh examples/jit_support/strided_timing_audit.pl tests/test-jit-strided-timing.pl >> '$(BOUND_PAIR_TIMING_IMAGE)/source.sha256'

jit-bound-pair-time-run: jit-profile-cpu-check
	@test -n '$(BOUND_PAIR_BASELINE)'
	@bash examples/jit_support/strided_timing.sh '$(BOUND_PAIR_TIMING_IMAGE)' '$(BOUND_PAIR_TIMING_DIR)' '$(PROFILE_CPU)' '$(BOUND_PAIR_WINDOWS)' bound_pair_baseline '$(BOUND_PAIR_BASELINE)'

jit-bound-pair-time-audit:
	@perl examples/jit_support/strided_timing_audit.pl '$(BOUND_PAIR_TIMING_DIR)'

jit-region-flow:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::entry_flow::region_tests
	@$(MAKE) --no-print-directory jit-region-flow-backend

jit-region-flow-backend:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::corrupted_region_flow_

jit-source-binding:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::ownership_tests
	@$(MAKE) --no-print-directory jit-source-binding-backend

jit-source-binding-backend:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::corrupted_binding_

jit-generic-for:
	@$(CARGO) test --locked -p luna --test vm_semantics $(TARGET_ARG) generic_for
	@set -e; for mode in off auto force; do \
		LUNA_TEST_JIT_MODE=$$mode $(CARGO) test --locked -p luna --features jit --test vm_semantics $(TARGET_ARG) generic_for; \
	done

jit-suspension:
	@$(CARGO) test --locked -p luna --features jit --test jit_suspension $(TARGET_ARG) $(ARGS)
	@$(CARGO) test --locked -p luna --features jit,async --test jit_suspension $(TARGET_ARG) $(ARGS)

.PHONY: jit-sequences
jit-sequences:
	@$(CARGO) test --locked -p luna --features jit --test jit_sequences $(TARGET_ARG) $(ARGS)
	@$(CARGO) test --locked -p luna --features jit,async --test jit_sequences $(TARGET_ARG) $(ARGS)

.PHONY: stdlib-tempfiles
stdlib-tempfiles:
	@$(CARGO) test --locked -p luna --test os_lib --test stdlib_gaps $(TARGET_ARG) $(ARGS)
	@$(CARGO) test --locked -p luna --lib $(TARGET_ARG) stdlib::tempfile::tests
	@for mode in off auto force; do \
		LUNA_TEST_JIT_MODE=$$mode $(CARGO) test --locked -p luna --features jit --test os_lib --test stdlib_gaps $(TARGET_ARG) $(ARGS) || exit $$?; \
	done

jit-errors:
	@$(CARGO) test --locked -p luna --features jit --test jit_errors $(TARGET_ARG)
	@$(CARGO) test --locked -p luna --features jit,async --test jit_errors $(TARGET_ARG)

.PHONY: jit-debug
.PHONY: hook-progress
.PHONY: vm-hook-resume vm-hook-resume-miri
vm-hook-resume:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::vm::hook_resume $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::vm::hook_resume $(ARGS)

vm-hook-resume-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' thread::vm::hook_resume -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::vm::hook_resume -- --test-threads=1

hook-progress:
	@$(CARGO) test --locked -p luna --no-default-features --test hook_progress $(TARGET_ARG) $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --test hook_progress $(TARGET_ARG) $(ARGS)

.PHONY: hook-progress-miri
.PHONY: hook-progress-builtin
hook-progress-builtin:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::executor::builtin_tests::builtin_persistent_hook_zero_fuel_trace_matches_through_completion -- --exact

hook-progress-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --test hook_progress --target '$(MIRI_TARGET)' -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --test hook_progress --target '$(MIRI_TARGET)' -- --test-threads=1

jit-debug:
	@$(CARGO) test --locked -p luna --features jit --test jit_debug $(TARGET_ARG) $(ARGS)
	@$(CARGO) test --locked -p luna --features jit,async --test jit_debug $(TARGET_ARG) $(ARGS)

.PHONY: stdlib-debug
stdlib-debug:
	@$(CARGO) test --locked -p luna --test debug_lib $(TARGET_ARG) $(ARGS)
	@set -e; for mode in off auto force; do \
		LUNA_TEST_JIT_MODE=$$mode $(CARGO) test --locked -p luna --features jit --test debug_lib $(TARGET_ARG) $(ARGS); \
	done

.PHONY: jit-gc-requests
jit-gc-requests:
	@$(CARGO) test --locked -p luna --test gc_control --test gc_pacing --test gc_finalizers $(TARGET_ARG) $(ARGS)
	@set -e; for mode in off auto force; do \
		LUNA_TEST_JIT_MODE=$$mode $(CARGO) test --locked -p luna --features jit --test gc_control --test gc_pacing --test gc_finalizers $(TARGET_ARG) $(ARGS); \
	done
	@$(CARGO) test --locked -p luna --features jit --test jit_gc_requests $(TARGET_ARG) $(ARGS)
	@$(CARGO) test --locked -p luna --features jit,async --test jit_gc_requests $(TARGET_ARG) $(ARGS)

jit-abi:
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::abi::tests

.PHONY: jit-phase-progress jit-phase-progress-miri
jit-phase-progress:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::vm_progress::tests

jit-phase-progress-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::vm_progress::tests -- --test-threads=1

.PHONY: jit-strided jit-strided-miri jit-strided-frame jit-strided-frame-miri
jit-strided-frame:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) strided_frame_ -- --test-threads=1

jit-strided-frame-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' strided_frame_ -- --test-threads=1

.PHONY: jit-strided-backend jit-strided-backend-miri jit-strided-runtime
jit-strided-backend:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::payload::strided:: -- --test-threads=1

jit-strided-backend-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::payload::strided:: -- --test-threads=1

jit-strided-runtime:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) strided_runtime_ -- --test-threads=1

.PHONY: jit-strided-runtime-nil
jit-strided-runtime-nil:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) strided_runtime_nil_fanout_ -- --test-threads=1

STRIDED_RUNTIME_DIR ?= target/jit-evidence/short-slice-performance/strided-runtime
MEMORY_RUNTIME_DIR ?= target/jit-evidence/short-slice-performance/universal-memory-runtime
FIELD_RUNTIME_DIR ?= target/jit-evidence/short-slice-performance/field-memory-runtime
MEMORY_KIND ?= universal
MEMORY_CASES ?= integer_loop float_loop array_table closure_upvalue polymorphic_metamethod rust_callbacks allocation_gc
MEMORY_DUMP ?= 0
.PHONY: jit-field-memory jit-field-memory-miri jit-field-memory-profile-build jit-field-memory-profile-run
jit-field-memory:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) field_memory -- --test-threads=1

jit-field-memory-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::field_memory:: -- --test-threads=1

jit-field-memory-profile-build:
	@$(MAKE) jit-universal-memory-profile-build MEMORY_KIND=field MEMORY_RUNTIME_DIR='$(FIELD_RUNTIME_DIR)'

jit-field-memory-profile-run:
	@$(MAKE) jit-universal-memory-profile-run MEMORY_KIND=field MEMORY_RUNTIME_DIR='$(FIELD_RUNTIME_DIR)'

.PHONY: jit-universal-memory jit-universal-memory-miri jit-universal-memory-profile-build jit-universal-memory-profile-run
jit-universal-memory:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) universal_memory -- --test-threads=1

jit-universal-memory-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::universal_memory:: -- --test-threads=1

jit-universal-memory-profile-build:
	@$(MAKE) jit-strided-profile-build STRIDED_DIR='$(MEMORY_RUNTIME_DIR)'
	@{ rustc -vV; $(CARGO) --version; printf 'opt_level=3\nstrip=false\nscope=full frozen workload execution\nwarmups=2\nmeasured_executions=1\nexperiment=$(MEMORY_KIND) memory regions\n'; } > '$(MEMORY_RUNTIME_DIR)/configuration'

jit-universal-memory-profile-run: jit-profile-cpu-check
	@sha256sum -c '$(MEMORY_RUNTIME_DIR)/binary.sha256' '$(MEMORY_RUNTIME_DIR)/source.sha256' > '$(MEMORY_RUNTIME_DIR)/verification-before.log'
	@set -e; for round in 0 1; do order='0 1'; if [ $$round = 1 ]; then order='1 0'; fi; for shape in $(MEMORY_CASES); do for selected in $$order; do \
		out='$(MEMORY_RUNTIME_DIR)'/$$round-$$shape-$$selected; test ! -e "$$out.callgrind"; \
		$(if $(filter 1,$(MEMORY_DUMP)),LUNA_JIT_DIAGNOSTIC_DIR="$$out-native",) LUNA_MEMORY_CASE=$$shape LUNA_MEMORY_SELECTED=$$selected taskset -c '$(PROFILE_CPU)' valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*strided_profile::profile_execute*' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file="$$out.callgrind" \
			'$(MEMORY_RUNTIME_DIR)/test-binary' jit::payload_runtime_tests::strided_profile::profile_$(MEMORY_KIND)_memory_runtime --ignored --exact --nocapture --test-threads=1 > "$$out.log" 2>&1; \
		grep -Fq "case=$$shape $(MEMORY_KIND)_memory=$$([ $$selected = 1 ] && echo true || echo false) validated=true" "$$out.log"; \
		grep -Fq 'test result: ok. 1 passed; 0 failed' "$$out.log"; \
		grep -Eq '^summary: [1-9][0-9]*' "$$out.callgrind"; \
	done; done; done
	@sha256sum -c '$(MEMORY_RUNTIME_DIR)/binary.sha256' '$(MEMORY_RUNTIME_DIR)/source.sha256' > '$(MEMORY_RUNTIME_DIR)/verification-after.log'

STRIDED_CASES ?= rust_callbacks closure_upvalue integer_loop float_loop
STRIDED_ROUNDS ?= 0 1
STRIDED_DUMP ?= 0
STRIDED_TIMING_DIR ?= target/jit-evidence/short-slice-performance/strided-timing
STRIDED_TIMING_WINDOWS ?= 21
.PHONY: jit-strided-runtime-timing jit-strided-timing-tests jit-strided-runtime-time-run jit-strided-runtime-time-audit
jit-strided-runtime-timing:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::payload_runtime_tests::strided_profile::timing:: -- --test-threads=1

jit-strided-timing-tests:
	@bash -n examples/jit_support/strided_timing.sh
	@perl -c examples/jit_support/strided_timing_audit.pl
	@perl tests/test-jit-strided-timing.pl

jit-strided-runtime-time-run: jit-profile-cpu-check
	@bash examples/jit_support/strided_timing.sh '$(STRIDED_RUNTIME_DIR)' '$(STRIDED_TIMING_DIR)' '$(PROFILE_CPU)' '$(STRIDED_TIMING_WINDOWS)'

jit-strided-runtime-time-audit:
	@perl examples/jit_support/strided_timing_audit.pl '$(STRIDED_TIMING_DIR)'

.PHONY: jit-strided-runtime-profile-build jit-strided-runtime-profile-run
jit-strided-runtime-profile-build:
	@$(MAKE) jit-strided-profile-build STRIDED_DIR='$(STRIDED_RUNTIME_DIR)'
	@printf 'scope=full frozen workload execution\nwarmups=2\nmeasured_executions=1\n' >> '$(STRIDED_RUNTIME_DIR)/configuration'

jit-strided-runtime-profile-run: jit-profile-cpu-check
	@sha256sum -c '$(STRIDED_RUNTIME_DIR)/binary.sha256' '$(STRIDED_RUNTIME_DIR)/source.sha256' > '$(STRIDED_RUNTIME_DIR)/verification-before.log'
	@set -e; for round in $(STRIDED_ROUNDS); do for shape in $(STRIDED_CASES); do for strided in 0 1; do \
		out='$(STRIDED_RUNTIME_DIR)'/$$round-$$shape-$$strided; test ! -e "$$out.callgrind"; \
		$(if $(filter 1,$(STRIDED_DUMP)),LUNA_JIT_DIAGNOSTIC_DIR="$$out-native",) LUNA_STRIDED_CASE=$$shape LUNA_STRIDED=$$strided taskset -c '$(PROFILE_CPU)' valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*strided_profile::profile_execute*' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file="$$out.callgrind" \
			'$(STRIDED_RUNTIME_DIR)/test-binary' jit::payload_runtime_tests::strided_profile::profile_strided_runtime --ignored --exact --nocapture --test-threads=1 > "$$out.log" 2>&1; \
		grep -Fq "case=$$shape strided=$$([ $$strided = 1 ] && echo true || echo false) validated=true" "$$out.log"; \
		grep -Fq 'test result: ok. 1 passed; 0 failed' "$$out.log"; \
		grep -Eq '^summary: [1-9][0-9]*' "$$out.callgrind"; \
	done; done; done
	@sha256sum -c '$(STRIDED_RUNTIME_DIR)/binary.sha256' '$(STRIDED_RUNTIME_DIR)/source.sha256' > '$(STRIDED_RUNTIME_DIR)/verification-after.log'
	@$(if $(filter 1,$(STRIDED_DUMP)),$(MAKE) --no-print-directory jit-strided-runtime-disassemble,:)

.PHONY: jit-strided-runtime-disassemble
jit-strided-runtime-disassemble:
	@set -e; case "$$(uname -m)" in x86_64) machine='i386:x86-64';; aarch64) machine=aarch64;; *) exit 2;; esac; \
	for metadata in '$(STRIDED_RUNTIME_DIR)'/*-native/kernel-*.metadata; do \
		grep -Fxq "arch=$$(uname -m)" "$$metadata"; \
		address=$$(sed -n 's/^entry_address=//p' "$$metadata"); test -n "$$address"; \
		binary=$${metadata%.metadata}.bin; test -s "$$binary"; \
		objdump -D -b binary -m "$$machine" --adjust-vma="$$address" "$$binary" > "$${metadata%.metadata}.asm"; \
		grep -Eq '^[[:space:]]*[[:xdigit:]]+:[[:space:]]' "$${metadata%.metadata}.asm"; \
		sha256sum "$$binary" "$$metadata" > "$${metadata%.metadata}.sha256"; \
	done

STRIDED_DIR ?= target/jit-evidence/short-slice-performance/strided
jit-strided:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::strided:: -- --test-threads=1 | awk '/^test result: ok[.] 16 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-strided-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::strided:: -- --test-threads=1 | awk '/^test result: ok[.] 12 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

.PHONY: jit-strided-prepared jit-strided-prepared-miri
jit-strided-prepared:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) strided_prepared_ -- --test-threads=1 | awk '/^test result: ok[.] 2 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-strided-prepared-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' strided_prepared_ -- --test-threads=1 | awk '/^test result: ok[.] 1 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

STRIDED_PREPARED_DIR ?= target/jit-evidence/short-slice-performance/strided-prepared
.PHONY: jit-strided-prepared-profile-build jit-strided-prepared-profile-run
jit-strided-prepared-profile-build:
	@$(MAKE) jit-strided-profile-build STRIDED_DIR='$(STRIDED_PREPARED_DIR)'

jit-strided-prepared-profile-run: jit-profile-cpu-check
	@sha256sum -c '$(STRIDED_PREPARED_DIR)/binary.sha256' '$(STRIDED_PREPARED_DIR)/source.sha256' > '$(STRIDED_PREPARED_DIR)/verification-before.log'
	@valgrind --version > '$(STRIDED_PREPARED_DIR)/profiler.log'
	@set -e; for round in 0 1; do for shape in scalar_lookup callback_lookup; do for strided in 0 1; do \
		out='$(STRIDED_PREPARED_DIR)'/$$round-$$shape-$$strided; test ! -e "$$out.callgrind"; \
		LUNA_STRIDED_SHAPE=$$shape LUNA_STRIDED=$$strided taskset -c '$(PROFILE_CPU)' valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*strided::transition::prepared::probe::profile_calls*' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file="$$out.callgrind" \
			'$(STRIDED_PREPARED_DIR)/test-binary' jit::abi::strided::transition::prepared::probe::profile_prepared_transport --ignored --exact --nocapture --test-threads=1 > "$$out.log" 2>&1; \
		grep -Fq "shape=$$shape strided=$$([ $$strided = 1 ] && echo true || echo false) calls=100000 completed=100000 callback_invocations=0 validated=true" "$$out.log"; \
		grep -Fq 'test result: ok. 1 passed; 0 failed' "$$out.log"; \
		grep -Eq '^summary: [1-9][0-9]*' "$$out.callgrind"; \
		callgrind_annotate --inclusive=no --threshold=99 "$$out.callgrind" > "$$out-annotation.log"; \
	done; done; done
	@sha256sum -c '$(STRIDED_PREPARED_DIR)/binary.sha256' '$(STRIDED_PREPARED_DIR)/source.sha256' > '$(STRIDED_PREPARED_DIR)/verification-after.log'

.PHONY: jit-strided-helpers jit-strided-helpers-miri
jit-strided-helpers:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) strided_helpers_ -- --test-threads=1 | awk '/^test result: ok[.] 6 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-strided-helpers-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' strided_helpers_ -- --test-threads=1 | awk '/^test result: ok[.] 4 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

.PHONY: jit-strided-profile-build jit-strided-profile-run
jit-strided-profile-build:
	@mkdir -p '$(STRIDED_DIR)'
	@test ! -e '$(STRIDED_DIR)/test-binary'
	@CARGO_PROFILE_RELEASE_OPT_LEVEL=3 CARGO_PROFILE_RELEASE_STRIP=false $(CARGO) test --locked --release -p luna --features jit --lib $(TARGET_ARG) --no-run --message-format=json > '$(STRIDED_DIR)/build.jsonl'
	@jq -r 'select(.reason == "compiler-artifact" and .target.name == "luna" and .profile.test and .executable != null) | .executable' '$(STRIDED_DIR)/build.jsonl' > '$(STRIDED_DIR)/binary-path'
	@test "$$(wc -l < '$(STRIDED_DIR)/binary-path')" -eq 1
	@cp "$$(cat '$(STRIDED_DIR)/binary-path')" '$(STRIDED_DIR)/test-binary'
	@sha256sum '$(STRIDED_DIR)/test-binary' > '$(STRIDED_DIR)/binary.sha256'
	@{ find src -type f -name '*.rs' -print0; printf '%s\0' Cargo.toml Cargo.lock Makefile examples/jit_support/workloads.rs examples/jit_support/resource_workloads.rs; } | sort -z | xargs -0 sha256sum > '$(STRIDED_DIR)/source.sha256'
	@{ rustc -vV; $(CARGO) --version; printf 'opt_level=3\nstrip=false\ncalls=100000\nscope=synthetic transport only\n'; } > '$(STRIDED_DIR)/configuration'

jit-strided-profile-run: jit-profile-cpu-check
	@sha256sum -c '$(STRIDED_DIR)/binary.sha256' '$(STRIDED_DIR)/source.sha256' > '$(STRIDED_DIR)/verification-before.log'
	@valgrind --version > '$(STRIDED_DIR)/profiler.log'
	@set -e; for shape in single mixed7 dense7 mixed16 dense16 mixed256 dense256; do for strided in 0 1; do \
		test ! -e '$(STRIDED_DIR)'/$$shape-$$strided.callgrind; \
		LUNA_STRIDED_SHAPE=$$shape LUNA_STRIDED=$$strided taskset -c '$(PROFILE_CPU)' valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*strided::native::probe::profile_transport*' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file='$(STRIDED_DIR)'/$$shape-$$strided.callgrind \
			'$(STRIDED_DIR)/test-binary' jit::abi::strided::native::probe::profile_generated_transport --ignored --exact --nocapture --test-threads=1 > '$(STRIDED_DIR)'/$$shape-$$strided.log 2>&1; \
		grep -Fq "shape=$$shape strided=$$([ $$strided = 1 ] && echo true || echo false) calls=100000 completed=100000 validated=true" '$(STRIDED_DIR)'/$$shape-$$strided.log; \
		grep -Fq 'test result: ok. 1 passed; 0 failed' '$(STRIDED_DIR)'/$$shape-$$strided.log; \
		grep -Eq '^summary: [1-9][0-9]*' '$(STRIDED_DIR)'/$$shape-$$strided.callgrind; \
		callgrind_annotate --inclusive=no --threshold=99 '$(STRIDED_DIR)'/$$shape-$$strided.callgrind > '$(STRIDED_DIR)'/$$shape-$$strided-annotation.log; \
	done; done
	@sha256sum -c '$(STRIDED_DIR)/binary.sha256' '$(STRIDED_DIR)/source.sha256' > '$(STRIDED_DIR)/verification-after.log'

.PHONY: jit-reference-import jit-reference-import-miri
jit-reference-import:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::tests::borrowed_import_matches_owned_values_and_current_mutations -- --list | grep -F 'borrowed_import_matches_owned_values_and_current_mutations: test'
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::tests::borrowed_import_matches_owned_values_and_current_mutations -- --exact

jit-reference-import-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::tests::borrowed_import_matches_owned_values_and_current_mutations -- --exact --test-threads=1 | awk '/^test result: ok[.] 1 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

.PHONY: jit-same-integer-miri
jit-same-integer-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::tests::same_integer_writeback_preserves_bits_references_and_refusals -- --exact --test-threads=1

.PHONY: jit-tag-encoding jit-tag-encoding-miri jit-tag-encoding-codegen
TAG_ENCODING_DIR ?= target/jit-evidence/tag-encoding
jit-tag-encoding:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::tag_encoding:: $(ARGS)

jit-tag-encoding-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::tag_encoding:: -- --test-threads=1 $(ARGS)

jit-tag-encoding-codegen:
	@mkdir -p '$(TAG_ENCODING_DIR)'
	@CARGO_PROFILE_RELEASE_OPT_LEVEL=3 CARGO_PROFILE_RELEASE_STRIP=false $(CARGO) test --locked --release -p luna --features jit --lib $(TARGET_ARG) --no-run --message-format=json > '$(TAG_ENCODING_DIR)/build.json'
	@jq -er 'select(.reason == "compiler-artifact" and .target.name == "luna" and .profile.test and .executable != null) | .executable' '$(TAG_ENCODING_DIR)/build.json' > '$(TAG_ENCODING_DIR)/executable'
	@test "$$(wc -l < '$(TAG_ENCODING_DIR)/executable')" -eq 1
	@cp "$$(cat '$(TAG_ENCODING_DIR)/executable')" '$(TAG_ENCODING_DIR)/probe'
	@set -o pipefail; '$(TAG_ENCODING_DIR)/probe' jit::abi::tag_encoding:: 2>&1 | tee '$(TAG_ENCODING_DIR)/tests.log'
	@set -o pipefail; objdump -Cd '$(TAG_ENCODING_DIR)/probe' | awk '/<luna::jit::abi::tag_encoding::(original_import|offset_import|split_import|integer_prefix_import)>:/ { emit=1 } emit { print } /^$$/ { emit=0 }' > '$(TAG_ENCODING_DIR)/assembly.log'
	@grep -Fq '<luna::jit::abi::tag_encoding::original_import>:' '$(TAG_ENCODING_DIR)/assembly.log'
	@grep -Fq '<luna::jit::abi::tag_encoding::offset_import>:' '$(TAG_ENCODING_DIR)/assembly.log'
	@grep -Fq '<luna::jit::abi::tag_encoding::split_import>:' '$(TAG_ENCODING_DIR)/assembly.log'
	@grep -Fq '<luna::jit::abi::tag_encoding::integer_prefix_import>:' '$(TAG_ENCODING_DIR)/assembly.log'
	@sha256sum '$(TAG_ENCODING_DIR)/probe' > '$(TAG_ENCODING_DIR)/binary.sha256'

.PHONY: jit-return-words jit-return-words-miri
.PHONY: jit-output-buffer jit-output-buffer-miri
jit-output-buffer:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::exit_buffer::
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::return_words::native_output_entry_
	@$(MAKE) --no-print-directory jit-exit-flow

jit-output-buffer-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::exit_buffer:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::exit_flow::tests -- --test-threads=1

jit-return-words:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::return_words::
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::return_words::
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::exit_transport::

jit-return-words-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::return_words:: -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::exit_transport:: -- --test-threads=1

.PHONY: jit-import-bits jit-import-bits-miri
jit-import-bits:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::tests::scalar_import_preserves_generated_payload_bits -- --exact

jit-import-bits-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::tests::scalar_import_preserves_generated_payload_bits -- --exact --test-threads=1

.PHONY: jit-rooted-moves jit-rooted-preservation
.PHONY: jit-readonly-lookup jit-readonly-lookup-miri
.PHONY: jit-readonly-identity jit-readonly-identity-miri
.PHONY: jit-repro-wrapper-tests jit-repro-images jit-repro-build-tests
JIT_REPRO_SOURCE ?= $(TOP_DIR)
JIT_REPRO_WRAPPER ?= $(TOP_DIR)/examples/jit_support/rustc_repro.sh
JIT_REPRO_HASH = $(shell sha256sum '$(JIT_REPRO_WRAPPER)' examples/jit_support/build_repro.sh | sha256sum | cut -d ' ' -f1)
JIT_REPRO_TARGET_DIR ?= $(TOP_DIR)/target/jit-repro/$(JIT_REPRO_HASH)/$(JIT_BENCH_OPT)$(if $(TARGET),-$(TARGET),)
JIT_REPRO_DIR ?= $(TOP_DIR)/target/jit-evidence/repro-images/$(JIT_REPRO_HASH)/$(JIT_BENCH_OPT)$(if $(TARGET),-$(TARGET),)
jit-repro-images:
	@bash examples/jit_support/build_repro.sh '$(CARGO)' '$(JIT_REPRO_SOURCE)' '$(abspath $(JIT_REPRO_TARGET_DIR))' '$(abspath $(JIT_REPRO_DIR))' '$(JIT_BENCH_OPT)' '$(TARGET)' '$(JIT_REPRO_WRAPPER)'

jit-repro-build-tests:
	@bash tests/test-jit-repro-build.sh '$(SHELL)'

jit-repro-wrapper-tests:
	@bash tests/test-jit-rustc-repro.sh

jit-readonly-identity:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::readonly_lookup::tests::identity::tests $(ARGS) | awk '/^test result: ok[.] 2 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-readonly-identity-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::readonly_lookup::tests::identity::tests -- --test-threads=1 | awk '/^test result: ok[.] 2 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

.PHONY: jit-readonly-native
.PHONY: jit-readonly-runtime
.PHONY: jit-scoped-read-production
jit-scoped-read-production:
	@$(CARGO) test --locked -p luna --features jit --test jit_scoped_read $(TARGET_ARG) $(ARGS)

jit-readonly-runtime:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::readonly_lookup::runtime::tests $(ARGS)

.PHONY: jit-call-fusion-fuel jit-call-fusion-fuel-miri
.PHONY: jit-call-fusion-native
.PHONY: jit-call-fusion-runtime
.PHONY: jit-call-fusion-publication
jit-call-fusion-publication: jit-call-fusion-runtime
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::activation::fused_region $(ARGS)

jit-call-fusion-runtime:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::call_fusion::runtime $(ARGS)

.PHONY: jit-call-fusion-window jit-call-fusion-time-build jit-call-fusion-time-run
FUSION_TIMING_DIR ?= target/jit-evidence/fusion-timing
FUSION_TIMING_OPT ?= 3
FUSION_TIMING_CASE ?= closure_upvalue
FUSION_TIMING_MODE ?= fusion
FUSION_TIMING_ITERATIONS ?= 20
jit-call-fusion-window:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::call_fusion::runtime::timing::frozen_windows $(ARGS)

jit-call-fusion-time-build:
	@$(MAKE) jit-readonly-time-build READONLY_TIMING_DIR='$(FUSION_TIMING_DIR)' READONLY_TIMING_OPT='$(FUSION_TIMING_OPT)'

jit-call-fusion-time-run: jit-profile-cpu-check
	@LUNA_FUSION_CASE='$(FUSION_TIMING_CASE)' LUNA_FUSION_MODE='$(FUSION_TIMING_MODE)' LUNA_FUSION_ITERATIONS='$(FUSION_TIMING_ITERATIONS)' taskset -c '$(PROFILE_CPU)' '$(FUSION_TIMING_DIR)/test-binary' jit::call_fusion::runtime::timing::time_frozen_runtime --exact --ignored --nocapture --test-threads=1

jit-call-fusion-native:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::call_fusion::site::kernel $(ARGS)

.PHONY: jit-strong-global-helper jit-strong-global-helper-miri
.PHONY: jit-frame-publication jit-frame-publication-miri
.PHONY: jit-lookup-publication jit-lookup-publication-miri
.PHONY: jit-canonical-resume jit-canonical-resume-miri
jit-canonical-resume:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) integer_resume $(ARGS)

jit-canonical-resume-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' integer_resume -- --test-threads=1

jit-lookup-publication:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::readonly_lookup::scalar_entry::publication::tests $(ARGS)

jit-lookup-publication-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::readonly_lookup::scalar_entry::publication::tests -- --test-threads=1

jit-frame-publication:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::frame_publication $(ARGS)

jit-frame-publication-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::frame_publication -- --test-threads=1

jit-strong-global-helper:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helpers::strong_global_tests $(ARGS)

jit-strong-global-helper-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::helpers::strong_global_tests -- --test-threads=1

jit-call-fusion-fuel:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::call_fusion $(ARGS)

jit-call-fusion-fuel-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::call_fusion -- --test-threads=1

.PHONY: jit-readonly-publication jit-readonly-publication-miri
jit-readonly-publication:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::readonly_lookup::publication::tests $(ARGS)

jit-readonly-publication-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::readonly_lookup::publication::tests -- --test-threads=1

.PHONY: jit-readonly-window jit-readonly-time-build jit-readonly-time-run
READONLY_TIMING_DIR ?= target/jit-evidence/readonly-timing
READONLY_TIMING_OPT ?= 3
READONLY_TIMING_CASE ?= rust_callbacks
READONLY_TIMING_MODE ?= ordinary
READONLY_TIMING_ITERATIONS ?= 20
jit-readonly-window:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::readonly_lookup::runtime::timing::frozen_workload_windows $(ARGS)

.PHONY: jit-readonly-profile-tests jit-readonly-profile-build
jit-readonly-profile-tests:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::readonly_lookup::runtime::profile $(ARGS)

jit-readonly-profile-build:
	@mkdir -p '$(READONLY_TIMING_DIR)'
	@test ! -e '$(READONLY_TIMING_DIR)/test-binary'
	@CARGO_PROFILE_RELEASE_OPT_LEVEL='$(READONLY_TIMING_OPT)' CARGO_PROFILE_RELEASE_LTO=true CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=false $(CARGO) test --locked --release -p luna --features jit --lib $(TARGET_ARG) --no-run --message-format=json > '$(READONLY_TIMING_DIR)/build.jsonl'
	@jq -r 'select(.reason == "compiler-artifact" and .target.name == "luna" and .profile.test and .executable != null) | .executable' '$(READONLY_TIMING_DIR)/build.jsonl' > '$(READONLY_TIMING_DIR)/binary-path'
	@test "$$(wc -l < '$(READONLY_TIMING_DIR)/binary-path')" -eq 1
	@cp "$$(cat '$(READONLY_TIMING_DIR)/binary-path')" '$(READONLY_TIMING_DIR)/test-binary'
	@sha256sum '$(READONLY_TIMING_DIR)/test-binary' > '$(READONLY_TIMING_DIR)/binary.sha256'
	@{ find src -type f -name '*.rs' -print0; printf '%s\0' Cargo.toml Cargo.lock Makefile examples/jit_support/workloads.rs examples/jit_support/profile_control.rs; } | sort -z | xargs -0 sha256sum > '$(READONLY_TIMING_DIR)/source.sha256'
	@{ rustc -vV; $(CARGO) --version; printf 'opt_level=$(READONLY_TIMING_OPT)\nlto=true\ncodegen_units=1\nstrip=false\nscope=warmed execution lifecycle attribution; not elapsed acceptance\nwarmups=2 plus compilation drain\n'; } > '$(READONLY_TIMING_DIR)/configuration'

jit-readonly-time-build:
	@mkdir -p '$(READONLY_TIMING_DIR)'
	@test ! -e '$(READONLY_TIMING_DIR)/test-binary'
	@CARGO_PROFILE_RELEASE_OPT_LEVEL='$(READONLY_TIMING_OPT)' CARGO_PROFILE_RELEASE_LTO=true CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=true $(CARGO) test --locked --release -p luna --features jit --lib $(TARGET_ARG) --no-run --message-format=json > '$(READONLY_TIMING_DIR)/build.jsonl'
	@jq -r 'select(.reason == "compiler-artifact" and .target.name == "luna" and .profile.test and .executable != null) | .executable' '$(READONLY_TIMING_DIR)/build.jsonl' > '$(READONLY_TIMING_DIR)/binary-path'
	@test "$$(wc -l < '$(READONLY_TIMING_DIR)/binary-path')" -eq 1
	@cp "$$(cat '$(READONLY_TIMING_DIR)/binary-path')" '$(READONLY_TIMING_DIR)/test-binary'
	@sha256sum '$(READONLY_TIMING_DIR)/test-binary' > '$(READONLY_TIMING_DIR)/binary.sha256'
	@{ find src -type f -name '*.rs' -print0; printf '%s\0' Cargo.toml Cargo.lock Makefile examples/jit_support/workloads.rs; } | sort -z | xargs -0 sha256sum > '$(READONLY_TIMING_DIR)/source.sha256'
	@{ rustc -vV; $(CARGO) --version; printf 'opt_level=$(READONLY_TIMING_OPT)\nlto=true\ncodegen_units=1\nstrip=true\nscope=diagnostic test runtime; not production acceptance\nwarmups=2\n'; } > '$(READONLY_TIMING_DIR)/configuration'

jit-readonly-time-run: jit-profile-cpu-check
	@LUNA_READONLY_CASE='$(READONLY_TIMING_CASE)' LUNA_READONLY_MODE='$(READONLY_TIMING_MODE)' LUNA_READONLY_ITERATIONS='$(READONLY_TIMING_ITERATIONS)' taskset -c '$(PROFILE_CPU)' '$(READONLY_TIMING_DIR)/test-binary' jit::readonly_lookup::runtime::timing::time_frozen_runtime --exact --ignored --nocapture --test-threads=1

jit-readonly-native:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::readonly_lookup::tests $(ARGS)

jit-readonly-lookup:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::readonly_lookup::tests $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::readonly_lookup::scalar_entry $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::readonly_lookup::binding::tests $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) table::raw::code_read_hint_tests::strong_peek $(ARGS)

jit-readonly-lookup-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::readonly_lookup::tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::readonly_lookup::scalar_entry -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::readonly_lookup::binding::tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' table::raw::code_read_hint_tests::strong_peek -- --test-threads=1

.PHONY: jit-root-kind-validation jit-root-kind-validation-miri
jit-root-kind-validation:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::roots::tests::reference_slot_validation_classifies_all_root_kinds -- --exact

jit-root-kind-validation-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::roots::tests::reference_slot_validation_classifies_all_root_kinds -- --exact --test-threads=1

jit-rooted-moves:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::roots::tests $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::rooted_moves_tests $(ARGS)

jit-rooted-preservation: jit-rooted-moves
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::model::tests $(ARGS)

.PHONY: jit-rooted-moves-miri
jit-rooted-moves-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::roots::tests -- --test-threads=1

jit-config:
	@$(CARGO) test -p luna --features jit --test jit_config $(TARGET_ARG) $(ARGS)

.PHONY: jit-test-modes
jit-test-modes:
	@$(CARGO) test --locked -p luna --features jit --test jit_test_modes $(TARGET_ARG) $(ARGS)
	@LUNA_TEST_JIT_MODE=force $(CARGO) test --locked -p luna --features jit --test scripts --test strings $(TARGET_ARG) $(ARGS)

.PHONY: jit-stats
jit-stats:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::stats_tests
	@$(CARGO) test --locked -p luna --features jit --test jit_dispatches $(TARGET_ARG)
	@$(CARGO) test --locked -p luna --features jit --test jit_fallback $(TARGET_ARG)
	@$(MAKE) --no-print-directory jit-native jit-heap

.PHONY: jit-stats-miri
.PHONY: jit-caller-shadow jit-caller-shadow-miri
jit-caller-shadow:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::caller_shadow:: $(ARGS)

jit-caller-shadow-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::caller_shadow:: -- --test-threads=1

jit-stats-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::stats_tests -- --test-threads=1

.PHONY: stdlib-portability jit-fallback
stdlib-portability:
	@$(CARGO) test --locked -p luna --lib $(TARGET_ARG) string::tests
	@$(CARGO) test --locked -p luna --test hardening --test numeric_semantics --test string_pack --test string_semantics --test strings $(TARGET_ARG) $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --test hardening --test numeric_semantics --test string_pack --test string_semantics --test strings $(TARGET_ARG) $(ARGS)

jit-fallback:
	@test "$(TARGET)" = i686-unknown-linux-musl || { echo 'Use TARGET=i686-unknown-linux-musl for the unsupported-target gate' >&2; exit 2; }
	@$(CARGO) test --locked -p luna --features jit --test jit_fallback $(TARGET_ARG)
	@$(CARGO) test --locked -p luna --all-features --test jit_fallback $(TARGET_ARG)
	@$(MAKE) test TARGET=$(TARGET)
	@$(MAKE) jit-test JIT_MODE=off TARGET=$(TARGET)
	@$(MAKE) jit-test JIT_MODE=auto TARGET=$(TARGET)
	@$(MAKE) jit-test-all JIT_MODE=force TARGET=$(TARGET)

jit-native:
	@$(CARGO) test -p luna --features jit --test jit_native $(TARGET_ARG) $(ARGS)

jit-numeric:
	@$(CARGO) test -p luna --test numeric_semantics $(TARGET_ARG) $(ARGS)
	@$(CARGO) test -p luna --features jit --test jit_native $(TARGET_ARG) mixed_numeric
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::model::tests::mixed_numeric
	@$(MAKE) --no-print-directory jit-numeric-exits

.PHONY: jit-numeric-exits
jit-numeric-exits:
	@$(CARGO) test --locked -p luna --features jit --test jit_numeric_exits $(TARGET_ARG) $(ARGS)

jit-fuzz-smoke:
	@case "$$(uname -sm)" in 'Linux x86_64'|'Linux aarch64') ;; *) echo 'Native fuzz requires supported Linux host'; exit 2;; esac
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::fuzz::supervisor_rejects_panics_signals_and_timeouts -- --exact --ignored --nocapture
	@$(MAKE) --no-print-directory jit-fuzz FUZZ_CASES=24
	@$(MAKE) --no-print-directory jit-fuzz-heap FUZZ_CASES=8

jit-fuzz-heap: FUZZ_CASES = 24
jit-fuzz-heap:
	@$(MAKE) --no-print-directory jit-fuzz FUZZ_TARGET=heap FUZZ_CASES='$(FUZZ_CASES)'

jit-heap-campaign-tests:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::fuzz::heap::

jit-owner:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::owner::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::eviction_tests

jit-owner-installation:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::eviction_tests::cached_owner_refusal_preserves_peer_and_refused_source_then_recovers -- --exact

jit-owner-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::owner::tests -- --test-threads=1

jit-memory-status: jit-memory-status-pure jit-memory-status-native

jit-memory-status-pure:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::atomic_owner::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::memory_status::tests
	@$(MAKE) --no-print-directory jit-memory-status-boundary

jit-memory-status-boundary:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::status_refusal_precedes_host_setup_and_releases_entry_storage -- --exact

jit-memory-status-native:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::status_refusal_preserves_live_module_and_same_snapshot_recovers -- --exact

jit-memory-status-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::atomic_owner::tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::memory_status::tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::memory_tests::status_refusal_precedes_host_setup_and_releases_entry_storage -- --exact --test-threads=1

jit-atomic-owner-ordering-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::atomic_owner::tests::final_drop_acquires_writes_from_other_owners_without_external_sync -- --exact --test-threads=1

jit-provider-box: jit-provider-box-pure jit-provider-box-native

jit-provider-box-pure:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::global_box::tests
	@$(MAKE) --no-print-directory jit-provider-box-boundary

jit-provider-box-boundary:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::provider_box_refusal_precedes_host_setup_and_releases_entry_storage -- --exact

jit-provider-box-admission:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::global_box::tests::child_parent_underlying_refusal_drop_input_and_restore_charges -- --exact

jit-provider-box-native:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::provider_box_refusal_preserves_live_module_and_same_snapshot_recovers -- --exact

jit-provider-box-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::global_box::tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::memory_tests::provider_box_refusal_precedes_host_setup_and_releases_entry_storage -- --exact --test-threads=1

jit-handoff:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::handoff::tests

jit-handoff-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::handoff::tests -- --test-threads=1

jit-image:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests
	@$(MAKE) --no-print-directory jit-backend jit-resources jit-host-native

jit-image-retention:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::finalized_image_retains_only_charged_runtime_storage -- --exact

jit-frontend-arrays:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::arrays::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::helper_flow_tests
	@$(MAKE) --no-print-directory jit-helpers jit-backend

jit-frontend-arrays-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::arrays::tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::helper_flow_tests -- --test-threads=1

jit-runtime-owner: jit-runtime-owner-pure jit-host-memory jit-mapping-counter

jit-runtime-owner-pure:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::runtime_owner_tests

jit-runtime-owner-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::runtime_owner_tests -- --test-threads=1

jit-ledger-owner: jit-ledger-owner-pure jit-host-memory jit-mapping-counter

jit-ledger-owner-pure:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::global_owner::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::resources::bootstrap_tests

jit-ledger-owner-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::global_owner::tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::resources::bootstrap_tests -- --test-threads=1

jit-mapping-counter: jit-mapping-counter-pure jit-mapping-counter-native

jit-mapping-counter-pure:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::resources::mapping_tests

jit-mapping-counter-native:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::eviction_tests::mapping_counter_retains_detached_lease_after_runtime_destruction -- --exact

jit-mapping-counter-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::resources::mapping_tests -- --test-threads=1

jit-segments: jit-segments-pure jit-segments-native

jit-segments-pure:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::segments::tests

jit-segments-native:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::segment_

jit-segments-reclamation:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::segment_reclamation_os_worker -- --exact

jit-segments-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::segments::tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::memory_tests::segment_record_underlying_refusal_precedes_mapping -- --exact --test-threads=1

jit-dominance:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::dominance::tests
	@$(MAKE) --no-print-directory jit-dominance-backend

jit-dominance-backend:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::dominance_

jit-predecessors:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::preds::tests
	@$(MAKE) --no-print-directory jit-predecessors-backend

jit-predecessors-backend:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::predecessor_quota_refuses_before_codegen_and_releases_storage -- --exact

jit-coverage-help:
	@$(CARGO) fuzz build --help
	@$(CARGO) fuzz run --help
	@$(CARGO) fuzz tmin --help

jit-coverage-environment:
	@$(MAKE) --no-print-directory environment
	@$(CARGO) fuzz --version
	@clang --version
	@printf 'CARGO_TARGET_DIR=%s\nRUSTFLAGS=%s\n' "$${CARGO_TARGET_DIR:-}" "$${RUSTFLAGS:-}"

jit-coverage-lock:
	@$(CARGO) generate-lockfile --manifest-path fuzz/Cargo.toml

jit-coverage-fmt:
	@$(CARGO) fmt --manifest-path fuzz/Cargo.toml

jit-coverage-fmt-check:
	@$(CARGO) fmt --manifest-path fuzz/Cargo.toml -- --check

jit-coverage-check:
	@$(CARGO) check --locked --manifest-path fuzz/Cargo.toml --all-targets

jit-coverage-test:
	@$(CARGO) test --locked --manifest-path fuzz/Cargo.toml --lib

jit-coverage-build:
	@case '$(CG_TARGET)' in scalar|heap) ;; *) echo 'Unknown coverage target' >&2; exit 2;; esac
	@CARGO_NET_OFFLINE=true $(MAKE) --no-print-directory jit-coverage-check
	@set -eu; lock=$$(sha256sum fuzz/Cargo.lock); trap 'test "$$lock" = "$$(sha256sum fuzz/Cargo.lock)" || { echo "Fuzz lockfile changed" >&2; exit 2; }' EXIT; CARGO_NET_OFFLINE=true $(CARGO) fuzz build --sanitizer address '$(CG_TARGET)'

jit-coverage-run:
	@case "$$(uname -sm)" in 'Linux x86_64'|'Linux aarch64') ;; *) echo 'Native coverage fuzz requires supported Linux'; exit 2;; esac
	@case '$(CG_TARGET)' in scalar|heap) ;; *) echo 'Unknown coverage target' >&2; exit 2;; esac
	@case '$(CG_RUNS):$(CG_SECONDS)' in *[!0-9:]*|:*|*:) echo 'Set numeric coverage limits' >&2; exit 2;; esac
	@test '$(CG_RUNS)' -gt 0 && test '$(CG_RUNS)' -le 100000
	@test '$(CG_SECONDS)' -gt 0 && test '$(CG_SECONDS)' -le 3600
	@$(MAKE) --no-print-directory jit-coverage-build CG_TARGET='$(CG_TARGET)'
	@mkdir -p '$(CG_DIR)/$(CG_TARGET)/corpus' '$(CG_DIR)/$(CG_TARGET)/artifacts'
	@cp -n fuzz/seeds/$(CG_TARGET)/* '$(CG_DIR)/$(CG_TARGET)/corpus/'
	@$(MAKE) --no-print-directory jit-coverage-environment > '$(CG_DIR)/$(CG_TARGET)/environment.log'
	@set -euo pipefail; lock=$$(sha256sum fuzz/Cargo.lock); trap 'test "$$lock" = "$$(sha256sum fuzz/Cargo.lock)" || { echo "Fuzz lockfile changed" >&2; exit 2; }' EXIT; CARGO_NET_OFFLINE=true timeout --signal=TERM --kill-after=10s "$$(( $(CG_SECONDS) + 60 ))s" $(CARGO) fuzz run --sanitizer address '$(CG_TARGET)' '$(CG_DIR)/$(CG_TARGET)/corpus' -- -runs='$(CG_RUNS)' -max_total_time='$(CG_SECONDS)' -max_len=256 -timeout=20 -rss_limit_mb=2048 -artifact_prefix='$(CG_DIR)/$(CG_TARGET)/artifacts/' -print_final_stats=1 2>&1 | tee '$(CG_DIR)/$(CG_TARGET)/run.log'

jit-coverage-replay:
	@case '$(CG_TARGET)' in scalar|heap) ;; *) echo 'Unknown coverage target' >&2; exit 2;; esac
	@test -n '$(CG_INPUT)' && test -f '$(CG_INPUT)'
	@$(MAKE) --no-print-directory jit-coverage-build CG_TARGET='$(CG_TARGET)'
	@mkdir -p '$(CG_DIR)/$(CG_TARGET)'
	@set -euo pipefail; lock=$$(sha256sum fuzz/Cargo.lock); trap 'test "$$lock" = "$$(sha256sum fuzz/Cargo.lock)" || { echo "Fuzz lockfile changed" >&2; exit 2; }' EXIT; CARGO_NET_OFFLINE=true timeout --signal=TERM --kill-after=10s 80s $(CARGO) fuzz run --sanitizer address '$(CG_TARGET)' '$(CG_INPUT)' -- -runs=1 -timeout=20 -rss_limit_mb=2048 -print_final_stats=1 2>&1 | tee '$(CG_DIR)/$(CG_TARGET)/replay.log'

jit-coverage-wrapper-tests:
	@mkdir -p '$(CG_DIR)/wrappers'
	@set -eu; for arg in CG_TARGET=unknown CG_RUNS=0 CG_RUNS=100001 CG_RUNS=-1 CG_RUNS=bad CG_RUNS= CG_SECONDS=0 CG_SECONDS=3601 CG_SECONDS=bad CG_SECONDS=; do log='$(CG_DIR)/wrappers/'"$${arg/=/-}".log; if $(MAKE) --no-print-directory jit-coverage-run CG_RUNS=1 CG_SECONDS=1 "$$arg" CARGO='echo CARGO_WAS_INVOKED' > "$$log" 2>&1; then echo "Accepted invalid limit: $$arg" >&2; exit 1; fi; if grep -q CARGO_WAS_INVOKED "$$log"; then echo "Launched Cargo for invalid limit: $$arg" >&2; exit 1; fi; done
	@set -eu; for target in jit-coverage-build jit-coverage-replay; do log='$(CG_DIR)/wrappers/'"$$target".log; if $(MAKE) --no-print-directory "$$target" CG_TARGET=unknown CARGO='echo CARGO_WAS_INVOKED' > "$$log" 2>&1; then exit 1; fi; if grep -q CARGO_WAS_INVOKED "$$log"; then exit 1; fi; done
	@set -eu; log='$(CG_DIR)/wrappers/replay-missing-input.log'; if $(MAKE) --no-print-directory jit-coverage-replay CG_INPUT= CARGO='echo CARGO_WAS_INVOKED' > "$$log" 2>&1; then exit 1; fi; if grep -q CARGO_WAS_INVOKED "$$log"; then exit 1; fi
	@echo 'Coverage wrapper refusal checks passed (13 cases)'

jit-fuzz:
	@case "$$(uname -sm)" in 'Linux x86_64'|'Linux aarch64') ;; *) echo 'Native fuzz requires supported Linux host'; exit 2;; esac
	@mkdir -p target/jit-evidence/fuzz
	@$(MAKE) --no-print-directory environment > target/jit-evidence/fuzz/environment.log
	@set -o pipefail; LUNA_JIT_FUZZ_TARGET='$(FUZZ_TARGET)' LUNA_JIT_FUZZ_CASES='$(FUZZ_CASES)' LUNA_JIT_FUZZ_SEEDS='$(FUZZ_SEEDS)' $(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::fuzz::supervisor -- --exact --ignored --nocapture 2>&1 | tee target/jit-evidence/fuzz/latest.log

jit-heap:
	@$(CARGO) test -p luna --features jit --test jit_heap $(TARGET_ARG) $(ARGS)

.PHONY: jit-upvalues
jit-upvalues:
	@$(CARGO) test -p luna --features jit --test jit_upvalues $(TARGET_ARG) $(ARGS)

jit-upvalue-current-frame:
	@$(CARGO) test --locked -p luna --test debug_lib $(TARGET_ARG) joined_upvalue_can_alias_the_executing_frames_local -- --exact
	@$(CARGO) test --locked -p luna --features jit --test jit_upvalues $(TARGET_ARG) current_frame_aliases_read_scratch_write_through_and_decline_stale_tables -- --exact

jit-helpers-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::helpers::tests -- --test-threads=1

jit-policy:
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::policy_tests
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::scheduling_tests
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::eviction_tests
	@$(CARGO) test -p luna --features jit --test jit_policy $(TARGET_ARG)

jit-host-memory:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) lua::memory_tests
	@$(MAKE) --no-print-directory jit-host-ledger jit-host-native jit-host-finish

jit-host-ledger:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::resources::tests

jit-host-native:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::host_
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::eviction_tests::host_

jit-host-finish:
	@$(CARGO) test --locked -p luna --features jit --test jit_memory $(TARGET_ARG)
	@$(MAKE) --no-print-directory jit-host-finish-async jit-host-reference

jit-host-finish-async:
	@$(CARGO) test --locked -p luna --features jit,async --test jit_memory $(TARGET_ARG)
	@$(CARGO) test --locked -p luna --features jit,async --test memory_accounting $(TARGET_ARG)

jit-host-reference:
	@$(CARGO) test --locked -p luna --test memory_accounting $(TARGET_ARG)

jit-relocations:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::relocation_tests
	@$(CARGO) test --locked -p luna --features jit --test jit_resources $(TARGET_ARG) relocation
	@$(MAKE) --no-print-directory jit-config

.PHONY: jit-compiler-lifetimes
jit-compiler-lifetimes:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::lifetime_tests

jit-resources:
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::resources
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::registry::tests
	@$(MAKE) --no-print-directory jit-transport-resources
	@$(CARGO) test -p luna --features jit --test jit_resources $(TARGET_ARG)

.PHONY: jit-transport-resources
jit-transport-resources:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::transport_resource_tests:: $(ARGS)

jit-registers:
	@$(CARGO) test -p luna --test register_boundaries $(TARGET_ARG)
	@$(CARGO) test -p luna --lib $(TARGET_ARG) compiler::register_allocator::tests
	@$(CARGO) test -p luna --features jit --test register_boundaries $(TARGET_ARG)

jit-boundary:
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit:: $(ARGS)

.PHONY: jit-mock
jit-mock:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::mock:: $(ARGS)

jit-example:
	@$(CARGO) run --example jit --features jit $(TARGET_ARG)

environment:
	@rustc --version
	@$(CARGO) --version
	@printf 'Cargo home: %s\n' "$${CARGO_HOME:-$$HOME/.cargo}"
	@printf 'Requested target: %s\n' '$(if $(TARGET),$(TARGET),host)'
	@printf 'Build directory: %s\nCargo target directory: %s\nNix output: %s\n' "$$(pwd -P)" "$${CARGO_TARGET_DIR:-default}" "$${out:-unset}"
	@printf 'NIX_LDFLAGS=%q\nNIX_LDFLAGS_FOR_TARGET=%q\n' "$${NIX_LDFLAGS:-}" "$${NIX_LDFLAGS_FOR_TARGET:-}"
	@uname -sm

jit-deps:
	@$(CARGO) fetch

jit-check:
	@$(CARGO) check --workspace --all-targets --features luna/jit $(TARGET_ARG)

jit-test:
	@case "$(JIT_MODE)" in off|auto|force) ;; *) echo "Invalid JIT_MODE: $(JIT_MODE)" >&2; exit 2;; esac
	@LUNA_TEST_JIT_MODE=$(JIT_MODE) $(CARGO) test --workspace --all-targets --features luna/jit $(TARGET_ARG)

jit-test-all:
	@case "$(JIT_MODE)" in off|auto|force) ;; *) echo "Invalid JIT_MODE: $(JIT_MODE)" >&2; exit 2;; esac
	@LUNA_TEST_JIT_MODE=$(JIT_MODE) $(CARGO) test --workspace --all-targets --all-features $(TARGET_ARG)

jit-test-doc:
	@$(CARGO) test --workspace --doc --features luna/jit $(TARGET_ARG)

jit-rustdoc:
	@RUSTDOCFLAGS="-Dwarnings" $(CARGO) doc --workspace --no-deps --features luna/jit $(TARGET_ARG)

jit-tree:
	@$(CARGO) tree -p luna -e normal $(TARGET_ARG)

jit-verify: verify jit-check jit-bench-paired-profile-tests jit-bench-raw-tests jit-oslo-consumer-tests
	@$(MAKE) jit-test JIT_MODE=off
	@$(MAKE) jit-test JIT_MODE=auto
	@$(MAKE) jit-test JIT_MODE=force
	@$(MAKE) jit-test-all JIT_MODE=force
	@$(MAKE) jit-test-doc jit-rustdoc
	@case "$$(uname -sm)" in 'Linux x86_64'|'Linux aarch64') $(MAKE) jit-fuzz-smoke ;; *) echo 'Skipping native fuzz: unsupported execution host' ;; esac

# luna is a library pair, not a binary: `build` compiles the workspace and its examples, because
# the examples are the only executables here and they are what breaks first on an API change.
build:
	@$(CARGO) build --workspace --all-targets $(TARGET_ARG)

b: build

# The fast inner loop, without the examples.
dev:
	@$(CARGO) build --workspace

compile:
	@$(CARGO) clean
	@$(MAKE) build

c: compile

# The REPL, or any other example: make run EXAMPLE=execute ARGS=script.lua
run:
	@$(CARGO) run --example $(EXAMPLE) $(TARGET_ARG) -- $(ARGS)

r: run

repl:
	@$(CARGO) run --example interpreter -- $(ARGS)

.PHONY: numeric-conversions jit-vm-dispatch jit-set-list jit-set-list-miri jit-frame-view jit-frame-view-miri jit-integer-table jit-integer-table-miri
.PHONY: jit-operand-sources jit-operand-sources-miri
.PHONY: jit-constant-keys jit-constant-keys-miri
.PHONY: string-key-hash string-key-hash-miri
string-key-hash:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) key_hash_tests $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) key_hash_tests $(ARGS)

string-key-hash-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' key_hash_tests -- --test-threads=1

jit-constant-keys:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helpers::constant_key_tests $(ARGS)

jit-constant-keys-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::helpers::constant_key_tests -- --test-threads=1

jit-operand-sources:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helpers::operand_source_tests
	@$(CARGO) test --locked -p luna --features jit --test jit_operand_sources $(TARGET_ARG)

jit-operand-sources-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::helpers::operand_source_tests -- --test-threads=1

jit-integer-table:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helpers::integer_table_tests
	@$(CARGO) test --locked -p luna --features jit --test jit_integer_table $(TARGET_ARG)

jit-integer-table-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helpers::integer_table_tests

jit-frame-view:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::register_view_tests

jit-frame-view-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib $(TARGET_ARG) thread::register_view_tests

.PHONY: vm-register-base vm-register-base-miri
vm-register-base:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::thread::tests::register_base
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::thread::tests::register_base

vm-register-base-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' thread::thread::tests::register_base -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::thread::tests::register_base -- --test-threads=1

jit-set-list-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helpers::set_list_tests

jit-set-list:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::helpers::set_list_tests
	@$(CARGO) test --locked -p luna --features jit --test jit_set_list $(TARGET_ARG)

jit-vm-dispatch:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::vm::dispatch::tests $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::vm::dispatch::tests $(ARGS)

.PHONY: jit-vm-dispatch-miri
jit-vm-dispatch-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' thread::vm::dispatch::tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::vm::dispatch::tests -- --test-threads=1

.PHONY: vm-numeric-pair vm-numeric-pair-miri
vm-numeric-pair:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::vm::numeric_pair
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::vm::numeric_pair

vm-numeric-pair-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' thread::vm::numeric_pair -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::vm::numeric_pair -- --test-threads=1

.PHONY: value-representation vm-constant-add vm-constant-add-miri vm-constants vm-constants-miri
.PHONY: vm-closures vm-closures-miri
vm-closures:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::vm::instantiate
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::vm::instantiate
	@$(MAKE) --no-print-directory stdlib-debug jit-upvalues jit-regions

vm-closures-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' thread::vm::instantiate -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::vm::instantiate -- --test-threads=1

value-representation:
	@$(CARGO) test --locked -p luna --no-default-features --test sizes $(TARGET_ARG)
	@$(CARGO) test --locked -p luna --features jit --test sizes $(TARGET_ARG)
	@$(MAKE) --no-print-directory vm-constants numeric-conversions

vm-constant-add:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::vm::constant_add::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::vm::constant_add::tests
	@$(MAKE) --no-print-directory vm-constants numeric-conversions

vm-constant-add-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' thread::vm::constant_add::tests -- --test-threads=1

vm-constants:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::vm::constants::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::vm::constants::tests
	@$(CARGO) test --locked -p luna --no-default-features --test vm_constants $(TARGET_ARG)
	@for mode in off auto force; do \
		LUNA_TEST_JIT_MODE=$$mode $(CARGO) test --locked -p luna --features jit --test vm_constants $(TARGET_ARG) || exit $$?; \
	done

vm-constants-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' thread::vm::constants::tests -- --test-threads=1

VM_CONSTANTS_DIR ?= target/jit-evidence/vm-constants
.PHONY: vm-constants-memcheck
vm-constants-memcheck:
	@mkdir -p '$(VM_CONSTANTS_DIR)'
	@$(CARGO) test --locked -p luna --features jit --test vm_constants $(TARGET_ARG) --no-run --message-format=json-render-diagnostics > '$(VM_CONSTANTS_DIR)/build.jsonl'
	@jq -r 'select(.reason == "compiler-artifact" and .target.name == "vm_constants" and .profile.test and .executable != null) | .executable' '$(VM_CONSTANTS_DIR)/build.jsonl' > '$(VM_CONSTANTS_DIR)/binary-path'
	@test "$$(wc -l < '$(VM_CONSTANTS_DIR)/binary-path')" -eq 1
	@for mode in off auto force; do \
		LUNA_TEST_JIT_MODE=$$mode valgrind --tool=memcheck --error-exitcode=99 --leak-check=full --show-leak-kinds=all --errors-for-leak-kinds=definite,indirect "$$(cat '$(VM_CONSTANTS_DIR)/binary-path')" --test-threads=1 > '$(VM_CONSTANTS_DIR)'/$$mode.log 2>&1 || exit $$?; \
	done

.PHONY: integer-conversions
integer-conversions:
	@$(CARGO) test --locked -p luna --no-default-features --test conversion $(TARGET_ARG)
	@for mode in off auto force; do \
		LUNA_TEST_JIT_MODE=$$mode $(CARGO) test --locked -p luna --features jit --test conversion $(TARGET_ARG) || exit $$?; \
	done

numeric-conversions:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) constant::tests
	@$(CARGO) test --locked -p luna --no-default-features --test numeric_semantics $(TARGET_ARG)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) constant::tests
	@for mode in off auto force; do \
		LUNA_TEST_JIT_MODE=$$mode $(CARGO) test --locked -p luna --features jit --test numeric_semantics $(TARGET_ARG) || exit $$?; \
	done

CALLBACK_SCRATCH_DIR ?= target/jit-evidence/hook-resume-recovery/callback-scratch/timing-image
CALLBACK_SCRATCH_TIMING_DIR ?= target/jit-evidence/hook-resume-recovery/callback-scratch/timing
.PHONY: jit-callback-scratch-time-run jit-callback-scratch-time-audit
jit-callback-scratch-time-run: jit-profile-cpu-check
	@bash examples/jit_support/strided_timing.sh '$(CALLBACK_SCRATCH_DIR)' '$(CALLBACK_SCRATCH_TIMING_DIR)' '$(PROFILE_CPU)' '$(STRIDED_TIMING_WINDOWS)' callback_scratch

jit-callback-scratch-time-audit:
	@perl examples/jit_support/strided_timing_audit.pl '$(CALLBACK_SCRATCH_TIMING_DIR)'

CALLBACK_BASELINE_DIR ?= target/jit-evidence/hook-resume-recovery/callback-scratch/scratch-free-image
.PHONY: jit-ordinary-baseline-time-run
jit-ordinary-baseline-time-run: jit-profile-cpu-check
	@bash examples/jit_support/strided_timing.sh '$(STRIDED_RUNTIME_DIR)' '$(STRIDED_TIMING_DIR)' '$(PROFILE_CPU)' '$(STRIDED_TIMING_WINDOWS)' ordinary_baseline '$(CALLBACK_BASELINE_DIR)'

.PHONY: jit-callback-baseline-time-run
jit-callback-baseline-time-run: jit-profile-cpu-check
	@bash examples/jit_support/strided_timing.sh '$(CALLBACK_SCRATCH_DIR)' '$(CALLBACK_SCRATCH_TIMING_DIR)' '$(PROFILE_CPU)' '$(STRIDED_TIMING_WINDOWS)' callback_scratch_baseline '$(CALLBACK_BASELINE_DIR)'

.PHONY: stack-bounds stack-bounds-miri stack-bounds-callbacks vm-return-spans
vm-return-spans:
	@for checked in true false; do \
		CARGO_PROFILE_DEV_OVERFLOW_CHECKS=$$checked $(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) thread::thread::tests::return_spans_preserve_bounds_fuel_and_capture_closing -- --exact || exit $$?; \
		CARGO_PROFILE_DEV_OVERFLOW_CHECKS=$$checked $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::thread::tests::return_spans_preserve_bounds_fuel_and_capture_closing -- --exact || exit $$?; \
	done

stack-bounds:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) stack::bounds_tests $(ARGS)
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) stack::bounds_tests $(ARGS)

stack-bounds-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' stack::bounds_tests -- --test-threads=1

stack-bounds-callbacks:
	@$(CARGO) test --locked -p luna --no-default-features --test callback $(TARGET_ARG) callback_stack_overflow_preserves_caller
	@for mode in off auto force; do \
		LUNA_TEST_JIT_MODE=$$mode $(CARGO) test --locked -p luna --features jit --test callback $(TARGET_ARG) callback_stack_overflow_preserves_caller || exit $$?; \
	done

.PHONY: stack-replace stack-replace-miri
stack-replace:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) stack::replace_tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) stack::replace_tests

stack-replace-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' stack::replace_tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' stack::replace_tests -- --test-threads=1

.PHONY: stack-consume stack-consume-miri
stack-consume:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) stack::consume_tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) stack::consume_tests

stack-consume-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' stack::consume_tests -- --test-threads=1

.PHONY: stack-drain stack-drain-miri
stack-drain:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) stack::drain_tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) stack::drain_tests
	@$(MAKE) --no-print-directory stack-consume

stack-drain-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' stack::drain_tests -- --test-threads=1
	@$(MAKE) --no-print-directory stack-consume-miri

# `--all-targets` includes integration suites, but not doc tests.
test:
	@$(CARGO) test --workspace --all-targets $(TARGET_ARG)
	@$(MAKE) --no-print-directory test-doc

t: test

# `default = []`, so the plain `test` target compiles `tests/derive.rs` and `tests/async_foreign.rs`
# down to zero tests. Without this target neither optional feature is ever built by the gate.
test-all:
	@$(CARGO) test --workspace --all-targets --all-features $(TARGET_ARG)

test-doc:
	@$(CARGO) test --workspace --doc $(TARGET_ARG)

check:
	@$(CARGO) check --workspace --all-targets $(TARGET_ARG)

check-all:
	@$(CARGO) check --workspace --all-targets --all-features $(TARGET_ARG)

fmt:
	@$(CARGO) fmt --all

fmt-check:
	@$(CARGO) fmt --all -- --check

# Warnings are reported, not denied. The code inherited from upstream carries ~137 lints, and a
# gate that fails on all of them from day one is a gate nobody can run. `clippy-strict` is the
# version to switch `verify` to once that backlog is cleared.
clippy:
	@$(CARGO) clippy --workspace --all-targets $(TARGET_ARG)

jit-clippy:
	@$(CARGO) clippy --workspace --all-targets --all-features $(TARGET_ARG)

clippy-strict:
	@$(CARGO) clippy --workspace --all-targets $(TARGET_ARG) -- -D warnings

rustdoc:
	@RUSTDOCFLAGS="-Dwarnings" $(CARGO) doc --workspace --no-deps $(TARGET_ARG)

# Echoes the name parsed out of Cargo.toml. CI prints it because an empty name trips the $(error)
# above before any target runs, and that failure looks like nothing at all in a run summary.
print-name:
	@echo '$(PROJECT_NAME)'

clean:
	@$(CARGO) clean

# Clippy is deliberately not in the gate yet: the code inherited from upstream carries 135
# warnings and 2 deny-by-default `never_loop` errors in src/meta_ops.rs, so a `verify` that
# included it would be red on arrival and stop being run. Put it back once that is cleared.
verify: fmt-check check check-all test test-all rustdoc

# Order matters: luna depends on luna-derive and luna-util depends on luna, so each has to be in
# the registry before the one that needs it.
publish:
	@$(CARGO) publish -p $(PROJECT_NAME)-derive
	@$(CARGO) publish -p $(PROJECT_NAME)
	@$(CARGO) publish -p $(PROJECT_NAME)-util

release:
	@if [ -z "$(HAS_REL)" ]; then \
		echo "git-rel is not installed. Please install it first."; \
		exit 1; \
	fi
	@if [ -z "$(TYPE)" ]; then \
		echo "Release type not specified. Use 'make release TYPE=[patch|minor|major|M.m.p]'"; \
		exit 1; \
	fi
	@git rel $(TYPE)

help:
	@echo
	@echo "Usage: make [target]"
	@echo
	@echo "Available targets:"
	@echo "  build        Build the workspace and its examples"
	@echo "  dev          Build the libraries only"
	@echo "  compile      Clean and rebuild"
	@echo "  run          Run an example (make run EXAMPLE=execute ARGS=script.lua)"
	@echo "  repl         Run the interpreter example"
	@echo "  test         Run all tests, including doc tests"
	@echo "  test-all     Run all tests with every feature enabled"
	@echo "  numeric-conversions Check numeric coercion and arithmetic in all runtime modes"
	@echo "  jit-vm-dispatch Check per-slice dispatch selection and code ownership"
	@echo "  jit-set-list Check fixed-list native writes, fuel, and fallback"
	@echo "  jit-frame-view Check current-frame views and bounds guards"
	@echo "  jit-integer-table Check integer reads, weak values, and fallback"
	@echo "  jit-operand-sources Check constant/register helper sources"
	@echo "  jit-helper-flow-miri Check pure source flow and import layouts"
	@echo "  test-doc     Run doc tests alone"
	@echo "  check        Run cargo check on all targets"
	@echo "  check-all    Run cargo check on all targets/all features"
	@echo "  clippy       Run clippy, reporting warnings"
	@echo "  clippy-strict Run clippy with warnings denied"
	@echo "  rustdoc      Build docs with warnings denied"
	@echo "  fmt          Format the workspace"
	@echo "  fmt-check    Check formatting"
	@echo "  print-name   Echo the package name parsed from Cargo.toml"
	@echo "  clean        Remove Cargo build artifacts"
	@echo "  verify       Run the full local gate"
	@echo "  jit-check    Check the optional native backend (TARGET=... supported)"
	@echo "  jit-test     Test JIT-enabled builds (JIT_MODE=off|auto|force)"
	@echo "  jit-test-all Test JIT builds with all optional features"
	@echo "  jit-verify   Run baseline and all JIT validation modes"
	@echo "  jit-platform Run the full gate on matching hardware (TARGET=... required)"
	@echo "  ci-check     Validate the active GitHub Actions workflow"
	@echo "  jit-backend  Execute the native helper-call and worker-transfer probes"
	@echo "  jit-helpers  Test scoped Rust helper frames and panic transport"
	@echo "  jit-miri     Check Rust-only JIT components (nix develop .#miri)"
	@echo "  jit-relocations Test finalization relocation limits and fallback"
	@echo "  jit-errors   Test native error positions and typed callback errors"
	@echo "  jit-debug    Compare exact hook events and mixed-tier tracebacks"
	@echo "  jit-gc-requests Test default/nil/explicit GC request boundaries"
	@echo "  stdlib-debug Test debug indices and mutation in Off/Auto/Force"
	@echo "  jit-boundary Test native exits against the Rust boundary model"
	@echo "  jit-native   Run integrated native execution and lifecycle tests"
	@echo "  jit-fallback Execute unsupported-target fallback (TARGET=i686-unknown-linux-musl)"
	@echo "  jit-compiler-lifetimes Check compiler workspace release boundaries"
	@echo "  jit-numeric  Check reference and exact native numeric comparisons"
	@echo "  jit-fuzz-smoke Run supervised admission/scalar/heap smoke"
	@echo "  jit-fuzz     Run bounded seeded campaigns (FUZZ_TARGET/CASES/SEEDS)"
	@echo "  jit-fuzz-heap Run supervised heap/lifecycle campaigns"
	@echo "  jit-heap     Run native heap/upvalue mutation and GC tests"
	@echo "  jit-upvalues Test upvalue aliases, foreign stacks and GC"
	@echo "  jit-abi      Test scalar and reference ABI conversions"
	@echo "  jit-config   Test configuration and disabled-state collection"
	@echo "  jit-stats    Verify native exit reasons and execution counters"
	@echo "  jit-accounting Check mixed counters, dispatches and exact fuel"
	@echo "  jit-chain    Test source-bound native caller continuations"
	@echo "  jit-payload-model       Test scoped scalar payloads"
	@echo "  jit-payload-packed-calls Verify packed helper lowering"
	@echo "  jit-payload-packed-calls-miri Check packed lowering under Miri"
	@echo "  jit-payload-packed-reads Verify packed scalar reads"
	@echo "  jit-payload-packed-reads-miri Check packed reads under Miri"
	@echo "  jit-payload-packed-writes Verify packed scalar stores"
	@echo "  jit-payload-packed-writes-miri Check packed stores under Miri"
	@echo "  jit-payload-packed-lower Verify whole packed lowering"
	@echo "  jit-payload-packed-entry Verify owned packed native entries"
	@echo "  jit-payload-packed-runtime Check packed runtime parity"
	@echo "  jit-payload-packed-fast Check scoped packed helper fast paths"
	@echo "  jit-payload-packed-lower-miri Check whole lowering under Miri"
	@echo "  jit-payload-relocations Check direct/indirect relocation budgets"
	@echo "  jit-helper-transports   Test scratch and direct helpers"
	@echo "  jit-helper-transports-miri Check helper transports under Miri"
	@echo "  jit-payload-model-miri  Check scalar payloads under Miri"
	@echo "  jit-test-modes Test Force preparation and explicit exclusions"
	@echo "  jit-numeric-exits Check numeric fallbacks between native work"
	@echo "  jit-registers Test register-255 and stack-256 boundaries"
	@echo "  jit-policy   Test quota refusal and configuration retirement"
	@echo "  jit-resources Test owned-container budgets and reclamation"
	@echo "  jit-example  Run the explicitly prepared native Lua example"
	@echo "  jit-bench    Measure checked workloads (ARGS='--mode off --samples 11')"
	@echo "  jit-bench-build Build the benchmark without timing"
	@echo "  jit-bench-run Time an existing artifact (JIT_BENCH_BINARY=path)"
	@echo "  jit-bench-profile-run Profile a frozen warm benchmark (PROFILE_CASE=... PROFILE_MODE=off|auto)"
	@echo "  PROFILE_CPU Select one allowed CPU for native/cost profiles (default: first allowed)"
	@echo "  jit-bench-paired-profile-run Profile a frozen full paired suite (new PAIRED_PROFILE_DIR=path)"
	@echo "  jit-bench-paired-profile-tests Test full-suite profile partition verification"
	@echo "  jit-metrics  Observe cold compilation, coverage and host slice costs"
	@echo "  jit-metrics-build Build the separate scheduling metrics artifact"
	@echo "  jit-metrics-run Measure an existing artifact (JIT_METRICS_BINARY=path)"
	@echo "  jit-metrics-tests Test scheduling observations and argument validation"
	@echo "  jit-registry Test weak source registration and queued cancellation"
	@echo "  jit-host-memory Test shared host quotas and finish/await enforcement"
	@echo "  jit-ir       Test owned operands, flow, effects, exits and quota"
	@echo "  jit-access   Test register, scalar output and helper admission"
	@echo "  jit-exits    Test exit snapshots and retry-after-store rejection"
	@echo "  jit-suspension Test native coroutine and foreign-await resumption"
	@echo "  jit-sequences Test Rust continuations across native execution"
	@echo "  jit-mock     Test Rust-only slice exits and interpreter fallback"
	@echo "  jit-bench-paired Alternate checked Off/Auto samples"
	@echo "  jit-bench-raw-audit Recompute paired statistics (JIT_BENCH_LOG=path)"
	@echo "  jit-bench-raw-tests Test raw paired statistics and thresholds"
	@echo "  jit-performance Check frozen paired workload thresholds"
	@echo "  jit-disassembly Dump finalized native kernels and addresses"
	@echo "  jit-shipping Publish checked paired timings at shipping opt-level s"
	@echo "  jit-size     Measure matched binary size and 5% disabled overhead"
	@echo "  jit-size-build Build/copy matched probes (SIZE_PROFILE=shipping|speed)"
	@echo "  jit-size-run Check existing matched probes without rebuilding"
	@echo "  jit-cost-tests Test feature-cost protocol and frozen overhead control"
	@echo "  jit-cost-selector-tests Verify all full/selected worker CLI protocols"
	@echo "  jit-cost-native Verify usable native code in the matched JIT artifact"
	@echo "  jit-cost-native-run Verify existing native probes without rebuilding"
	@echo "  jit-cost-profile Compare VM instruction/cache/branch counts (PROFILE_CASE=...)"
	@echo "  jit-profile  Profile the short-function workload (requires perf permission)"
	@echo "  jit-clean-release-package Clean only luna's release cache (TARGET=...)"
	@echo "  jit-rust-assembly Inspect symbol-retained Rust dispatch/helper assembly"
	@echo "  publish      Publish $(PROJECT_NAME)-derive, $(PROJECT_NAME), then $(PROJECT_NAME)-util"
	@echo "  release      Release a new version"
	@echo

h: help

.PHONY: jit-preferred-entry jit-preferred-entry-miri
jit-preferred-entry:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::entry_flow::preferred::tests | awk '/^test result: ok[.] 10 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

jit-preferred-entry-miri:
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::entry_flow::preferred::tests -- --test-threads=1 | awk '/^test result: ok[.] 10 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

.PHONY: jit-preferred-entry-native
jit-preferred-entry-native:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::entry_flow::preferred::native | awk '/^test result: ok[.] 6 passed; 0 failed;/ { valid=1 } { print } END { exit !valid }'

.PHONY: jit-preferred-entry-runtime
jit-preferred-entry-runtime:
	@set -o pipefail; $(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::payload_runtime_tests::preferred_entry | awk '/^test result: ok[.] 3 passed; 0 failed; 2 ignored;/ { valid=1 } { print } END { exit !valid }'

PREFERRED_ENTRY_DIR ?= target/jit-evidence/short-slice-performance/preferred-entry-profile
.PHONY: jit-preferred-entry-profile-build jit-preferred-entry-profile-run
jit-preferred-entry-profile-build:
	@$(MAKE) jit-strided-profile-build STRIDED_DIR='$(PREFERRED_ENTRY_DIR)'
	@{ rustc -vV; $(CARGO) --version; printf 'opt_level=3\nstrip=false\nscope=frozen rust_callbacks workload in empty Lua state\nwarmups=2\nmeasured_executions=1\nexperiment=test-only preferred entry\n'; } > '$(PREFERRED_ENTRY_DIR)/configuration'

jit-preferred-entry-profile-run: jit-profile-cpu-check
	@sha256sum -c '$(PREFERRED_ENTRY_DIR)/binary.sha256' '$(PREFERRED_ENTRY_DIR)/source.sha256' > '$(PREFERRED_ENTRY_DIR)/verification-before.log'
	@set -e; for round in 0 1 2; do order='0 1'; if [ $$round = 1 ]; then order='1 0'; fi; for selected in $$order; do \
		out='$(PREFERRED_ENTRY_DIR)'/$$round-$$selected; test ! -e "$$out.callgrind"; \
		LUNA_JIT_DIAGNOSTIC_DIR="$$out-native" LUNA_PREFERRED_ENTRY=$$selected taskset -c '$(PROFILE_CPU)' valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*preferred_entry::profile_execute*' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file="$$out.callgrind" \
			'$(PREFERRED_ENTRY_DIR)/test-binary' jit::payload_runtime_tests::preferred_entry::profile_preferred_callback --ignored --exact --nocapture --test-threads=1 > "$$out.log" 2>&1; \
		grep -Fq "case=rust_callbacks preferred=$$([ $$selected = 1 ] && echo true || echo false) validated=true" "$$out.log"; \
		grep -Fq 'test result: ok. 1 passed; 0 failed' "$$out.log"; \
		grep -Eq '^summary: [1-9][0-9]*' "$$out.callgrind"; \
	done; done
	@sha256sum -c '$(PREFERRED_ENTRY_DIR)/binary.sha256' '$(PREFERRED_ENTRY_DIR)/source.sha256' > '$(PREFERRED_ENTRY_DIR)/verification-after.log'

PREFERRED_TIMING_IMAGE ?= target/jit-evidence/short-slice-performance/preferred-entry-timing-image
PREFERRED_TIMING_DIR ?= target/jit-evidence/short-slice-performance/preferred-entry-timing
PREFERRED_TIMING_WINDOWS ?= 201
.PHONY: jit-preferred-entry-time-build jit-preferred-entry-time-run jit-preferred-entry-time-audit
jit-preferred-entry-time-build:
	@$(MAKE) jit-preferred-entry-profile-build PREFERRED_ENTRY_DIR='$(PREFERRED_TIMING_IMAGE)'
	@sha256sum examples/jit_support/strided_timing.sh examples/jit_support/strided_timing_audit.pl tests/test-jit-strided-timing.pl >> '$(PREFERRED_TIMING_IMAGE)/source.sha256'

jit-preferred-entry-time-run: jit-profile-cpu-check
	@bash examples/jit_support/strided_timing.sh '$(PREFERRED_TIMING_IMAGE)' '$(PREFERRED_TIMING_DIR)' '$(PROFILE_CPU)' '$(PREFERRED_TIMING_WINDOWS)' preferred_entry

jit-preferred-entry-time-audit:
	@perl examples/jit_support/strided_timing_audit.pl '$(PREFERRED_TIMING_DIR)'

.PHONY: jit-entry-admission jit-entry-admission-miri
jit-entry-admission:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::entry_admission

jit-entry-admission-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::entry_admission::tests -- --test-threads=1

.PHONY: jit-specialized-owner jit-specialized-owner-miri
jit-specialized-owner:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::specialized::tests

jit-specialized-owner-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::specialized::tests -- --test-threads=1

.PHONY: binary-metamethods binary-metamethods-miri
binary-metamethods:
	@$(CARGO) test --locked -p luna --no-default-features --lib $(TARGET_ARG) meta_ops::binary_dispatch_tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) meta_ops::binary_dispatch_tests
	@for mode in off auto force; do \
		LUNA_TEST_JIT_MODE=$$mode $(CARGO) test --locked -p luna --features jit --test metamethods $(TARGET_ARG) || exit $$?; \
	done

binary-metamethods-miri:
	@$(CARGO) miri test --locked -p luna --no-default-features --lib --target '$(MIRI_TARGET)' meta_ops::binary_dispatch_tests -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' meta_ops::binary_dispatch_tests -- --test-threads=1

.PHONY: jit-entry-hints jit-entry-hints-miri
jit-entry-hints:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::entry_bytes
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) table::raw::code_read_hint_tests

jit-entry-hints-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::entry_bytes -- --test-threads=1
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' table::raw::code_read_hint_tests -- --test-threads=1

.PHONY: jit-paired-progress jit-paired-progress-miri
jit-paired-progress:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::paired_progress $(ARGS)

jit-paired-progress-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::paired_progress -- --test-threads=1

.PHONY: jit-transport-inputs jit-transport-inputs-miri
jit-transport-inputs:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::abi::transport_inputs $(ARGS)
jit-transport-inputs-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::abi::transport_inputs -- --test-threads=1
