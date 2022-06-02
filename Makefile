all: lint

CARGO := cargo

test:
	${CARGO} test --workspace -- --skip trust_metric --nocapture

doc:
	cargo doc --workspace --no-deps

doc-deps:
	cargo doc --workspace

# generate GraphQL API documentation
doc-api:
	bash docs/build/gql_api.sh

check:
	${CARGO} check --workspace

build:
	${CARGO} build --release

prod-muta-chain:
	${CARGO} build --release --example muta-chain

fmt:
	cargo +nightly fmt

lint:
	${CARGO} clippy --workspace

ci: clippy test

info:
	date
	pwd
	env

e2e-test:
	cargo build --example muta-chain
	rm -rf ./devtools/chain/data
	./target/debug/examples/muta-chain -c ./devtools/chain/config.toml -g ./devtools/chain/genesis.toml > /tmp/log 2>&1 &
	cd tests/e2e && yarn && ./wait-for-it.sh -t 300 localhost:8000 -- yarn run test
	pkill -2 muta-chain

byz-test:
	cargo build --example muta-chain
	cargo build --example byzantine_node
	rm -rf ./devtools/chain/data
	CONFIG=./examples/config-1.toml GENESIS=./examples/genesis.toml ./target/debug/examples/muta-chain > /tmp/log 2>&1 &
	CONFIG=./examples/config-2.toml GENESIS=./examples/genesis.toml ./target/debug/examples/muta-chain > /tmp/log 2>&1 &
	CONFIG=./examples/config-3.toml GENESIS=./examples/genesis.toml ./target/debug/examples/muta-chain > /tmp/log 2>&1 &
	CONFIG=./examples/config-4.toml GENESIS=./examples/genesis.toml ./target/debug/examples/byzantine_node > /tmp/log 2>&1 &
	cd byzantine/tests && yarn && ../../tests/e2e/wait-for-it.sh -t 300 localhost:8000 -- yarn run test
	pkill -2 muta-chain byzantine_node

security-audit:
	@cargo audit --version || cargo install cargo-audit
	@cargo audit
