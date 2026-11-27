# dbgen: build, test and code-quality tasks. `make` or `make help` lists them.
#
# Cargo runs on the host when it is at least Rust 1.100 (the crate's rust-version),
# otherwise inside Docker through scripts/cargo-docker.sh. Force either with
# `make CARGO=cargo ...` or `make CARGO=scripts/cargo-docker.sh ...`. Analyzers that are
# not installed are fetched on first use: cargo tools with `cargo install`, the others as
# Docker images or with npx.

SHELL := /bin/bash
.DEFAULT_GOAL := help

HOST_CARGO_OK := $(shell cargo --version 2>/dev/null | awk '{ split($$2, v, "."); if (v[1] > 1 || (v[1] == 1 && v[2] >= 100)) print "yes" }')
CARGO ?= $(if $(HOST_CARGO_OK),cargo,scripts/cargo-docker.sh)
FEATURES := clickhouse postgres mysql sqlite

# The test servers of docker-compose.yml (`make db-up`).
PG_URL ?= postgres://dbgen:dbgen@localhost:15432
MYSQL_URL ?= mysql://root:dbgen@localhost:13306
CH_URL ?= http://dbgen:dbgen@localhost:18123
SERVERS := DBGEN_TEST_POSTGRES_URL=$(PG_URL) DBGEN_TEST_MYSQL_URL=$(MYSQL_URL) DBGEN_TEST_CLICKHOUSE_URL=$(CH_URL)

# Arguments of `make run`.
ARGS ?= --help
IMAGE ?= dbgen
PREFIX ?= $(HOME)/.local
PRETTIER ?= $(if $(shell command -v npx 2>/dev/null),npx --yes prettier@3,docker run --rm -v "$(CURDIR):/work" -w /work node:22-alpine npx --yes prettier@3)
PRETTIER_FILES := "**/*.{md,yml,yaml,json}"
TRIVY_IMAGE ?= aquasec/trivy:latest
GITLEAKS_IMAGE ?= zricethezav/gitleaks:latest

# Installs cargo subcommand $(1) from crate $(2) when it is missing.
define cargo_tool
	@$(CARGO) $(1) --version >/dev/null 2>&1 || $(CARGO) install --locked $(2)
endef

# With a host toolchain, copies the release binary to bin/ (the Docker wrapper does it itself).
define copy_binary
	@if [ "$(CARGO)" = cargo ] && [ -x target/release/dbgen ]; then mkdir -p bin && cp target/release/dbgen bin/; fi
	@echo "Binary: bin/dbgen"
endef

.PHONY: help build release release-native run image install clean \
	test test-unit test-all test-postgres test-mysql test-sqlite test-clickhouse db-up db-down demo \
	fmt fmt-check prettier prettier-check lint lint-features doc \
	audit deny machete trivy trivy-image secrets analyze security ci

help: ## List the targets
	@awk 'BEGIN {FS = ":.*## "} /^## / {printf "\n%s\n", substr($$0, 4)} /^[a-zA-Z_-]+:.*## / {printf "  %-16s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo
	@echo "Cargo: $(CARGO)"

## Build

build: ## Debug build
	$(CARGO) build

release: ## Optimized build (opt-level 3, thin LTO, one codegen unit) to bin/dbgen
	$(CARGO) build --release
	$(copy_binary)

release-native: ## Optimized for this machine's CPU (faster, may not run on other CPUs)
	RUSTFLAGS="-C target-cpu=native" $(CARGO) build --release
	$(copy_binary)

run: release ## Build, then run dbgen with ARGS: make run ARGS="validate -c dbgen.yaml"
	bin/dbgen $(ARGS)

image: ## Self-contained Docker image (IMAGE=dbgen)
	docker build -t $(IMAGE) .

install: release ## Install bin/dbgen into PREFIX/bin (PREFIX=~/.local)
	install -Dm755 bin/dbgen "$(PREFIX)/bin/dbgen"

clean: ## Remove build outputs
	$(CARGO) clean
	rm -rf bin

## Test

test: ## Unit tests and the tests that need no server (SQLite, fixture compilation)
	$(CARGO) test

test-unit: ## Unit tests only
	$(CARGO) test --lib

test-all: db-up ## Every test, against the servers of docker-compose.yml
	$(SERVERS) $(CARGO) test

test-postgres: ## PostgreSQL conformance tests
	$(SERVERS) $(CARGO) test --test postgres

test-mysql: ## MySQL conformance tests
	$(SERVERS) $(CARGO) test --test mysql

test-sqlite: ## SQLite conformance tests
	$(CARGO) test --test sqlite

test-clickhouse: ## ClickHouse tests
	$(SERVERS) $(CARGO) test --test clickhouse --test clickhouse_conformance

db-up: ## Start the test servers that are not running, and wait until they are healthy
	@for s in postgres mysql clickhouse; do \
		if [ "$$(docker inspect -f '{{.State.Running}}' dbgen-$$s 2>/dev/null)" = true ]; then echo "dbgen-$$s: running"; \
		else docker compose up -d --wait $$s || exit 1; fi; \
	done

db-down: ## Stop the test servers (their data volumes are kept)
	docker compose down

demo: release ## Load the shop example into SQLite and verify it
	bin/dbgen apply -c examples/shop/sqlite.yaml --create-database
	bin/dbgen generate -c examples/shop/sqlite.yaml --fresh --verify

## Code quality

fmt: ## Format the Rust code (rustfmt.toml)
	$(CARGO) fmt --all

fmt-check: ## Check the Rust formatting, changing nothing
	$(CARGO) fmt --all -- --check

prettier: ## Format Markdown, YAML and JSON (Prettier)
	$(PRETTIER) --write $(PRETTIER_FILES)

prettier-check: ## Check Markdown, YAML and JSON formatting, changing nothing
	$(PRETTIER) --check $(PRETTIER_FILES)

lint: ## Clippy on every target, warnings as errors
	$(CARGO) clippy --all-targets -- -D warnings

lint-features: ## Clippy with no connector and with each connector alone
	$(CARGO) clippy --no-default-features --lib --bins -- -D warnings
	@for f in $(FEATURES); do echo "== feature $$f"; $(CARGO) clippy --no-default-features --features $$f --lib --bins -- -D warnings || exit 1; done

doc: ## API documentation, warnings as errors (target/doc)
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --no-deps --document-private-items

## Security

audit: ## Known vulnerabilities in dependencies (cargo-audit, RustSec database)
	$(call cargo_tool,audit,cargo-audit)
	$(CARGO) audit

deny: ## Advisories, licenses, banned and duplicate crates, sources (cargo-deny, deny.toml)
	$(call cargo_tool,deny,cargo-deny)
	$(CARGO) deny check

machete: ## Unused dependencies (cargo-machete)
	$(call cargo_tool,machete,cargo-machete)
	$(CARGO) machete

# Trivy fails on HIGH and CRITICAL findings that have a fix; findings without one are
# listed but cannot be acted on, so they do not fail the build.
trivy: ## Vulnerabilities, secrets and misconfigurations in the repository (Trivy)
	docker run --rm -v "$(CURDIR):/src:ro" -v dbgen-trivy-cache:/root/.cache $(TRIVY_IMAGE) fs \
		--scanners vuln,secret,misconfig --skip-dirs /src/target --skip-dirs /src/bin --exit-code 1 --severity HIGH,CRITICAL --ignore-unfixed /src

trivy-image: image ## Vulnerabilities in the Docker image (Trivy)
	docker run --rm -v /var/run/docker.sock:/var/run/docker.sock -v dbgen-trivy-cache:/root/.cache $(TRIVY_IMAGE) image \
		--exit-code 1 --severity HIGH,CRITICAL --ignore-unfixed $(IMAGE)

secrets: ## Credentials in the files and the git history (Gitleaks)
	docker run --rm -v "$(CURDIR):/repo:ro" $(GITLEAKS_IMAGE) git /repo --no-banner --redact --config /repo/.gitleaks.toml
	docker run --rm -v "$(CURDIR):/repo:ro" $(GITLEAKS_IMAGE) dir /repo --no-banner --redact --config /repo/.gitleaks.toml

analyze: fmt-check prettier-check lint doc machete ## Every code analyzer (use `make -k analyze` to see all failures)

security: audit deny secrets trivy ## Every security scanner

ci: lint lint-features doc test-all audit deny ## What a CI pipeline should run
