# cce-display-manager

The login screen of the cce desktop: a root daemon on a VT that shows a
`cce-ui` greeter under [cage](https://github.com/cage-kiosk/cage), checks the
user's password or fingerprint against PAM, and starts their session —
normally `startcce`, the cce compositor.

It runs as `cce-display-manager@tty1.service` (enabled; `getty@tty1` is its
conflict). The machine's own session list comes from
`/usr/share/wayland-sessions` and `/usr/share/xsessions`, plus a built-in
**Bash Shell** entry for a console login on the tty.

## Using the greeter

- Type the username (the last user is filled in) and the password, then Enter.
- **Fingerprint**: with the username filled in and fprintd enabled for
  `cce-display-manager-fprint`, a scan starts on its own; Enter with an empty
  password starts one again. Submitting a password cancels a scan in
  progress.
- **Up / Down** or **Ctrl+P / Ctrl+N** pick the session; **Tab** moves between
  the two fields.
- **F5** restarts the display manager daemon from `/usr/bin` — how a newly
  installed version takes effect without a reboot.
- **Ctrl+C** exits the display manager; its unit (`Restart=always`) starts it
  again a second later — a full restart, where F5 only re-execs the daemon.

## How it is put together

One binary, four roles, each its own process:

| Role | Started as | Runs as | Job |
|---|---|---|---|
| daemon | `cce-display-manager` (the unit) | root | owns the tty; loops: greeter → session → greeter |
| greeter | `cage -s -- cce-display-manager --greeter` | root | the login screen; verifies the user, tells the daemon |
| fingerprint helper | `cce-display-manager --fprint-auth <user>` | root | one fingerprint attempt, killable (it holds the sensor) |
| session worker | `cce-display-manager --session-worker …` | root, then the user | opens the PAM/logind session, runs it as the user, closes it |

When the session ends the daemon ends its logind session too — a session's
leftover processes do not outlive it — and shows the greeter again. A
compositor restart (`ccectl restart-compositor`) relaunches the same session
straight away, without the greeter.

## Installing

```bash
make install
```

That is two installs, and both matter:

- `ccebuild install cce-display-manager` — user-level: the keyring helper
  scripts into `~/.local/bin` and the Secret Service D-Bus file.
- `ccebuild install-system` — root: `/usr/bin/cce-display-manager`, the
  systemd units and the PAM stacks in `/etc/pam.d`. This is what the login
  runs. It asks for sudo once and backs up everything it replaces as
  `*.bak-<date>`.

A new binary takes effect for the **greeter** at the next login, but the
**daemon** keeps running the code it started with — press F5 at the greeter,
or reboot.

**If a login breaks** (the password is accepted, then no session): Ctrl+Alt+F2
gives a text login; restore `/usr/bin/cce-display-manager.bak-<date>` over
`/usr/bin/cce-display-manager` (and any `/etc/pam.d/cce-display-manager*.bak-<date>`
you changed) and reboot.

## Logs

- The daemon, greeter, cage and session worker:
  `journalctl -u cce-display-manager@tty1 -b`. (Under an older unit, or after an
  F5 restart of a daemon started by one, they go to
  `/var/log/cce-display-manager-tty1.log` instead.)
- The session's own output: `/run/user/<uid>/cce-session.log`, with the
  previous session's as `cce-session.log.old`. The compositor also keeps its
  own logs in `/run/user/<uid>/cce/`.

## Login keyring

Login here is often by fingerprint, so PAM never sees a password and
`pam_gnome_keyring` cannot unlock anything. The keyring password is instead
sealed to the machine's TPM and fed to the daemon at startup — and
`pam_gnome_keyring` is deliberately **not** in the login stacks: it only
started a second daemon at every login that could not unlock the keyring and
raced this one.

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
- `scripts/cce-keyring-selftest` — one-command verdict on whether this login's
  keyring chain worked.

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

## Configuration

`/etc/cce/cce.json` — `{"scale": 2.0}` scales the greeter for a HiDPI panel
(cage reports a scale-1 output). The repo's `cce.json` is the one this machine
uses.
