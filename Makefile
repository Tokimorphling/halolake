.PHONY: demo build-web build-control-web dist dist-control-lite size-report

DIST_TARGET_DIR ?= target/dist-workspace
LITE_TARGET_DIR ?= target/dist-control-lite

demo:
	./scripts/first-demo.sh

build-web:
	cd web/new-api && bun install
	cd web/new-api/default && VITE_REACT_APP_VERSION=halolake-demo bun run build
	cd web/new-api/classic && VITE_REACT_APP_VERSION=halolake-demo bun run build

build-control-web: build-web
	HALOLAKE_WEB_BUILD_ID=$$(date +%s) cargo build -p halolake-control-api

# Production binaries: fat LTO, one codegen unit, abort-on-panic and stripped symbols.
dist:
	CARGO_TARGET_DIR=$(DIST_TARGET_DIR) cargo build --profile dist --workspace

# Smaller control-api without bundled Web assets, Passkey, admin extras or compat stubs.
dist-control-lite:
	CARGO_TARGET_DIR=$(LITE_TARGET_DIR) cargo build --profile dist -p halolake-control-api \
		--no-default-features --features core,allocator-mimalloc

size-report:
	@find "$(DIST_TARGET_DIR)/dist" "$(LITE_TARGET_DIR)/dist" \
		-maxdepth 1 -type f -perm -111 \
		-exec sh -c 'for file do bytes=$$(wc -c < "$$file" | tr -d " "); printf "%12s  %s\n" "$$bytes" "$$file"; done' sh {} + \
		2>/dev/null | sort -n
