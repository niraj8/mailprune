BIN := mailprune
PREFIX := $(HOME)/bin

.PHONY: install build test clean release publish-tap

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

# cut a release: make release VERSION=0.2.2
release:
	@test -n "$(VERSION)" || { echo "usage: make release VERSION=X.Y.Z"; exit 1; }
	scripts/release.sh $(VERSION)

# bump the Homebrew formula for the current Cargo.toml version;
# the release and its tarballs must already exist
publish-tap:
	scripts/publish-tap.sh
