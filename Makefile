.PHONY: up down build test fmt lint check run bench

up: ## Start local Postgres
	docker compose up -d

down: ## Stop local Postgres
	docker compose down

build: ## Build in release mode
	cargo build --release

test: ## Run unit + integration tests (needs `make up` and DATABASE_URL set first)
	cargo test

fmt: ## Format code
	cargo fmt

lint: ## Run clippy with warnings denied
	cargo clippy --all-targets --all-features -- -D warnings

check: ## Run everything CI runs, before opening a PR
	cargo fmt --check
	$(MAKE) lint
	$(MAKE) test

run: ## Run the server (expects DATABASE_URL / .env set)
	cargo run --release -- serve --port 8080

bench: ## Run the Phase 1 exit-criteria benchmark against a running server
	./scripts/benchmark.sh

resilience: ## Run the osquery-failure resilience test (needs DATABASE_URL, psql)
	./scripts/resilience-test.sh

events-bench: ## Run the Phase 4 events volume benchmark (needs DATABASE_URL, psql)
	./scripts/events-benchmark.sh
