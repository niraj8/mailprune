BIN := mailprune
PREFIX := $(HOME)/bin

.PHONY: install build test clean publish-tap

install: build
	mkdir -p $(PREFIX)
	cp target/release/$(BIN) $(PREFIX)/$(BIN)
	@echo "installed $(PREFIX)/$(BIN)"

build:
	cargo build --release

test:
	cargo test

clean:
	cargo clean

# bump the Homebrew formula for the current Cargo.toml version;
# the release and its tarballs must already exist
publish-tap:
	scripts/publish-tap.sh
