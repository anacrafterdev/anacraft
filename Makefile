# anacraft — developer shortcuts
#
# The site in docs/ must be served over HTTP, not opened as a file:// URL:
# the self-hosted webfont is blocked by CORS on file://, and without it the
# dashboard falls back to a font with different metrics and renders misaligned.

PORT ?= 8000

.PHONY: help serve open dash check fmt lint test capture partials palettes \
	switch-to-prod build-prod generate-secrets-prod dns-prod deploy-prod rollout-prod deploy-and-rollout-prod

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

# ------------------------------------------------ app.anacraft.dev (GKE) ---
#
# Same cluster and shape as ~/source/api: an image pushed to GCR, a Helm
# release in the prod namespace, and a GCE Ingress with a managed cert. The
# image is the published release of `craft`, not a build of this tree — tag
# a release first, then deploy it (VERSION=v0.35.0 to pin another one).

PROD_PROJECT_ID = smartloop-gcp-us-east
PROD_CLUSTER_REGION = us-west1-b
REGISTRY = gcr.io
APP_IMAGE = anacraft-app
APP_HOST = app.anacraft.dev
APP_IP_NAME = anacraft-app-ip
# DEMO=true deploys the hosted server with synthetic data and no sign-in —
# what runs until the Web OAuth client exists.
DEMO ?= false
VERSION ?= v$(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

switch-to-prod: ## Point kubectl at the prod cluster
	gcloud container clusters get-credentials \
		$$(gcloud container clusters list --project=$(PROD_PROJECT_ID) --format="value(name)" | head -1) \
		--region=$(PROD_CLUSTER_REGION) --project=$(PROD_PROJECT_ID)

build-prod: ## Build the craft serve image from release $(VERSION) and push it
	./deploy/build.sh $(PROD_PROJECT_ID) $(APP_IMAGE) $(REGISTRY) $(VERSION)

generate-secrets-prod: switch-to-prod ## Sync the Web OAuth client from Secret Manager
	ENV=prod DEMO=$(DEMO) PROJECT_ID=$(PROD_PROJECT_ID) ./deploy/sync-secrets.sh

dns-prod: ## Reserve the static IP and point $(APP_HOST) at it in Cloudflare
	PROJECT_ID=$(PROD_PROJECT_ID) IP_NAME=$(APP_IP_NAME) HOST=$(APP_HOST) ./deploy/dns.sh

deploy-prod: generate-secrets-prod dns-prod ## helm upgrade --install into prod
	helm upgrade --install anacraft-app helm/app \
		-f helm/app/values.yaml \
		-f helm/app/values.secrets.prod.yaml \
		--set demo=$(DEMO) \
		--namespace prod

rollout-prod: switch-to-prod build-prod ## Push a fresh image and restart the pod
	kubectl rollout restart deployment/anacraft-app -n prod
	kubectl rollout status deployment/anacraft-app -n prod --timeout=180s

deploy-and-rollout-prod: deploy-prod rollout-prod ## Everything: DNS, secrets, chart, image, restart
