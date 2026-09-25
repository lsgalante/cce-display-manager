# CLAUDE.md

> This is the `cce-display-manager` crate, inside the larger **`cce` Cargo
> workspace** — read `../cce-compositor/WORKSPACE.md` first for the multi-repo
> layout, the standalone-build rule, `ccebuild`, and the `cce-ui` toolkit. This
> file covers only what is specific to this crate. `README.md` is the
> user-facing account (keys, install, logs, the keyring chain); read it too.

`cce-display-manager` **is the login path.** A mistake here is not a broken
window: it is a machine the user cannot log into, and the usual tools for
finding out why (their session) are the thing that is missing. Every change
here gets verified by a REAL login before it is called done, and every
install leaves the rollback in place (see "Installing and verifying").

The whole crate is `src/main.rs`. One binary, four roles, each its own process,
chosen by argv in `main`:

- **daemon** (no args, root, the unit's `ExecStart`) — owns the tty, runs the
  resume watchdog thread, and loops: greeter → session → greeter.
- **greeter** (`--greeter`, root, under `cage -s`) — the `cce-ui` login screen.
  Checks the password (PAM `cce-display-manager-password`, authenticate +
  acct_mgmt only — see "No PAM session in the greeter") and prints one line on
  stdout for the daemon: `AUTH_SUCCESS|user|exec|is_wayland|password`
  (`auth_success_line` / `parse_auth_success`; the password is the LAST field,
  taken verbatim to end of line, because it may contain `|`).
- **fingerprint helper** (`--fprint-auth <user>`) — one attempt on the
  `cce-display-manager-fprint` stack, in a separate PROCESS because
  `pam_fprintd` blocks uninterruptibly and only killing the process makes
  fprintd release the sensor. `PR_SET_PDEATHSIG` ties it to the greeter.
- **session worker** (`--session-worker <tty> <user> <is_wayland> <exec>`,
  root) — opens the PAM/logind session, runs the session as the user, closes
  PAM when it exits. Password on STDIN (never argv — world-readable in
  /proc); it reports `SESSION_ID <id>` on stdout and then points stdout at
  /dev/null.

## The rules that keep logins working

**The session worker is a fresh process, never a `fork()` of the daemon**
(since 2026-09-25). The daemon is multi-threaded (the resume watchdog), and a
forked child inherits whatever locks another thread held at that instant; the
worker then does PAM, env writes, logging and spawns. That class of wedge froze
the GREETER's login on "Authenticating…" (2026-09-18: `pam_gnome_keyring`'s
auto_start forking out of the threaded Vulkan greeter). The daemon runs the
worker with `Command` and NO `pre_exec`, so only async-signal-safe work
happens between its fork and exec. Do not reintroduce `libc::fork` here, and do
not add a `pre_exec` to the daemon's spawns.

**No PAM session in the greeter.** The greeter authenticates only; the session
worker opens the real session. `open_session` in the greeter registered a
throwaway logind session under cage and ran session modules inside a threaded
Vulkan process — the freeze above.

**Hand the worker the password that was VERIFIED, verbatim.**
`State::auth_password` is set by each attempt (empty for a fingerprint) and is
what `AUTH_SUCCESS` carries. Two bugs lived here: the password was `trim()`ed
(a password with a leading/trailing space could never log in), and a
fingerprint success sent the password BOX's text, so a half-typed password
sent the worker down the password stack to fail a login the greeter had
accepted. An empty password means "already verified" to the worker
(`session_pam_service` → the autologin stack, `pam_permit`) — so the greeter's
word is the whole of a fingerprint login's authentication.

**End the logind session when the session is over** (`terminate_session`,
from the DAEMON after the worker exits — pam_systemd put the worker inside the
scope). With logind's `KillUserProcesses=no`, closing PAM only marks a session
`closing`; its leftover processes live on in the scope. Every login leaked that
way until 2026-09-25 (eight sessions stuck `closing`, 16 orphaned 1Password
helpers, 1.9 GB). Use **`kill-session`, not `terminate-session`**: once the
leader has exited, logind has abandoned the scope and TerminateSession does
NOTHING — no error, no journal line (measured). SIGTERM, poll up to 2 s for the
session to go, then SIGKILL; polled rather than timed on a thread, because the
next worker is spawned right after. This also runs on a compositor-restart
relaunch: nothing survives into the new session from the old scope today (the
new compositor starts its clients fresh); if clients ever reconnect ACROSS a
restart, they must be carried into the new session instead.

**One keyring provider: the TPM-sealed `gnome-keyring-daemon` unit** (README,
"Login keyring"). `pam_gnome_keyring` must stay OUT of the PAM stacks —
`no_pam_stack_starts_a_keyring` enforces it. In a stack it started a second
daemon at every login that could not unlock the keyring (its password is the
sealed random one) and raced the unit.

## Where things go

- **Session output** → `/run/user/<uid>/cce-session.log` (previous one `.old`),
  opened in the session command's `pre_exec` AS THE USER — a root open in a
  user-owned directory could be symlinked at any file. The Bash console
  session (`CONSOLE_SESSION_EXEC`) keeps the tty instead. It used to inherit the
  daemon's stdout: a world-readable root log, truncated at every daemon start.
- **Daemon / greeter / cage / worker logs** → the journal. The units set
  `StandardOutput/StandardError=journal`; the daemon skips its old
  `/var/log/cce-display-manager-<tty>.log` only when `JOURNAL_STREAM` says
  systemd connected it, so a new binary under an old unit (or an F5 re-exec of
  a daemon an old unit started) still logs to the file. `JOURNAL_STREAM` is
  removed from the session's environment.
- **Persistent state** → `/var/lib/cce-display-manager/last_user`,
  `last_session`. Runtime → `/run/cce-display-manager-<tty>/` (the greeter's
  `XDG_RUNTIME_DIR`, 0700).
- **Compositor restart** → `ccectl restart-compositor` writes
  `/tmp/cce-restart-requested-<user>` and exits; the daemon relaunches the
  same session on the autologin stack. The flag is honored only as a regular
  file owned by that user (`symlink_metadata`), since anyone can create names
  in /tmp.

**The unit restarts the daemon after any exit** (`Restart=always`, since
2026-09-25). Ctrl+C at the greeter exits it (130 from the greeter → the daemon
`exit(0)`), which left no login screen until a reboot. A daemon that dies
mid-session has already ended the session — it leads tty1's session, and the
compositor and `startcce`, in its foreground process group, take the kernel's
SIGHUP with the default action (checked in `/proc/<pid>/status`) — so a
restart then cannot put a greeter beside a live session. If the compositor
ever starts ignoring SIGHUP, that reasoning has to be redone.

## Installing and verifying

The login runs `/usr/bin/cce-display-manager`, the units in
`/etc/systemd/system` and the stacks in `/etc/pam.d` — all installed by
**`ccebuild install-system`** (root, backs up to `*.bak-<date>`), never by
`ccebuild install`, which only puts the keyring helper scripts (needed: the TPM
drop-in runs one) and an unused copy of the binary in `~/.local/bin`.
`make install` runs both. `ccebuild install-system --dry-run` shows what would
change, and is how to check a build actually differs from what is installed.

**A new binary does not reach the running DAEMON.** Each login starts the
greeter from `/usr/bin`, but the daemon keeps the code it started with (its
`/proc/<pid>/exe` reads `(deleted)`): F5 at the greeter re-execs it from
`/usr/bin`, a reboot restarts it. So daemon-side changes (the session worker,
session termination) need a logout + F5 + login to be tested at all. Do NOT
restart `cce-display-manager@tty1` from inside a session — it owns the tty the
session is on.

What the suite covers (`cargo test -p cce-display-manager`): the line
protocols, arg building/parsing, PAM service choice, session-id validation,
that no stack starts a keyring, and (`tests/session_worker.rs`, the real
binary) that `--session-worker` refuses a non-root caller and a malformed argv.
It cannot cover PAM, logind or the handoff — those need root and a seat. Verify
on a real login, reading `journalctl -u cce-display-manager@tty1 -b` (or the
/var/log file, see above), `loginctl list-sessions`, and the session log.
Rollback is Ctrl+Alt+F2 (logind's auto-VTs), restore the `.bak`, reboot.

## Known gaps

- `WLR_DRM_DEVICES=/dev/dri/card1:/dev/dri/card0` for the greeter's cage is
  this machine's card numbering, hard-coded; `startcce` pins card1 the same way.
- `busctl monitor` (the resume watchdog) and `chvt` / `loginctl` are shelled
  out to, deliberately — a D-Bus crate would widen a root binary's
  dependencies for three calls.
