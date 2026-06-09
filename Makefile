BIN := mailprune
PREFIX := $(HOME)/bin

.PHONY: install build test clean

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
