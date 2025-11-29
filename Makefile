# XEarthLayer Makefile
# Single interface for all project operations

# Configuration
RUST_BACKTRACE := 1
CARGO := cargo
CARGO_FLAGS :=
COVERAGE_MIN := 80

# Colors for output
RED := \033[0;31m
GREEN := \033[0;32m
YELLOW := \033[0;33m
BLUE := \033[0;36m
NC := \033[0m # No Color

.DEFAULT_GOAL := help

##@ Help

.PHONY: help
help: ## Show this help message
	@echo '$(BLUE)XEarthLayer - Available Make Targets$(NC)'
	@echo ''
	@awk 'BEGIN {FS = ":.*##"; printf "Usage:\n  make $(BLUE)<target>$(NC)\n"} /^[a-zA-Z_0-9-]+:.*?##/ { printf "  $(BLUE)%-20s$(NC) %s\n", $$1, $$2 } /^##@/ { printf "\n$(YELLOW)%s$(NC)\n", substr($$0, 5) } ' $(MAKEFILE_LIST)

##@ Development

.PHONY: init
init: ## Initialize development environment
	@echo "$(BLUE)Initializing development environment...$(NC)"
	@command -v cargo >/dev/null 2>&1 || { echo "$(RED)Error: cargo not found. Install Rust from https://rustup.rs/$(NC)"; exit 1; }
	@command -v rustfmt >/dev/null 2>&1 || rustup component add rustfmt
	@command -v clippy >/dev/null 2>&1 || rustup component add clippy
	@echo "$(GREEN)Development environment ready!$(NC)"

.PHONY: dev
dev: ## Run in development mode
	$(CARGO) run $(CARGO_FLAGS)

.PHONY: run
run: build ## Run the application (debug build)
	$(CARGO) run $(CARGO_FLAGS)

.PHONY: watch
watch: ## Watch for changes and run tests automatically
	@command -v cargo-watch >/dev/null 2>&1 || { echo "$(YELLOW)Installing cargo-watch...$(NC)"; cargo install cargo-watch; }
	cargo watch -x test -x check

##@ Building

.PHONY: check
check: ## Fast compilation check (no codegen)
	@echo "$(BLUE)Running fast compilation check...$(NC)"
	$(CARGO) check $(CARGO_FLAGS)

.PHONY: build
build: ## Build debug version
	@echo "$(BLUE)Building debug version...$(NC)"
	$(CARGO) build $(CARGO_FLAGS)

.PHONY: release
release: verify ## Build optimized release version
	@echo "$(BLUE)Building release version...$(NC)"
	$(CARGO) build --release $(CARGO_FLAGS)
	@echo "$(GREEN)Release build complete!$(NC)"

.PHONY: clean
clean: ## Remove build artifacts
	@echo "$(BLUE)Cleaning build artifacts...$(NC)"
	$(CARGO) clean
	@rm -rf target/
	@echo "$(GREEN)Clean complete!$(NC)"

##@ Testing

.PHONY: test
test: ## Run all tests
	@echo "$(BLUE)Running all tests...$(NC)"
	RUST_BACKTRACE=$(RUST_BACKTRACE) $(CARGO) test $(CARGO_FLAGS) -- --nocapture

.PHONY: test-unit
test-unit: ## Run unit tests only
	@echo "$(BLUE)Running unit tests...$(NC)"
	RUST_BACKTRACE=$(RUST_BACKTRACE) $(CARGO) test $(CARGO_FLAGS) --lib -- --nocapture

.PHONY: integration-tests
integration-tests: build ## Run integration tests only (requires built binary)
	@echo "$(BLUE)Running integration tests...$(NC)"
	RUST_BACKTRACE=$(RUST_BACKTRACE) $(CARGO) test $(CARGO_FLAGS) --test '*' -- --ignored --nocapture
	@echo "$(GREEN)Integration tests complete!$(NC)"

.PHONY: test-doc
test-doc: ## Run documentation tests
	@echo "$(BLUE)Running documentation tests...$(NC)"
	$(CARGO) test $(CARGO_FLAGS) --doc

.PHONY: test-all
test-all: test test-doc ## Run all tests including doc tests
	@echo "$(GREEN)All tests complete!$(NC)"

.PHONY: coverage
coverage: ## Generate test coverage report
	@echo "$(BLUE)Generating test coverage report...$(NC)"
	@command -v cargo-tarpaulin >/dev/null 2>&1 || { echo "$(YELLOW)Installing cargo-tarpaulin...$(NC)"; cargo install cargo-tarpaulin; }
	$(CARGO) tarpaulin --out Html --out Lcov --output-dir target/coverage --line --branch
	@echo "$(GREEN)Coverage report generated at target/coverage/index.html$(NC)"
	@echo "$(BLUE)Opening coverage report...$(NC)"
	@xdg-open target/coverage/index.html 2>/dev/null || open target/coverage/index.html 2>/dev/null || echo "$(YELLOW)Please open target/coverage/index.html manually$(NC)"

.PHONY: coverage-check
coverage-check: ## Check if coverage meets minimum threshold
	@echo "$(BLUE)Checking coverage threshold (minimum: $(COVERAGE_MIN)%)...$(NC)"
	@command -v cargo-tarpaulin >/dev/null 2>&1 || { echo "$(YELLOW)Installing cargo-tarpaulin...$(NC)"; cargo install cargo-tarpaulin; }
	@$(CARGO) tarpaulin --out Stdout --fail-under $(COVERAGE_MIN) || { echo "$(RED)Coverage below $(COVERAGE_MIN)%!$(NC)"; exit 1; }
	@echo "$(GREEN)Coverage meets minimum threshold!$(NC)"

##@ Code Quality

.PHONY: format
format: ## Format code with rustfmt
	@echo "$(BLUE)Formatting code...$(NC)"
	$(CARGO) fmt
	@echo "$(GREEN)Code formatted!$(NC)"

.PHONY: format-check
format-check: ## Check code formatting without modifying
	@echo "$(BLUE)Checking code formatting...$(NC)"
	$(CARGO) fmt -- --check

.PHONY: lint
lint: ## Run clippy linter
	@echo "$(BLUE)Running clippy linter...$(NC)"
	$(CARGO) clippy $(CARGO_FLAGS) -- -D warnings

.PHONY: lint-fix
lint-fix: ## Run clippy and automatically fix issues
	@echo "$(BLUE)Running clippy with auto-fix...$(NC)"
	$(CARGO) clippy $(CARGO_FLAGS) --fix --allow-dirty --allow-staged

.PHONY: verify
verify: format-check lint test ## Run all verification checks (format, lint, test)
	@echo "$(GREEN)All verification checks passed!$(NC)"

.PHONY: pre-commit
pre-commit: format verify ## Run pre-commit checks (format + verify)
	@echo "$(GREEN)Pre-commit checks passed! Ready to commit.$(NC)"

##@ Documentation

.PHONY: doc
doc: ## Generate documentation
	@echo "$(BLUE)Generating documentation...$(NC)"
	$(CARGO) doc --no-deps $(CARGO_FLAGS)
	@echo "$(GREEN)Documentation generated!$(NC)"

.PHONY: doc-open
doc-open: doc ## Generate and open documentation in browser
	@echo "$(BLUE)Opening documentation...$(NC)"
	$(CARGO) doc --no-deps --open $(CARGO_FLAGS)

.PHONY: doc-all
doc-all: ## Generate documentation including dependencies
	@echo "$(BLUE)Generating documentation with dependencies...$(NC)"
	$(CARGO) doc $(CARGO_FLAGS)

##@ Dependencies

.PHONY: deps-check
deps-check: ## Check for outdated dependencies
	@echo "$(BLUE)Checking for outdated dependencies...$(NC)"
	@command -v cargo-outdated >/dev/null 2>&1 || { echo "$(YELLOW)Installing cargo-outdated...$(NC)"; cargo install cargo-outdated; }
	$(CARGO) outdated

.PHONY: deps-update
deps-update: ## Update dependencies
	@echo "$(BLUE)Updating dependencies...$(NC)"
	$(CARGO) update
	@echo "$(GREEN)Dependencies updated!$(NC)"

.PHONY: audit
audit: ## Run security audit
	@echo "$(BLUE)Running security audit...$(NC)"
	@command -v cargo-audit >/dev/null 2>&1 || { echo "$(YELLOW)Installing cargo-audit...$(NC)"; cargo install cargo-audit; }
	$(CARGO) audit
	@echo "$(GREEN)Security audit complete!$(NC)"

##@ Benchmarking

.PHONY: bench
bench: ## Run benchmarks
	@echo "$(BLUE)Running benchmarks...$(NC)"
	$(CARGO) bench $(CARGO_FLAGS)

##@ CI/CD

.PHONY: ci
ci: verify coverage-check ## Run all CI checks (verify + coverage check)
	@echo "$(GREEN)All CI checks passed!$(NC)"

.PHONY: ci-full
ci-full: verify coverage-check audit deps-check ## Run comprehensive CI checks
	@echo "$(GREEN)All comprehensive CI checks passed!$(NC)"

##@ Installation

.PHONY: install
install: release ## Install binary to system
	@echo "$(BLUE)Installing xearthlayer...$(NC)"
	$(CARGO) install --path .
	@echo "$(GREEN)Installation complete!$(NC)"

.PHONY: uninstall
uninstall: ## Uninstall binary from system
	@echo "$(BLUE)Uninstalling xearthlayer...$(NC)"
	$(CARGO) uninstall xearthlayer
	@echo "$(GREEN)Uninstall complete!$(NC)"

##@ Maintenance

.PHONY: clean-all
clean-all: clean ## Deep clean (including cargo cache)
	@echo "$(BLUE)Performing deep clean...$(NC)"
	$(CARGO) clean
	@rm -rf target/
	@rm -rf Cargo.lock
	@echo "$(GREEN)Deep clean complete!$(NC)"

.PHONY: reset
reset: clean-all init ## Reset environment and reinitialize
	@echo "$(GREEN)Environment reset complete!$(NC)"
