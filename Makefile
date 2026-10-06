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
JIT_METRICS_BINARY ?= $(TOP_DIR)/target/$(if $(TARGET),$(TARGET)/,)release/examples/jit_metrics
JIT_DUMP_DIR ?= target/jit-evidence/native/$(if $(TARGET),$(TARGET),host)
SIZE_PROFILE ?= shipping
COST_SAMPLES ?= 11
COST_ITERATIONS ?= 20
COST_STRIP ?= true
PROFILE_CASE ?= float_loop
PROFILE_MODE ?= off
COST_DIR := target/jit-evidence/feature-cost/$(SIZE_PROFILE)$(if $(TARGET),-$(TARGET),)$(if $(filter false,$(COST_STRIP)),-symbols,)
COST_PROFILE_DIR := $(COST_DIR)/callgrind/$(PROFILE_CASE)/$(PROFILE_MODE)
COST_RELEASE_DIR := target/$(if $(TARGET),$(TARGET)/,)release/examples
COST_OPT := $(if $(filter shipping,$(SIZE_PROFILE)),s,3)
MIRI_TARGET ?= x86_64-unknown-linux-gnu
MIRI_DIR := target/jit-evidence/miri/$(MIRI_TARGET)

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
.PHONY: jit-helper-flow jit-helper-flow-backend
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
.PHONY: jit-clippy

ci-check:
	@$(ACTIONLINT) .github/workflows/tests.yml

jit-miri-wrapper-tests:
	@bash tests/test-jit-miri.sh

jit-miri:
	@mkdir -p '$(MIRI_DIR)'
	@set -o pipefail; { rustc -vV; $(CARGO) miri --version; printf 'MIRIFLAGS=%s\ntarget=%s\n' "$${MIRIFLAGS:-}" '$(MIRI_TARGET)'; } 2>&1 | tee '$(MIRI_DIR)/environment.log'
	@set -o pipefail; $(CARGO) miri setup --target '$(MIRI_TARGET)' 2>&1 | tee '$(MIRI_DIR)/setup.log'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::resources::tests -- --test-threads=1 --skip jit::resources::tests::installation_refusal_reclaims_generated_mappings 2>&1 | tee '$(MIRI_DIR)/jit-resources-tests.log'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::memory_tests::block_map_quota_refuses_before_host_setup_and_releases_storage -- --exact --test-threads=1 2>&1 | tee '$(MIRI_DIR)/block-map-quota.log'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::memory_tests::entry_path_quota_refuses_before_host_setup_and_releases_storage -- --exact --test-threads=1 2>&1 | tee '$(MIRI_DIR)/entry-path-quota.log'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::memory_tests::status_refusal_precedes_host_setup_and_releases_entry_storage -- --exact --test-threads=1 2>&1 | tee '$(MIRI_DIR)/memory-status-quota.log'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::memory_tests::provider_box_refusal_precedes_host_setup_and_releases_entry_storage -- --exact --test-threads=1 2>&1 | tee '$(MIRI_DIR)/provider-box-quota.log'
	@set -o pipefail; $(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::backend::memory_tests::segment_record_underlying_refusal_precedes_mapping -- --exact --test-threads=1 2>&1 | tee '$(MIRI_DIR)/segment-record-quota.log'
	@set -e -o pipefail; for filter in jit::handoff::tests jit::arrays::tests jit::runtime_owner_tests jit::global_owner::tests jit::resources::bootstrap_tests jit::resources::mapping_tests jit::abi::tests jit::helpers::tests jit::registry::tests jit::ir::tests jit::flow::tests jit::work::tests jit::preds::tests jit::dominance::tests jit::owner::tests jit::atomic_owner::tests jit::memory_status::tests jit::global_box::tests jit::segments::tests jit::tags::tests jit::shape::tests jit::backend::comparison_tests jit::backend::loop_tests jit::backend::transfer_tests jit::backend::helper_flow_tests jit::backend::ownership_tests jit::access::tests jit::backend::access_tests jit::entry_flow::tests jit::entry_flow::region_tests jit::exit_flow::tests jit::exits::tests jit::backend::exit_tests jit::policy_tests finalizers::tests lua::memory_tests; do \
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
	@set -o pipefail; CARGO_TARGET_DIR='$(TOP_DIR)/target' LUNA_BENCH_OPT_LEVEL=$(JIT_BENCH_OPT) CARGO_PROFILE_RELEASE_OPT_LEVEL=$(JIT_BENCH_OPT) $(CARGO) build --release --example jit_bench --features jit $(TARGET_ARG) 2>&1 | tee target/jit-evidence/bench-build.log

jit-bench-run:
	@mkdir -p target/jit-evidence
	@test -x '$(JIT_BENCH_BINARY)'
	@set -o pipefail; '$(JIT_BENCH_BINARY)' $(ARGS) 2>&1 | tee target/jit-evidence/bench.log

jit-bench: jit-bench-build
	@$(MAKE) --no-print-directory jit-bench-run

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

jit-performance:
	@$(MAKE) --no-print-directory jit-bench JIT_BENCH_OPT=3 ARGS='--mode paired --samples 11 --check $(ARGS)'

jit-shipping:
	@$(MAKE) --no-print-directory jit-bench JIT_BENCH_OPT=s ARGS='--mode paired --samples 11 $(ARGS)'

jit-cost-tests:
	@$(CARGO) test --example jit_feature_cost --no-default-features $(TARGET_ARG)
	@$(CARGO) test --example jit_feature_cost --no-default-features --features jit $(TARGET_ARG)
	@$(MAKE) --no-print-directory jit-artifact-tests

.PHONY: jit-cost-verify jit-artifact-tests
jit-cost-verify:
	@bash examples/jit_support/verify_artifacts.sh '$(COST_DIR)'

jit-artifact-tests:
	@bash tests/test-jit-artifacts.sh

jit-size-build:
	@case '$(SIZE_PROFILE)' in shipping|speed) ;; *) echo 'SIZE_PROFILE must be shipping|speed' >&2; exit 2;; esac
	@mkdir -p $(COST_DIR)
	@$(MAKE) --no-print-directory environment > $(COST_DIR)/environment.log
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
jit-cost-profile-run:
	@case '$(PROFILE_CASE)' in integer_loop|float_loop|array_table|closure_upvalue|polymorphic_metamethod|rust_callbacks|allocation_gc) ;; *) echo 'Unknown warm profiling case' >&2; exit 2;; esac
	@case '$(PROFILE_MODE)' in off|auto) ;; *) echo 'PROFILE_MODE must be off|auto' >&2; exit 2;; esac
	@mkdir -p $(COST_PROFILE_DIR)
	@cp $(COST_DIR)/environment.log $(COST_DIR)/binary-sha256.log $(COST_PROFILE_DIR)/
	@valgrind --version > $(COST_PROFILE_DIR)/profiler.log
	@printf 'case=%s\niterations=3\nwarmups=2\ncollection=*Executor>::step\nno_jit_mode=off\njit_mode=%s\n' '$(PROFILE_CASE)' '$(PROFILE_MODE)' >> $(COST_PROFILE_DIR)/profiler.log
	@set -e; for variant in no-jit jit-off; do \
		mode=off; if test "$$variant" = jit-off; then mode='$(PROFILE_MODE)'; fi; \
		valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*Executor>::step' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file=$(COST_PROFILE_DIR)/$$variant.callgrind \
			$(COST_DIR)/$$variant --worker --mode "$$mode" --case '$(PROFILE_CASE)' --iterations 3 > $(COST_PROFILE_DIR)/$$variant-profile.log 2>&1; \
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

jit-rust-assembly: jit-profile-build
	@mkdir -p target/jit-evidence
	@set -o pipefail; objdump -Cd target/release/examples/jit_bench | awk '/<luna::jit::Runtime::(run|lookup)>:|<<luna::jit::Runtime>::invoke.*>:|<luna::jit::helpers::call.*>:|<luna::thread::vm::run_vm.*>:/ { emit=1 } emit { print } /^$$/ { emit=0 }' > target/jit-evidence/rust-assembly.log
	@test -s target/jit-evidence/rust-assembly.log

jit-disassembly:
	@case "$$(uname -sm)" in 'Linux x86_64') ;; 'Linux aarch64') ;; *) echo 'Native diagnostics require supported Linux host'; exit 2;; esac
	@mkdir -p '$(JIT_DUMP_DIR)'
	@$(MAKE) --no-print-directory environment > '$(JIT_DUMP_DIR)/environment.log'
	@objdump --version > '$(JIT_DUMP_DIR)/objdump-version.log'
	@set -o pipefail; LUNA_JIT_DIAGNOSTIC_DIR='$(abspath $(JIT_DUMP_DIR))' $(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::dump_finalized_native_kernels -- --exact --ignored --nocapture 2>&1 | tee '$(JIT_DUMP_DIR)/test.log'
	@grep -Fq 'test jit::backend::memory_tests::dump_finalized_native_kernels ... ok' '$(JIT_DUMP_DIR)/test.log'
	@set -e; case "$$(uname -m)" in x86_64) machine='i386:x86-64';; aarch64) machine=aarch64;; esac; \
	for fixture in scalar table; do \
		grep -Fxq "arch=$$(uname -m)" '$(JIT_DUMP_DIR)'/$$fixture.metadata; \
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

jit-backend:
	@$(CARGO) test -p luna --features jit --test jit_backend $(TARGET_ARG)

jit-helpers:
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::helpers::tests

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

.PHONY: jit-call-canonical
jit-call-canonical:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::canonical::tests $(ARGS)

.PHONY: jit-call-pairs
jit-call-pairs:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::pairs::tests $(ARGS)

.PHONY: jit-call-runtime
jit-call-runtime:
	@$(CARGO) test --locked -p luna --features jit --test jit_call_pairs $(TARGET_ARG) $(ARGS)

jit-integer:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::integer::tests $(ARGS)

jit-integer-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' jit::integer::tests -- --test-threads=1 $(ARGS)

.PHONY: jit-projection jit-projection-miri
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
vm-activation-tests:
	@$(CARGO) test --locked -p luna --lib $(TARGET_ARG) thread::executor::activation_tests $(ARGS)

jit-activation-tests:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) thread::executor::activation_tests $(ARGS)

.PHONY: jit-activation-host-miri
jit-activation-host-miri:
	@$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' thread::executor::activation_tests::scoped_activation_host_releases_before_nested_executor_callbacks -- --exact --test-threads=1

vm-activation-miri:
	@$(CARGO) miri test --locked -p luna --lib --target '$(MIRI_TARGET)' thread::executor::activation_tests::native_and_interpreted_nested_executors_preserve_open_upvalues -- --exact --test-threads=1

jit-projection-runtime-miri:
	@set -e; for filter in jit::projection::native::tests jit::projection::tests jit::helpers::tests jit::abi::tests::original_scratch_entry; do \
		$(CARGO) miri test --locked -p luna --features jit --lib --target '$(MIRI_TARGET)' "$$filter" -- --test-threads=1; \
	done

jit-registry:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::registry::tests

jit-ir:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::ir::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::flow::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::work::tests
	@$(MAKE) --no-print-directory jit-tags
	@$(MAKE) --no-print-directory jit-access
	@$(MAKE) --no-print-directory jit-exits

jit-tags:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::tags::tests
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::corrupted_scalar_store_is_refused_before_codegen_and_mapping
	@$(MAKE) --no-print-directory jit-input

jit-input:
	@$(CARGO) test --locked -p luna --features jit --lib $(TARGET_ARG) jit::backend::memory_tests::omitted_numeric_guards_are_refused_before_codegen_and_mapping
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
	@$(CARGO) test -p luna --features jit --test jit_resources $(TARGET_ARG)

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

jit-verify: verify jit-check
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

# `--all-targets` covers the integration suites under tests/, which are the ones that actually
# exercise the Lua scripts; it does *not* cover doc tests, so `test-doc` runs beside it.
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
	@echo "  jit-test-modes Test Force preparation and explicit exclusions"
	@echo "  jit-numeric-exits Check numeric fallbacks between native work"
	@echo "  jit-registers Test register-255 and stack-256 boundaries"
	@echo "  jit-policy   Test quota refusal and configuration retirement"
	@echo "  jit-resources Test owned-container budgets and reclamation"
	@echo "  jit-example  Run the explicitly prepared native Lua example"
	@echo "  jit-bench    Measure checked workloads (ARGS='--mode off --samples 11')"
	@echo "  jit-bench-build Build the benchmark without timing"
	@echo "  jit-bench-run Time an existing artifact (JIT_BENCH_BINARY=path)"
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
	@echo "  jit-performance Check frozen paired workload thresholds"
	@echo "  jit-disassembly Dump finalized native kernels and addresses"
	@echo "  jit-shipping Publish checked paired timings at shipping opt-level s"
	@echo "  jit-size     Measure matched binary size and 5% disabled overhead"
	@echo "  jit-size-build Build/copy matched probes (SIZE_PROFILE=shipping|speed)"
	@echo "  jit-size-run Check existing matched probes without rebuilding"
	@echo "  jit-cost-tests Test feature-cost protocol and frozen overhead control"
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
