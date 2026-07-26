TYPST ?= $(shell command -v typst || echo /opt/homebrew/bin/typst)

help:
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9\/_\.-]+:.*?## / {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

all: ## Build the full site into ../_site (docs + dashboard)
	TYPST=$(TYPST) ../tools/build-site.sh

serve: all ## Build and serve locally on :8000
	python3 -m http.server -d ../_site 8000

fmt: ## Format typst sources
	typstyle -i *.typ shim/*.typ

clean: ## Remove build output
	rm -rf ../_site

.PHONY: help all serve fmt clean
