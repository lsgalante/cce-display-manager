# CCE Display Manager

`cce-display-manager` is a premium GUI display manager greeter built using the `cce-ui` framework, leveraging Wayland via `smithay-client-toolkit` and GPU-accelerated graphics via `wgpu`. 

It integrates seamlessly with the rest of the **Clear OS** desktop ecosystem, offering a highly customized login greeter interface that transitions directly into the `clear-computing-environment-client` (River WM) or standard fallback sessions.

## Features

- **Premium Design Aesthetics**: Fully hardware-accelerated dark theme matching the design system of the Clear desktop environment.
- **Session Selector**: Interactive session cyclist allowing selection between the Wayland-based River window manager and a fallback Bash login shell.
- **Obfuscated Password Fields**: Dedicated custom password widget wrapper around `cce-ui` text inputs.
- **Focus Cycle Navigation**: Easily navigate fields using standard `Tab` focus switching keys.
- **Seamless Launch Integration**: Authenticates credentials and starts the session via `/home/lsgalante/Dropbox/Clear/clear-computing-environment-client/start-river.sh`.

## Architecture

- **Wayland Protocol Handling**: Managed using `smithay-client-toolkit` and `calloop` event dispatcher loop.
- **Rendering Engine**: `wgpu` with WGSL custom shaders and `glyphon` text atlas system.
- **Core GUI**: Designed as a layout grid inside a centered card frame containing:
  - Custom `LoginCard` container box
  - `TextBox` input fields
  - Cyclic `Button` session selector
  - `StatusLabel` validation indicator

## Running & Compiling

Build the project locally:

```bash
cargo build --release
```

Run in an existing Wayland environment (for testing/development):

```bash
cargo run
```

## Login keyring

Login here is by fingerprint, so PAM never sees a password and
`pam_gnome_keyring` cannot unlock anything. The keyring password is instead
sealed to the machine's TPM and fed to the daemon at startup.

- `scripts/cce-gnome-keyring-enroll` — one-time: seals a random password with
  **tpm2-tools** into `~/.config/cce/keyring-seal.{pub,priv}` and creates the
  gnome-keyring `login` keyring with it. Needs the `tss` group and
  `tpm2-tools`.
- `scripts/cce-gnome-keyring-start` — unseals and pipes the password into
  `gnome-keyring-daemon --foreground --unlock`, as a single process.
- `systemd/gnome-keyring-daemon.service.d/tpm-unlock.conf` — points the stock
  unit at that script and sets `Restart=no`.
- `dbus/org.freedesktop.secrets.service` — routes bus activation to the same
  unit, so there is only ever one provider.

**`ccebuild` does not install drop-ins** — `unit_files()` matches only
`.service/.target/.timer/.socket/.path`. Install this one by hand:

```bash
install -Dm644 systemd/gnome-keyring-daemon.service.d/tpm-unlock.conf \
  ~/.config/systemd/user/gnome-keyring-daemon.service.d/tpm-unlock.conf
```

Three traps, each of which broke a previous attempt:

- **`systemd-creds` is not usable here.** Run by a non-root user it does not
  touch the TPM; it delegates to a polkit-gated root service. It succeeds in an
  interactive session and fails at login with
  `io.systemd.InteractiveAuthenticationRequired`. tpm2-tools talks to
  `/dev/tpmrm0` directly via the `tss` group, so it needs no agent.
- **Never validate this from an interactive shell** — it has a polkit agent and
  a TTY that the login path does not. Use
  `systemd-run --user --pipe --wait --setenv=PATH=...`, which reproduces the
  login environment and the failure above.
- **`Restart=no` is load-bearing.** A drop-in replacing `ExecStart` inherits the
  stock `Restart=on-failure`; with a credential that could not decrypt at login
  that produced 99 restarts in ~90s and hung the greeter. Losing secrets is
  recoverable, an unusable login is not.

Clients need `--password-store=gnome-libsecret`: Chromium picks its backend
from `XDG_CURRENT_DESKTOP`, does not recognise `cce`, and silently falls back
to plaintext even when the keyring is healthy.
