# anacraft — developer shortcuts
#
# The site in docs/ must be served over HTTP, not opened as a file:// URL:
# the self-hosted webfont is blocked by CORS on file://, and without it the
# dashboard falls back to a font with different metrics and renders misaligned.

PORT ?= 8000

.PHONY: help serve open dash check fmt lint test capture partials palettes

help:  ## Show this help
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'

serve: ## Serve docs/ at http://localhost:$(PORT) (Ctrl-C to stop)
	@echo "⛏  anacraft.dev on http://localhost:$(PORT)  —  Ctrl-C to stop"
	@cd docs && python3 -m http.server $(PORT)

open: ## Serve and open a browser at the site
	@( sleep 1; open http://localhost:$(PORT) ) &
	@$(MAKE) serve

dash: ## Run the dashboard on synthetic data
	cargo run --release -- dash --demo

check: fmt lint test ## Everything CI runs

fmt: ## Check formatting
	cargo fmt --check

lint: ## Clippy, warnings denied
	cargo clippy --all-targets -- -D warnings

test: ## Run the test suite
	cargo test

partials: ## Splice the shared nav and footer (scripts/) into every page
	@python3 scripts/splice-partials.py

palettes: ## Regenerate the site's CSS palettes from src/theme.rs
	@python3 scripts/gen-palettes.py

capture: ## Regenerate the site's dashboard captures from the real TUI
	@cargo run --quiet --release -- capture | python3 scripts/splice-capture.py
