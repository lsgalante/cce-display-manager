.PHONY: build install run clean

build:
	cargo build --release

install: build
	mkdir -p ~/.local/bin
	install -m 755 target/release/clear-display-manager ~/.local/bin/clear-display-manager

run:
	cargo run

clean:
	cargo clean
