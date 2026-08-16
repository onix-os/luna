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

HAS_REL := $(shell command -v git-rel 2>/dev/null)

$(info ------------------------------------------)
$(info Project: $(PROJECT_NAME) v$(PROJECT_VERSION))
$(info ------------------------------------------)

.PHONY: build b dev compile c run r repl test t test-doc check check-all clippy clippy-strict rustdoc fmt fmt-check print-name clean verify publish release help h

# luna is a library pair, not a binary: `build` compiles the workspace and its examples, because
# the examples are the only executables here and they are what breaks first on an API change.
build:
	@$(CARGO) build --workspace --all-targets

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
	@$(CARGO) run --example $(EXAMPLE) -- $(ARGS)

r: run

repl:
	@$(CARGO) run --example interpreter -- $(ARGS)

# `--all-targets` covers the integration suites under tests/, which are the ones that actually
# exercise the Lua scripts; it does *not* cover doc tests, so `test-doc` runs beside it.
test:
	@$(CARGO) test --workspace --all-targets
	@$(MAKE) --no-print-directory test-doc

t: test

test-doc:
	@$(CARGO) test --workspace --doc

check:
	@$(CARGO) check --workspace --all-targets

check-all:
	@$(CARGO) check --workspace --all-targets --all-features

fmt:
	@$(CARGO) fmt --all

fmt-check:
	@$(CARGO) fmt --all -- --check

# Warnings are reported, not denied. The code inherited from upstream carries ~137 lints, and a
# gate that fails on all of them from day one is a gate nobody can run. `clippy-strict` is the
# version to switch `verify` to once that backlog is cleared.
clippy:
	@$(CARGO) clippy --workspace --all-targets

clippy-strict:
	@$(CARGO) clippy --workspace --all-targets -- -D warnings

rustdoc:
	@RUSTDOCFLAGS="-Dwarnings" $(CARGO) doc --workspace --no-deps

# Echoes the name parsed out of Cargo.toml. CI prints it because an empty name trips the $(error)
# above before any target runs, and that failure looks like nothing at all in a run summary.
print-name:
	@echo '$(PROJECT_NAME)'

clean:
	@$(CARGO) clean

# Clippy is deliberately not in the gate yet: the code inherited from upstream carries 135
# warnings and 2 deny-by-default `never_loop` errors in src/meta_ops.rs, so a `verify` that
# included it would be red on arrival and stop being run. Put it back once that is cleared.
verify: fmt-check check test rustdoc

# Order matters: luna-util depends on luna, so the registry has to have the new luna first.
publish:
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
	@echo "  publish      Publish $(PROJECT_NAME) then $(PROJECT_NAME)-util to crates.io"
	@echo "  release      Release a new version"
	@echo

h: help
