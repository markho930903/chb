.PHONY: check check-env setup-dev dev

check:
	cargo fmt --check
	cargo clippy --all-targets --locked -- -D warnings
	cargo test --locked

UV_BIN ?= $(shell command -v uv 2>/dev/null)
HEADROOM_BIN ?= $(shell if [ -n "$(UV_BIN)" ]; then "$(UV_BIN)" tool dir --bin 2>/dev/null; else printf '%s/.local/bin' "$$HOME"; fi)/headroom

check-env:
	@test -x "$(HEADROOM_BIN)" || { \
		echo "Headroom is not installed at $(HEADROOM_BIN)" >&2; \
		if [ -z "$(UV_BIN)" ]; then echo "uv is not installed; run: make setup-dev" >&2; else echo "Run: make setup-dev" >&2; fi; \
		echo 'Or: make dev HEADROOM_BIN=/path/to/headroom' >&2; \
		exit 1; \
	}
	@echo "Headroom: $(HEADROOM_BIN)"

setup-dev:
	@if [ -z "$(UV_BIN)" ]; then \
		echo "Installing uv..."; \
		curl -LsSf https://astral.sh/uv/install.sh | sh; \
		UV_BIN="$$HOME/.local/bin/uv"; \
	else \
		UV_BIN="$(UV_BIN)"; \
	fi; \
	test -x "$$UV_BIN" || { echo "uv installation did not produce an executable" >&2; exit 1; }; \
	"$$UV_BIN" tool install --python 3.13 "headroom-ai[all]"

dev: check-env
	cargo run -- serve --development --headroom-bin "$(HEADROOM_BIN)"
