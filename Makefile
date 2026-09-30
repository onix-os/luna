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
JIT_BENCH_OPT ?= 3
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

HAS_REL := $(shell command -v git-rel 2>/dev/null)

$(info ------------------------------------------)
$(info Project: $(PROJECT_NAME) v$(PROJECT_VERSION))
$(info ------------------------------------------)

.PHONY: build b dev compile c run r repl test t test-all test-doc check check-all clippy clippy-strict rustdoc fmt fmt-check print-name clean verify publish release help h
.PHONY: jit-deps jit-check jit-test jit-test-all jit-test-doc jit-rustdoc jit-verify jit-tree jit-reference jit-backend jit-boundary jit-native jit-numeric jit-heap jit-registers jit-policy jit-resources jit-example jit-bench jit-bench-paired jit-performance jit-profile jit-profile-build jit-rust-assembly jit-fuzz-smoke jit-fuzz jit-evidence environment
.PHONY: ci-check jit-platform
.PHONY: jit-size jit-size-build jit-cost-tests jit-cost-native jit-shipping
.PHONY: jit-cost-profile
.PHONY: jit-disassembly

ci-check:
	@$(ACTIONLINT) .github/workflows/tests.yml

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

jit-bench:
	@mkdir -p target/jit-evidence
	@set -o pipefail; LUNA_BENCH_OPT_LEVEL=$(JIT_BENCH_OPT) CARGO_PROFILE_RELEASE_OPT_LEVEL=$(JIT_BENCH_OPT) $(CARGO) run --release --example jit_bench --features jit $(TARGET_ARG) -- $(ARGS) 2>&1 | tee target/jit-evidence/bench.log

jit-bench-paired:
	@$(MAKE) --no-print-directory jit-bench ARGS='--mode paired --samples 11 $(ARGS)'

jit-performance:
	@$(MAKE) --no-print-directory jit-bench JIT_BENCH_OPT=3 ARGS='--mode paired --samples 11 --check $(ARGS)'

jit-shipping:
	@$(MAKE) --no-print-directory jit-bench JIT_BENCH_OPT=s ARGS='--mode paired --samples 11 $(ARGS)'

jit-cost-tests:
	@$(CARGO) test --example jit_feature_cost --no-default-features $(TARGET_ARG)
	@$(CARGO) test --example jit_feature_cost --no-default-features --features jit $(TARGET_ARG)

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
	@set -o pipefail; $(COST_DIR)/jit-off --mode auto --iterations 1 2>&1 | tee $(COST_DIR)/native-proof.log

jit-size: jit-cost-native
	@set -o pipefail; $(COST_DIR)/no-jit --compare $(COST_DIR)/no-jit $(COST_DIR)/jit-off --samples $(COST_SAMPLES) --iterations $(COST_ITERATIONS) --check 2>&1 | tee $(COST_DIR)/cost.log

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
	@printf 'case=%s\niterations=3\nwarmups=2\ncollection=*run_vm*\nno_jit_mode=off\njit_mode=%s\n' '$(PROFILE_CASE)' '$(PROFILE_MODE)' >> $(COST_PROFILE_DIR)/profiler.log
	@set -e; for variant in no-jit jit-off; do \
		mode=off; if test "$$variant" = jit-off; then mode='$(PROFILE_MODE)'; fi; \
		valgrind --tool=callgrind --error-exitcode=99 --collect-atstart=no --toggle-collect='*run_vm*' --cache-sim=yes --branch-sim=yes --dump-instr=yes --callgrind-out-file=$(COST_PROFILE_DIR)/$$variant.callgrind \
			$(COST_DIR)/$$variant --worker --mode "$$mode" --case '$(PROFILE_CASE)' --iterations 3 > $(COST_PROFILE_DIR)/$$variant-profile.log 2>&1; \
		grep -Eq '^summary: [1-9][0-9]*' $(COST_PROFILE_DIR)/$$variant.callgrind; \
		callgrind_annotate --inclusive=no --threshold=99 $(COST_PROFILE_DIR)/$$variant.callgrind > $(COST_PROFILE_DIR)/$$variant-annotation.log; \
	done

jit-profile-build:
	@CARGO_PROFILE_RELEASE_OPT_LEVEL=3 CARGO_PROFILE_RELEASE_STRIP=false $(CARGO) build --release --example jit_bench --features jit

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

jit-backend:
	@$(CARGO) test -p luna --features jit --test jit_backend $(TARGET_ARG)

jit-native:
	@$(CARGO) test -p luna --features jit --test jit_native $(TARGET_ARG) $(ARGS)

jit-numeric:
	@$(CARGO) test -p luna --test numeric_semantics $(TARGET_ARG) $(ARGS)
	@$(CARGO) test -p luna --features jit --test jit_native $(TARGET_ARG) mixed_numeric
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::model::tests::mixed_numeric

jit-fuzz-smoke:
	@case "$$(uname -sm)" in 'Linux x86_64'|'Linux aarch64') ;; *) echo 'Native fuzz requires supported Linux host'; exit 2;; esac
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::fuzz::supervisor_rejects_panics_signals_and_timeouts -- --exact --ignored --nocapture
	@$(MAKE) --no-print-directory jit-fuzz FUZZ_CASES=24

jit-fuzz:
	@case "$$(uname -sm)" in 'Linux x86_64'|'Linux aarch64') ;; *) echo 'Native fuzz requires supported Linux host'; exit 2;; esac
	@mkdir -p target/jit-evidence/fuzz
	@$(MAKE) --no-print-directory environment > target/jit-evidence/fuzz/environment.log
	@set -o pipefail; LUNA_JIT_FUZZ_TARGET='$(FUZZ_TARGET)' LUNA_JIT_FUZZ_CASES='$(FUZZ_CASES)' LUNA_JIT_FUZZ_SEEDS='$(FUZZ_SEEDS)' $(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::fuzz::supervisor -- --exact --ignored --nocapture 2>&1 | tee target/jit-evidence/fuzz/latest.log

jit-heap:
	@$(CARGO) test -p luna --features jit --test jit_heap $(TARGET_ARG) $(ARGS)

jit-policy:
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::policy_tests
	@$(CARGO) test -p luna --features jit --test jit_policy $(TARGET_ARG)

jit-resources:
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit::resources
	@$(CARGO) test -p luna --features jit --test jit_resources $(TARGET_ARG)

jit-registers:
	@$(CARGO) test -p luna --test register_boundaries $(TARGET_ARG)
	@$(CARGO) test -p luna --lib $(TARGET_ARG) compiler::register_allocator::tests
	@$(CARGO) test -p luna --features jit --test register_boundaries $(TARGET_ARG)

jit-boundary:
	@$(CARGO) test -p luna --features jit --lib $(TARGET_ARG) jit:: $(ARGS)

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
	@echo "  jit-boundary Test native exits against the Rust boundary model"
	@echo "  jit-native   Run integrated native execution and lifecycle tests"
	@echo "  jit-numeric  Check reference and exact native numeric comparisons"
	@echo "  jit-fuzz-smoke Run supervised admission/scalar fuzz smoke"
	@echo "  jit-fuzz     Run bounded seeded campaigns (FUZZ_TARGET/CASES/SEEDS)"
	@echo "  jit-heap     Run native heap/upvalue mutation and GC tests"
	@echo "  jit-registers Test register-255 and stack-256 boundaries"
	@echo "  jit-policy   Test quota refusal and configuration retirement"
	@echo "  jit-resources Test owned-container budgets and reclamation"
	@echo "  jit-example  Run the explicitly prepared native Lua example"
	@echo "  jit-bench    Measure checked workloads (ARGS='--mode off --samples 11')"
	@echo "  jit-bench-paired Alternate checked Off/Auto samples"
	@echo "  jit-performance Check frozen paired workload thresholds"
	@echo "  jit-disassembly Dump finalized native kernels and addresses"
	@echo "  jit-shipping Publish checked paired timings at shipping opt-level s"
	@echo "  jit-size     Measure matched binary size and 5% disabled overhead"
	@echo "  jit-size-build Build/copy matched probes (SIZE_PROFILE=shipping|speed)"
	@echo "  jit-cost-tests Test feature-cost protocol and frozen overhead control"
	@echo "  jit-cost-native Verify usable native code in the matched JIT artifact"
	@echo "  jit-cost-profile Compare VM instruction/cache/branch counts (PROFILE_CASE=...)"
	@echo "  jit-profile  Profile the short-function workload (requires perf permission)"
	@echo "  jit-rust-assembly Inspect symbol-retained Rust dispatch/helper assembly"
	@echo "  publish      Publish $(PROJECT_NAME)-derive, $(PROJECT_NAME), then $(PROJECT_NAME)-util"
	@echo "  release      Release a new version"
	@echo

h: help
