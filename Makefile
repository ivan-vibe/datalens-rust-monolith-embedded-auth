# DataLens Rust monolith — common tasks.
.PHONY: build check test test-auth smoke-dataset e2e-ui up down reset ps logs parity

build:            ## release build of the single binary (auth is inside)
	cargo build --release

check:
	cargo check --offline || cargo check

test:             ## unit tests (monolith + auth)
	cargo test --offline || cargo test
	cd auth && cargo test --offline || cargo test

test-auth:        ## auth crate tests only
	cd auth && cargo test --offline || cargo test

smoke-dataset:    ## replay the editor's second-drop/join/refresh flow against a live monolith
	python3 scripts/dataset_editor_smoke.py

e2e-ui:           ## browser-level dataset editor test (needs Chrome on :9224 and a live UI)
	node scripts/ui_dataset_editor_e2e.mjs

up:               ## build images and start the cluster (Docker)
	./deploy/cluster.sh up

down:
	./deploy/cluster.sh down

reset:            ## wipe the postgres volume; the seed is restored on next `make up`
	./deploy/cluster.sh reset

ps:
	./deploy/cluster.sh ps

logs:
	./deploy/cluster.sh logs -f

HAR ?= capture.har

parity:           ## chart payload parity against a reference Python DataLens cluster
	python3 scripts/parity.py --har $(HAR)
