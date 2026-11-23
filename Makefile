# docgait: common tasks. `make help` lists them.

SHELL := /bin/bash
DESKTOP := apps/desktop
PNPM := pnpm --dir $(DESKTOP)

# Find cargo even when rustup's env isn't sourced in this shell.
CARGO := $(or $(shell command -v cargo 2>/dev/null),$(wildcard $(HOME)/.cargo/bin/cargo),cargo)
export PATH := $(dir $(CARGO)):$(PATH)

UNAME := $(shell uname -s)
ifeq ($(UNAME),Darwin)
  BUNDLES := app
  APP_OUT := target/release/bundle/macos/docgait.app
else
  BUNDLES := deb
  APP_OUT := target/release/bundle/deb/
endif

.DEFAULT_GOAL := help
.PHONY: help install build build-cli build-app dev test lint fmt check run demo clean

help: ## List targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*## "} {printf "  \033[1m%-10s\033[0m %s\n", $$1, $$2}'

install: ## Install frontend dependencies
	$(PNPM) install --frozen-lockfile

build: build-cli build-app ## Production build: CLI + desktop app (.app on macOS, .deb on Linux)

build-cli: ## Release build of the CLI -> target/release/docgait
	$(CARGO) build --release -p docgait-cli
	@echo "CLI: target/release/docgait"

build-app: install ## Release build of the desktop app
	$(PNPM) tauri build --bundles $(BUNDLES)
	@echo "App: $(APP_OUT)"

dev: install ## Run the desktop app with hot reload
	$(PNPM) tauri dev

test: ## Run the Rust tests
	$(CARGO) test -p docgait-core -p docgait-cli

lint: install ## Same checks as CI's Lint job
	$(PNPM) format:check
	$(PNPM) lint
	$(PNPM) build
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt: install ## Format Rust and frontend code
	$(CARGO) fmt --all
	$(PNPM) format

check: lint test ## Everything CI runs (lint + tests)

run: build-app ## Build and open the desktop app (macOS)
ifeq ($(UNAME),Darwin)
	open $(APP_OUT)
else
	@echo "Install with: sudo apt install ./$(APP_OUT)docgait_*.deb"
endif

DEMO := $(CURDIR)/target/demo
demo: build-app ## Fresh test data in target/demo, then open the app on it (your real data is untouched)
	python3 scripts/demo-data.py $(DEMO)
	@echo "Library folder: $(DEMO)/Library   Sources: $(DEMO)/drives/CopyA, CopyB, CopyC"
ifeq ($(UNAME),Darwin)
	DOCGAIT_DATA_DIR=$(DEMO)/data DOCGAIT_TRASH_DIR=$(DEMO)/trash $(APP_OUT)/Contents/MacOS/docgait-desktop
else
	DOCGAIT_DATA_DIR=$(DEMO)/data DOCGAIT_TRASH_DIR=$(DEMO)/trash target/release/docgait-desktop
endif

clean: ## Remove build output
	$(CARGO) clean
	rm -rf $(DESKTOP)/dist
