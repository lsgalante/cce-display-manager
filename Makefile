.PHONY: build install install-user install-system run clean

build:
	cargo build --release

# The display manager lives in TWO places, and `install` does both:
#
#   install-user    ~/.local/bin + ~/.local/share/dbus-1: the keyring helper
#                   scripts (the TPM drop-in runs cce-gnome-keyring-start from
#                   there) and the Secret Service D-Bus file. It also drops a
#                   copy of the binary in ~/.local/bin, which nothing runs.
#   install-system  /usr/bin/cce-display-manager, the systemd units and the
#                   PAM stacks — what the login actually runs. Root, one sudo
#                   prompt; it backs up whatever it replaces (*.bak-<date>)
#                   and covers every crate's root artifacts, not only ours.
#
# Until 2026-09-25 `install` was install-user alone, so it never touched the
# greeter the unit starts. Binaries, helper scripts and units are enumerated
# by ccebuild from cargo metadata and the crate's dirs — hand-listing them is
# what left cce-bevel and the keyring helpers uninstalled for weeks.
install: build install-user install-system

install-user:
	@command -v ccebuild >/dev/null || { echo "ccebuild not installed — run: make -C ../cce-compositor install"; exit 1; }
	ccebuild install --no-build cce-display-manager

install-system:
	@command -v ccebuild >/dev/null || { echo "ccebuild not installed — run: make -C ../cce-compositor install"; exit 1; }
	ccebuild install-system

# The greeter alone, in the current Wayland session — the daemon (no args)
# must run as root and refuses otherwise.
run:
	cargo run -- --greeter

clean:
	cargo clean
