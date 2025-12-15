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

##@ Packaging

# Package configuration
PKG_NAME := xearthlayer
PKG_VERSION := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
AUR_DIR := pkg/aur
AUR_REPO := ssh://aur@aur.archlinux.org/$(PKG_NAME).git

.PHONY: pkg-deb
pkg-deb: release ## Build Debian package (.deb)
	@echo "$(BLUE)Building Debian package...$(NC)"
	@command -v cargo-deb >/dev/null 2>&1 || { echo "$(YELLOW)Installing cargo-deb...$(NC)"; cargo install cargo-deb; }
	cd xearthlayer-cli && $(CARGO) deb --no-build
	@echo "$(GREEN)Debian package built: target/debian/$(PKG_NAME)_$(PKG_VERSION)-1_amd64.deb$(NC)"

.PHONY: pkg-rpm
pkg-rpm: release ## Build RPM package (.rpm) - requires rpmbuild
	@echo "$(BLUE)Building RPM package...$(NC)"
	@command -v rpmbuild >/dev/null 2>&1 || { echo "$(RED)Error: rpmbuild not found. Install rpm-build package.$(NC)"; exit 1; }
	@mkdir -p ~/rpmbuild/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
	@mkdir -p $(PKG_NAME)-$(PKG_VERSION)
	@cp -r * $(PKG_NAME)-$(PKG_VERSION)/ 2>/dev/null || true
	@tar czvf ~/rpmbuild/SOURCES/$(PKG_NAME)-$(PKG_VERSION).tar.gz $(PKG_NAME)-$(PKG_VERSION)
	@rm -rf $(PKG_NAME)-$(PKG_VERSION)
	@cp pkg/rpm/$(PKG_NAME).spec ~/rpmbuild/SPECS/
	rpmbuild -bb ~/rpmbuild/SPECS/$(PKG_NAME).spec
	@echo "$(GREEN)RPM package built in ~/rpmbuild/RPMS/$(NC)"

.PHONY: pkg-tarball
pkg-tarball: release ## Build release tarball
	@echo "$(BLUE)Building release tarball...$(NC)"
	@mkdir -p dist
	@cp target/release/xearthlayer dist/
	@cp README.md LICENSE dist/
	@cd dist && tar czvf $(PKG_NAME)-$(PKG_VERSION)-x86_64-linux.tar.gz xearthlayer README.md LICENSE
	@rm dist/xearthlayer dist/README.md dist/LICENSE
	@echo "$(GREEN)Tarball built: dist/$(PKG_NAME)-$(PKG_VERSION)-x86_64-linux.tar.gz$(NC)"

.PHONY: aur-prepare
aur-prepare: ## Prepare AUR package for a tagged release (TAG=v0.2.0)
	@if [ -z "$(TAG)" ]; then echo "$(RED)Error: TAG is required. Usage: make aur-prepare TAG=v0.2.0$(NC)"; exit 1; fi
	@echo "$(BLUE)Preparing AUR package for $(TAG)...$(NC)"
	@# Extract version from tag (remove 'v' prefix)
	$(eval VERSION := $(shell echo $(TAG) | sed 's/^v//'))
	@# Create AUR working directory
	@rm -rf $(AUR_DIR)
	@mkdir -p $(AUR_DIR)
	@# Download source tarball from GitHub
	@echo "$(BLUE)Downloading source tarball...$(NC)"
	@curl -sL "https://github.com/samsoir/xearthlayer/archive/$(TAG).tar.gz" -o "$(AUR_DIR)/$(PKG_NAME)-$(VERSION).tar.gz"
	@# Calculate SHA256
	$(eval SHA256 := $(shell sha256sum "$(AUR_DIR)/$(PKG_NAME)-$(VERSION).tar.gz" | cut -d' ' -f1))
	@echo "$(BLUE)SHA256: $(SHA256)$(NC)"
	@# Generate PKGBUILD with correct version and checksum
	@sed -e 's/^pkgver=.*/pkgver=$(VERSION)/' \
	     -e "s/^sha256sums=.*/sha256sums=('$(SHA256)')/" \
	     pkg/arch/PKGBUILD > $(AUR_DIR)/PKGBUILD
	@# Generate .SRCINFO
	@cd $(AUR_DIR) && makepkg --printsrcinfo > .SRCINFO
	@# Clean up downloaded tarball (not needed for AUR)
	@rm "$(AUR_DIR)/$(PKG_NAME)-$(VERSION).tar.gz"
	@echo ""
	@echo "$(GREEN)AUR package prepared in $(AUR_DIR)/$(NC)"
	@echo "$(BLUE)Contents:$(NC)"
	@ls -la $(AUR_DIR)/
	@echo ""
	@echo "$(YELLOW)Next steps:$(NC)"
	@echo "  1. Review the PKGBUILD: cat $(AUR_DIR)/PKGBUILD"
	@echo "  2. Test locally: cd $(AUR_DIR) && makepkg -si"
	@echo "  3. Submit to AUR: make aur-publish TAG=$(TAG)"

.PHONY: aur-publish
aur-publish: ## Publish prepared AUR package (TAG=v0.2.0)
	@if [ -z "$(TAG)" ]; then echo "$(RED)Error: TAG is required. Usage: make aur-publish TAG=v0.2.0$(NC)"; exit 1; fi
	@if [ ! -f "$(AUR_DIR)/PKGBUILD" ]; then echo "$(RED)Error: Run 'make aur-prepare TAG=$(TAG)' first$(NC)"; exit 1; fi
	@echo "$(BLUE)Publishing AUR package for $(TAG)...$(NC)"
	$(eval VERSION := $(shell echo $(TAG) | sed 's/^v//'))
	@# Clone or update AUR repo
	@if [ -d "$(AUR_DIR)/aur-repo" ]; then \
		echo "$(BLUE)Updating existing AUR repo...$(NC)"; \
		cd $(AUR_DIR)/aur-repo && git pull; \
	else \
		echo "$(BLUE)Cloning AUR repo...$(NC)"; \
		git clone $(AUR_REPO) $(AUR_DIR)/aur-repo || { \
			echo "$(YELLOW)AUR repo doesn't exist yet. Creating new package...$(NC)"; \
			mkdir -p $(AUR_DIR)/aur-repo && cd $(AUR_DIR)/aur-repo && git init; \
		}; \
	fi
	@# Copy PKGBUILD and .SRCINFO to AUR repo
	@cp $(AUR_DIR)/PKGBUILD $(AUR_DIR)/.SRCINFO $(AUR_DIR)/aur-repo/
	@# Commit and push
	@cd $(AUR_DIR)/aur-repo && \
		git add PKGBUILD .SRCINFO && \
		git commit -m "Update to $(VERSION)" && \
		echo "$(BLUE)Pushing to AUR...$(NC)" && \
		git push origin master
	@echo ""
	@echo "$(GREEN)Successfully published $(PKG_NAME) $(VERSION) to AUR!$(NC)"
	@echo "$(BLUE)View at: https://aur.archlinux.org/packages/$(PKG_NAME)$(NC)"

.PHONY: aur-test
aur-test: ## Test AUR package build locally (TAG=v0.2.0)
	@if [ -z "$(TAG)" ]; then echo "$(RED)Error: TAG is required. Usage: make aur-test TAG=v0.2.0$(NC)"; exit 1; fi
	@if [ ! -f "$(AUR_DIR)/PKGBUILD" ]; then \
		echo "$(YELLOW)AUR package not prepared. Running aur-prepare first...$(NC)"; \
		$(MAKE) aur-prepare TAG=$(TAG); \
	fi
	@echo "$(BLUE)Testing AUR package build...$(NC)"
	@cd $(AUR_DIR) && makepkg -sf
	@echo "$(GREEN)AUR package built successfully!$(NC)"
	@echo "$(BLUE)Package:$(NC)"
	@ls -la $(AUR_DIR)/*.pkg.tar.zst 2>/dev/null || ls -la $(AUR_DIR)/*.pkg.tar.xz 2>/dev/null

.PHONY: pkg-all
pkg-all: pkg-deb pkg-tarball ## Build all packages (deb + tarball)
	@echo "$(GREEN)All packages built!$(NC)"

##@ Release Management

.PHONY: version
version: ## Show current version
	@echo "$(BLUE)Current version:$(NC) $(PKG_VERSION)"

.PHONY: bump-version
bump-version: ## Bump version across all files (VERSION=x.y.z)
	@if [ -z "$(VERSION)" ]; then echo "$(RED)Error: VERSION is required. Usage: make bump-version VERSION=0.3.0$(NC)"; exit 1; fi
	@echo "$(BLUE)Bumping version to $(VERSION)...$(NC)"
	@# Update workspace Cargo.toml
	@sed -i 's/^version = ".*"/version = "$(VERSION)"/' Cargo.toml
	@# Update RPM spec file
	@sed -i 's/^Version:.*/Version:        $(VERSION)/' pkg/rpm/xearthlayer.spec
	@echo "$(GREEN)Version bumped to $(VERSION)$(NC)"
	@echo ""
	@echo "$(YELLOW)Next steps:$(NC)"
	@echo "  1. Update CHANGELOG.md with release notes"
	@echo "  2. Commit: git add -A && git commit -m 'Bump version to $(VERSION)'"
	@echo "  3. Tag: git tag v$(VERSION)"
	@echo "  4. Push: git push origin HEAD && git push origin v$(VERSION)"

.PHONY: release-checklist
release-checklist: ## Show release checklist
	@echo "$(BLUE)Release Checklist$(NC)"
	@echo ""
	@echo "  1. [ ] Ensure all tests pass: make verify"
	@echo "  2. [ ] Update version: make bump-version VERSION=x.y.z"
	@echo "  3. [ ] Update CHANGELOG.md with release notes"
	@echo "  4. [ ] Commit version bump"
	@echo "  5. [ ] Push to release branch and verify CI passes"
	@echo "  6. [ ] Create and push tag: git tag vx.y.z && git push origin vx.y.z"
	@echo "  7. [ ] Review and publish draft release on GitHub"
	@echo "  8. [ ] (Optional) Publish to AUR: make aur-publish TAG=vx.y.z"
	@echo ""

##@ Maintenance

.PHONY: clean-all
clean-all: clean ## Deep clean (including cargo cache)
	@echo "$(BLUE)Performing deep clean...$(NC)"
	$(CARGO) clean
	@rm -rf target/
	@rm -rf Cargo.lock
	@rm -rf dist/
	@rm -rf $(AUR_DIR)/
	@echo "$(GREEN)Deep clean complete!$(NC)"

.PHONY: reset
reset: clean-all init ## Reset environment and reinitialize
	@echo "$(GREEN)Environment reset complete!$(NC)"
