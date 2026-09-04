# Kernos: one command per job. Every target works from a fresh clone with the
# toolchains installed (Rust stable, Go 1.22+, Python 3.10+, Node 18+).

SHELL := /bin/bash
export PATH := $(HOME)/.cargo/bin:/opt/homebrew/bin:$(PATH)

PY      ?= python3
VENV    ?= sdk/python/.venv
PYBIN   := $(VENV)/bin

.PHONY: help build build-rust build-go build-python build-ts test test-rust test-go test-python test-ts \
        lint fmt accept clean release-check leak-scan versions docs docs-build

help:
	@echo "build          build every component (kernel, gateway, python sdk, ts sdk)"
	@echo "test           unit and integration tests for every component"
	@echo "accept         the end-to-end acceptance suite (tests/acceptance)"
	@echo "lint           clippy, go vet, ruff, tsc"
	@echo "fmt            rustfmt, gofmt, ruff format"
	@echo "release-check  versions in sync, changelog entry present, leak scan"
	@echo "leak-scan      gitleaks over the full history"
	@echo "docs           serve the documentation site locally"

# ---------------------------------------------------------------- build ----
build: build-rust build-go build-python build-ts

build-rust:
	cargo build --release --workspace

build-go:
	cd gateway && go build -o bin/kernos-gateway ./cmd/kernos-gateway

$(VENV)/bin/activate:
	$(PY) -m venv $(VENV)
	$(PYBIN)/pip install --quiet --upgrade pip

build-python: $(VENV)/bin/activate
	$(PYBIN)/pip install --quiet -e "sdk/python[dev]"

build-ts:
	cd sdk/typescript && npm ci --silent && npm run build --silent

# ----------------------------------------------------------------- test ----
test: test-rust test-go test-python test-ts

test-rust:
	cargo test --workspace

test-go:
	cd gateway && go test ./... -count=1

test-python: build-python
	cd sdk/python && $(CURDIR)/$(PYBIN)/python -m pytest -q

# npm test runs against the built package, and neither node_modules nor dist
# exist in a fresh checkout, so the build has to come first.
test-ts: build-ts
	cd sdk/typescript && npm test --silent

# ----------------------------------------------------------- acceptance ----
accept: build
	KERNOS_BIN=$(CURDIR)/target/release/kernos \
	KERNOS_GATEWAY_BIN=$(CURDIR)/gateway/bin/kernos-gateway \
	PATH="$(CURDIR)/$(PYBIN):$$PATH" \
	$(PYBIN)/python tests/acceptance/run.py $(ACCEPT_ARGS)

# ----------------------------------------------------------- lint, fmt ----
lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cd gateway && test -z "$$(gofmt -l .)" && go vet ./...
	cd sdk/python && $(CURDIR)/$(PYBIN)/ruff check src tests
	cd sdk/typescript && npm run check --silent

fmt:
	cargo fmt --all
	cd gateway && gofmt -w .
	cd sdk/python && $(CURDIR)/$(PYBIN)/ruff format src tests

# -------------------------------------------------------------- release ----
versions:
	$(PY) scripts/check_versions.py

release-check: versions leak-scan
	@echo "release check passed"

leak-scan:
	gitleaks git --redact --no-banner . || (echo "leak scan failed"; exit 1)

# ----------------------------------------------------------------- docs ----
DOCS_VENV ?= .venv-docs

$(DOCS_VENV)/bin/mkdocs:
	$(PY) -m venv $(DOCS_VENV)
	$(DOCS_VENV)/bin/pip install --quiet -r docs/requirements.txt

docs: $(DOCS_VENV)/bin/mkdocs
	cp CHANGELOG.md docs/changelog.md
	$(DOCS_VENV)/bin/mkdocs serve

docs-build: $(DOCS_VENV)/bin/mkdocs
	cp CHANGELOG.md docs/changelog.md
	$(DOCS_VENV)/bin/mkdocs build --strict

clean:
	rm -rf target gateway/bin sdk/python/dist sdk/python/build sdk/typescript/dist tests/acceptance/.work site docs/changelog.md
