# ccaudit release automation.
#
# Cross-compiles the binary for 4 targets using cargo-zigbuild, stages
# each into the matching npm/<platform>/ directory, and syncs versions
# from Cargo.toml. Nothing here publishes — run `npm publish` by hand
# once you're happy (platform packages first, then the main package).
#
#   make setup    one-time: install cargo-zigbuild, add rustup targets
#   make cross    build all 4 targets (~5 min clean, ~1 min incremental)
#   make stage    copy binaries into npm/<platform>/
#   make release  cross + sync-versions + stage + check
#   make check    verify every platform binary exists and is executable
#   make clean-npm  remove staged binaries

SHELL := bash

# Single source of truth — Cargo.toml drives npm versions.
PKG_VERSION := $(shell sed -n 's/^version *= *"\(.*\)".*/\1/p' Cargo.toml | head -n 1)

# rust triple → npm dir. musl on Linux so the binary runs on Alpine and
# any glibc distro without dynamic dependency drama.
DARWIN_ARM64 := aarch64-apple-darwin
DARWIN_X64   := x86_64-apple-darwin
LINUX_X64    := x86_64-unknown-linux-musl
LINUX_ARM64  := aarch64-unknown-linux-musl

TARGETS := $(DARWIN_ARM64) $(DARWIN_X64) $(LINUX_X64) $(LINUX_ARM64)

# Which feature set ships via npm. "full" = TUI + web + locale (~1.1 MB).
# Set CCAUDIT_DIST_FEATURES= to ship the lean variant instead.
CCAUDIT_DIST_FEATURES ?= --features full

CARGO_FLAGS := --release $(CCAUDIT_DIST_FEATURES)

# Host triple. For the host target we use plain `cargo build`, because
# zigbuild's linker config drops CoreFoundation on macOS — chrono's Local
# timezone needs CF on Darwin, so the "native" path is both simpler and
# the only one that links. Foreign targets go through zigbuild.
UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)
ifeq ($(UNAME_S),Darwin)
  ifeq ($(UNAME_M),arm64)
    HOST_TRIPLE := aarch64-apple-darwin
  else
    HOST_TRIPLE := x86_64-apple-darwin
  endif
else
  ifeq ($(UNAME_M),aarch64)
    HOST_TRIPLE := aarch64-unknown-linux-musl
  else
    HOST_TRIPLE := x86_64-unknown-linux-musl
  endif
endif

# Use `cargo build` when target == host, `cargo zigbuild` otherwise.
cargo_for = $(if $(filter $(1),$(HOST_TRIPLE)),cargo build,cargo zigbuild)

.PHONY: help setup cross stage sync-versions check release clean-npm \
        build-darwin-arm64 build-darwin-x64 build-linux-x64 build-linux-arm64 \
        lint-web lint-web-fix \
        pypi-wheels pypi-clean \
        pypi-stage-darwin-arm64 pypi-stage-darwin-x64 \
        pypi-stage-linux-x64 pypi-stage-linux-arm64 \
        pypi-wheel-darwin-arm64 pypi-wheel-darwin-x64 \
        pypi-wheel-linux-x64 pypi-wheel-linux-arm64

# Lint the standalone web sources (src/web/{app.js,style.css}) with the
# pinned eslint+stylelint+prettier versions in package.json. CI-friendly
# and one-command. `make lint-web-fix` auto-applies safe fixes.
lint-web:
	@test -d node_modules || npm install --silent
	./node_modules/.bin/eslint src/web/app.js
	./node_modules/.bin/stylelint src/web/style.css
	./node_modules/.bin/prettier --check src/web/app.js src/web/style.css

lint-web-fix:
	@test -d node_modules || npm install --silent
	./node_modules/.bin/prettier --write src/web/app.js src/web/style.css
	./node_modules/.bin/eslint --fix src/web/app.js
	./node_modules/.bin/stylelint --fix src/web/style.css

# JS tests for the pure helpers in src/web/util.js. No build step, no
# test framework, no deps — just Node's stdlib parsing the same test.js
# that test.html runs in a browser. Exits non-zero if any test fails.
test-web:
	@node src/web/run-tests.js

# Open test.html in the default browser for the full interactive run.
# Same suite as `make test-web` but rendered as a page.
test-web-open:
	@open src/web/test.html 2>/dev/null || xdg-open src/web/test.html 2>/dev/null || \
	  echo "open src/web/test.html in your browser"

# ── PyPI wheels ─────────────────────────────────────────────────────
#
# Mirrors the npm packaging pattern: `make cross` produces one binary
# per target; `make pypi-stage-<target>` copies it into the wheel
# skeleton; `make pypi-wheel-<target>` builds a platform-tagged wheel.
# `make pypi-wheels` does all four in sequence. `make pypi-clean` wipes
# staged binaries + dist.
#
# Wheels land in `pypi/ccaudit/dist/`. Upload with `uv publish` or
# `twine upload` once verified.

PYPI_DIR := pypi/ccaudit
PYPI_BIN := $(PYPI_DIR)/src/ccaudit/bin/ccaudit

pypi-stage-darwin-arm64:
	@mkdir -p $(PYPI_DIR)/src/ccaudit/bin
	cp -f target/$(DARWIN_ARM64)/release/ccaudit $(PYPI_BIN)
	chmod +x $(PYPI_BIN)

pypi-stage-darwin-x64:
	@mkdir -p $(PYPI_DIR)/src/ccaudit/bin
	cp -f target/$(DARWIN_X64)/release/ccaudit $(PYPI_BIN)
	chmod +x $(PYPI_BIN)

pypi-stage-linux-x64:
	@mkdir -p $(PYPI_DIR)/src/ccaudit/bin
	cp -f target/$(LINUX_X64)/release/ccaudit $(PYPI_BIN)
	chmod +x $(PYPI_BIN)

pypi-stage-linux-arm64:
	@mkdir -p $(PYPI_DIR)/src/ccaudit/bin
	cp -f target/$(LINUX_ARM64)/release/ccaudit $(PYPI_BIN)
	chmod +x $(PYPI_BIN)

# Platform → PEP 425 wheel tag. musl-linked Linux binaries are portable
# across glibc versions, so `manylinux_2_17_*` is accurate here.
# `py3-none-<plat>` tags the wheel as pure-Python-except-for-the-binary.
pypi-wheel-darwin-arm64: pypi-stage-darwin-arm64
	cd $(PYPI_DIR) && python setup.py bdist_wheel --plat-name macosx_11_0_arm64 --python-tag py3

pypi-wheel-darwin-x64: pypi-stage-darwin-x64
	cd $(PYPI_DIR) && python setup.py bdist_wheel --plat-name macosx_10_12_x86_64 --python-tag py3

pypi-wheel-linux-x64: pypi-stage-linux-x64
	cd $(PYPI_DIR) && python setup.py bdist_wheel --plat-name manylinux_2_17_x86_64 --python-tag py3

pypi-wheel-linux-arm64: pypi-stage-linux-arm64
	cd $(PYPI_DIR) && python setup.py bdist_wheel --plat-name manylinux_2_17_aarch64 --python-tag py3

# All four wheels in one command. Requires `make cross` to have been
# run first so the Rust binaries exist in target/<triple>/release/.
pypi-wheels: pypi-wheel-darwin-arm64 pypi-wheel-darwin-x64 pypi-wheel-linux-x64 pypi-wheel-linux-arm64
	@echo ""
	@echo "wheels built:"
	@ls -la $(PYPI_DIR)/dist/*.whl 2>/dev/null || echo "  (none found — check target/ for binaries)"

pypi-clean:
	rm -rf $(PYPI_DIR)/dist $(PYPI_DIR)/build $(PYPI_DIR)/*.egg-info
	rm -f $(PYPI_BIN)

help:
	@echo "ccaudit release targets (version $(PKG_VERSION))"
	@echo ""
	@echo "  make setup          one-time toolchain install"
	@echo "  make cross          build all 4 targets"
	@echo "  make stage          copy binaries into npm/<platform>/"
	@echo "  make sync-versions  write $(PKG_VERSION) into npm/*/package.json"
	@echo "  make release        cross + sync-versions + stage + check"
	@echo "  make check          verify staged binaries"
	@echo "  make clean-npm      remove staged binaries"
	@echo ""
	@echo "To ship the lean build instead:"
	@echo "  make release CCAUDIT_DIST_FEATURES='--no-default-features'"

setup:
	@command -v zig >/dev/null 2>&1 || { echo "error: zig not found. install with: brew install zig"; exit 1; }
	@command -v cargo-zigbuild >/dev/null 2>&1 || cargo install --locked cargo-zigbuild
	@for t in $(TARGETS); do rustup target add $$t >/dev/null; done
	@echo "toolchain ready for: $(TARGETS)"

# Each target is its own Make rule so `make -j` actually parallelizes.
cross: build-darwin-arm64 build-darwin-x64 build-linux-x64 build-linux-arm64

build-darwin-arm64:
	$(call cargo_for,$(DARWIN_ARM64)) --target $(DARWIN_ARM64) $(CARGO_FLAGS)
build-darwin-x64:
	$(call cargo_for,$(DARWIN_X64)) --target $(DARWIN_X64) $(CARGO_FLAGS)
build-linux-x64:
	$(call cargo_for,$(LINUX_X64)) --target $(LINUX_X64) $(CARGO_FLAGS)
build-linux-arm64:
	$(call cargo_for,$(LINUX_ARM64)) --target $(LINUX_ARM64) $(CARGO_FLAGS)

stage:
	@mkdir -p npm/darwin-arm64 npm/darwin-x64 npm/linux-x64 npm/linux-arm64
	cp -f target/$(DARWIN_ARM64)/release/ccaudit   npm/darwin-arm64/ccaudit
	cp -f target/$(DARWIN_X64)/release/ccaudit     npm/darwin-x64/ccaudit
	cp -f target/$(LINUX_X64)/release/ccaudit      npm/linux-x64/ccaudit
	cp -f target/$(LINUX_ARM64)/release/ccaudit    npm/linux-arm64/ccaudit
	chmod +x npm/*/ccaudit

sync-versions:
	@echo "syncing version $(PKG_VERSION) → npm + pypi packages"
	@# Platform packages + main package version (npm)
	@for pj in npm/ccaudit/package.json \
	           npm/darwin-arm64/package.json \
	           npm/darwin-x64/package.json \
	           npm/linux-x64/package.json \
	           npm/linux-arm64/package.json; do \
		sed -i.bak -E 's/"version": *"[^"]*"/"version": "$(PKG_VERSION)"/' $$pj && rm -f $$pj.bak; \
	done
	@# Main package's optionalDependencies pins — keep in lockstep so the
	@# right platform binary is always fetched.
	@sed -i.bak -E 's|("@ccaudit/[a-z0-9-]+" *: *)"[^"]*"|\1"$(PKG_VERSION)"|g' npm/ccaudit/package.json
	@rm -f npm/ccaudit/package.json.bak
	@# PyPI main package (wheel builds all inherit this).
	@sed -i.bak -E 's/^version = "[^"]*"/version = "$(PKG_VERSION)"/' pypi/ccaudit/pyproject.toml
	@rm -f pypi/ccaudit/pyproject.toml.bak

check:
	@missing=0; \
	for f in npm/darwin-arm64/ccaudit npm/darwin-x64/ccaudit npm/linux-x64/ccaudit npm/linux-arm64/ccaudit; do \
		if [ ! -f "$$f" ]; then echo "MISSING $$f"; missing=1; continue; fi; \
		sz=$$(stat -f '%z' "$$f" 2>/dev/null || stat -c '%s' "$$f"); \
		printf "  %-32s %8d bytes\n" "$$f" "$$sz"; \
	done; \
	test $$missing -eq 0
	@echo ""
	@echo "running native binary --help:"
	@./npm/darwin-arm64/ccaudit --help 2>&1 | head -5 || true

release: cross sync-versions stage check
	@echo ""
	@echo "release $(PKG_VERSION) staged. publish order:"
	@echo "  cd npm/darwin-arm64 && npm publish --access public"
	@echo "  cd npm/darwin-x64   && npm publish --access public"
	@echo "  cd npm/linux-x64    && npm publish --access public"
	@echo "  cd npm/linux-arm64  && npm publish --access public"
	@echo "  cd npm/ccaudit        && npm publish --access public"

clean-npm:
	rm -f npm/*/ccaudit
