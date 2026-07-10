.PHONY: demo build-web build-control-web

demo:
	./scripts/first-demo.sh

build-web:
	cd web/new-api && bun install
	cd web/new-api/default && VITE_REACT_APP_VERSION=halolake-demo bun run build
	cd web/new-api/classic && VITE_REACT_APP_VERSION=halolake-demo bun run build

build-control-web: build-web
	HALOLAKE_WEB_BUILD_ID=$$(date +%s) cargo build -p halolake-control-api
