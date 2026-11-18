# Storefront on stormcos -- a multi-pod, multi-container example.
CLI ?= sc
NS  ?= storefront

.DEFAULT_GOAL := help

.PHONY: help
help: ## Show this help
	@grep -hE '^[a-z-]+:.*?## ' $(MAKEFILE_LIST) \
	  | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

.PHONY: validate
validate: ## Offline lint: YAML, embedded configs, restricted hygiene, images
	@./scripts/validate.sh

.PHONY: secrets
secrets: ## Generate manifests/03-secrets.local.yaml with random values
	@./scripts/make-secrets.sh

.PHONY: deploy
deploy: ## Apply in dependency order and wait for each tier
	@CLI=$(CLI) NS=$(NS) ./scripts/deploy.sh

.PHONY: apply
apply: ## Apply everything at once (sc apply -k manifests/)
	@$(CLI) apply -k manifests/

.PHONY: test
test: ## Smoke-test through the LoadBalancer address and the HTTPRoute host
	@CLI=$(CLI) NS=$(NS) ./scripts/smoke-test.sh

.PHONY: status
status: ## Pods, services, routes, HPA, PDB at a glance
	@$(CLI) -n $(NS) get pods -o wide
	@echo
	@$(CLI) -n $(NS) get svc,httproute,hpa,pdb,networkpolicy

.PHONY: logs-edge logs-web logs-api
logs-edge: ## Every container in the edge pods
	@$(CLI) -n $(NS) logs -l app.kubernetes.io/name=edge --all-containers --tail=50
logs-web: ## Every container in the web pods
	@$(CLI) -n $(NS) logs -l app.kubernetes.io/name=web --all-containers --tail=50
logs-api: ## Every container in the api pods
	@$(CLI) -n $(NS) logs -l app.kubernetes.io/name=api --all-containers --tail=50

.PHONY: forward
forward: ## Port-forward the edge to http://127.0.0.1:8080
	@$(CLI) -n $(NS) port-forward svc/edge-lb 8080:80

.PHONY: undeploy
undeploy: ## Delete everything except the project and its PVCs
	@$(CLI) delete -k manifests --ignore-not-found
	@echo "PVCs kept. Remove them with: $(CLI) -n $(NS) delete pvc --all"

.PHONY: destroy
destroy: ## Delete the project and everything in it
	@$(CLI) delete project $(NS) --ignore-not-found
