BUILD_MODE="debug"
CURRENTDATE=`date +"%Y-%m-%d"`

ci:
	cargo test --features "persist_sled" --
	cargo fmt --all -- --check
	cargo clippy --features persist_sled -- -D warnings

startall:
	cargo build
	mkdir -p circuits/testdata/persist logs
	`pwd`/target/$(BUILD_MODE)/rollup_state_manager 1>logs/rollup_state_manager.$(CURRENTDATE).log 2>&1 &

taillogs:
	tail -n 15 logs/*
