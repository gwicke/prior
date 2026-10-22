# Sandbox env-var and X11 audit

*Scope: the `bwrap` sandbox that prior builds in `src/tools.rs::build_bwrap_args`.
Audited 2026-10-21 against `bubblewrap 0.12.0`, live on the author's host
(Debian, i3/X11, Wayland absent). Findings below are backed by the experiments
in "Evidence" — every claim marked CONFIRMED was reproduced, not inferred.*

## Status

**R1 and R2 are implemented** (see "Implementation notes" at the end of the
Recommendations section). R3 (the X11 control) and R4 are not.

## Summary

Three findings, in descending severity:

1. **The env filter is bypassable whenever `/proc` is mounted.** The bwrap
   parent's `/proc/1/environ` still holds the *host* environment, so a
   sandboxed command can read every secret the filter dropped. CONFIRMED.
   A fresh `/proc` does **not** help — see "Does a fresh `/proc` help?" below.
   The fix is to scrub bwrap's *own* environment at spawn time (R1).
2. **The default env policy is a denylist and leaks the entire desktop/session
   environment.** On the author's host it dropped 4 of 66 variables and let 62
   through, including `DISPLAY`, `XAUTHORITY`, `SSH_AGENT_PID`,
   `DBUS_SESSION_BUS_ADDRESS`, `GPG_AGENT_INFO` and every WM/IPC socket path.
   CONFIRMED.
3. **X11 is reachable from inside the sandbox**, over the *abstract* Unix
   socket, whenever `network: true` is set — full window enumeration, title
   reads, clipboard read/write and `XTEST` synthetic input are all available.
   CONFIRMED. `DISPLAY`/`XAUTHORITY` survive the env filter, so the sandboxed
   command is *told* where the display is.

Two consequences for the plan of record:

- An allowlist alone is **not** enough. It has to be paired with R1, or the
  filter stays decorative.
- X11 should be blocked by **layered** controls — the network namespace for
  reachability, Landlock for per-path control, seccomp as the fallback — not
  by removing `DISPLAY`, which is only a hint (R3).

Two reference implementations are used throughout. Anthropic's
[sandbox-runtime](https://github.com/anthropics/sandbox-runtime) (`srt`) is
the closest comparable implementation; it shares Finding 1 — see
"Cross-check: sandbox-runtime". The Rust option `nono`, together with
Landlock itself, is the better model for the X11 control — see
"Cross-check: Landlock, and the Rust alternatives".

---

## How the sandbox is built today

`build_bwrap_args` (`src/tools.rs:1704`) assembles, in order:

- `--ro-bind` for each of `ESSENTIAL_RO_DIRS` = `/usr /bin /lib /lib64`
  (`src/tools.rs:1611`), plus `/etc` when no `/etc` binding is configured.
- User `ro`/`rw` bindings; a binding whose in-sandbox target is `/dev` or
  `/proc` is translated to bwrap's `--dev` / `--proc` (`src/tools.rs:1790-1801`).
- A `--tmpfs /tmp` scratch area (unless a user binding covers `/tmp`).
- `--unshare-net` unless `network: true`; `--unshare-pid` and `--cap-drop ALL`
  by default; `--die-with-parent`.
- `--clearenv` followed by one `--setenv NAME VALUE` per variable the env
  policy permits, then the sandbox's own forced values (`src/tools.rs:1850-1876`).

The env policy itself (`src/config.rs:1043-1200`) is a two-mode glob filter:

- `EnvMode::Disallow` — keep the whole environment minus matching names.
- `EnvMode::OnlyAllow` — keep only matching names.

The **default**, when `[tool_config.bash.sandbox.env]` is unset, is now
`OnlyAllow` with `DEFAULT_ENV_ALLOW` (`src/config.rs:1119`) — the change
recommended as R2 below, and implemented. `DEFAULT_ENV_DISALLOW` is retained
as an explicit opt-in for operators who want the old behaviour.

Separately, bwrap itself is spawned with a scrubbed environment of its own
(`PATH` and nothing else, `config::bwrap_scrub_env` at `src/config.rs:1219`).
See R1 below.

---

## Finding 1 — the env filter is bypassable via `/proc` (severity: high)

### Mechanism

`bwrap` is the parent of the sandboxed command. It inherits the host
environment and then calls `clearenv()` + `setenv()` for the child, but
**bwrap's own `/proc/<pid>/environ` still contains the unmodified host
environment**. When prior mounts a fresh procfs with `--proc /proc`, the
sandboxed command can read the bwrap parent's environ and recover every
variable the policy dropped.

`--unshare-pid` does not help: bwrap's reaper is PID 1 *inside* the new
namespace, and its `/proc/1/environ` is the host environment.

### Evidence

```
$ MY_SECRET=leaked_value_123 bwrap ... --proc /proc --unshare-pid \
      --unshare-net --cap-drop ALL --clearenv --setenv PATH "$PATH" \
      -- sh -c 'echo "child env: [${MY_SECRET:-<empty>}]";
                echo "via /proc/1/environ:";
                tr "\0" "\n" < /proc/1/environ | grep MY_SECRET'

child env: [<empty>]                          <-- filter "worked"
via /proc/1/environ:
MY_SECRET=leaked_value_123                   <-- filter defeated
```

The same holds for the real secret shapes:

```
$ ANTHROPIC_API_KEY=sk-ant-SECRET SSH_AUTH_SOCK=/run/user/1000/ssh-agent.sock bwrap ...
child env has API key? []
parent /proc/1/environ:
ANTHROPIC_API_KEY=sk-ant-SECRET
SSH_AUTH_SOCK=/run/user/1000/ssh-agent.sock
```

And against the *live* sandbox this audit ran inside (prior's own bwrap, PID 1
of my namespace), where the env filter had correctly removed the keys from the
child environment:

```
child sees DEEPSEEK_API_KEY?   [<empty>]
child sees SSH_AUTH_SOCK?      [<empty>]

but /proc/1/environ (bwrap parent) exposes:
DEEPSEEK_API_KEY=<REDACTED-VALUE-PRESENT>
OPENROUTER_API_KEY=<REDACTED-VALUE-PRESENT>
SSH_AUTH_SOCK=<REDACTED-VALUE-PRESENT>
XIAOMI_MIMO_API_KEY=<REDACTED-VALUE-PRESENT>
```

### Does a fresh `/proc` help?

**No.** This is worth stating explicitly, because mounting a fresh procfs looks
like it should close the hole and does not. A fresh `/proc` changes *which*
processes are visible; it does not change *whose* environment PID 1 has.

With `--unshare-pid` — which prior enables by default — PID 1 inside the
namespace **is** the bwrap parent:

```
$ MY_SECRET=leakme bwrap ... --proc /proc --unshare-pid --unshare-net \
      --clearenv --setenv PATH "$PATH" -- sh -c '
        echo "--- /proc/1/cmdline ---"; tr "\0" " " < /proc/1/cmdline | head -c 60; echo
        echo "--- /proc/1/environ ---";  tr "\0" "\n" < /proc/1/environ | grep MY_SECRET'

--- /proc/1/cmdline ---
bwrap --ro-bind /usr /usr --ro-bind /bin /bin --ro-bind /lib /lib
--- /proc/1/environ ---
MY_SECRET=leakme
```

So the two controls are orthogonal:

| | fresh `/proc` | `--unshare-pid` | env filter effective? |
|---|---|---|---|
| no `/proc` mount | — | irrelevant | **yes** (nothing to read) |
| fresh `/proc` | yes | yes | **no** (PID 1 = bwrap) |
| fresh `/proc` | yes | no | **no** (host processes visible) |
| host `/proc` bound | no | yes | **no** (worst: host + bwrap) |

What a fresh `/proc` *does* buy is real: it hides the host's process list and
the other host processes' environments. Without it, a sandbox can enumerate
the host's processes. That is a genuine isolation win, and it is why prior
should keep `--proc` — but it does not touch Finding 1.

### Scope

The bypass requires `--proc /proc` to be present. prior only emits that when
the operator's config binds `/proc` (translated at `src/tools.rs:1797`). So:

- **Default config (no bindings)** — no `/proc`, no bypass. The denylist is
  the only thing standing between the model and the host env, and it is leaky
  (Finding 2).
- **Config with a `/proc` binding** — the env filter is *entirely* inert. The
  `OnlyAllow` mode would be equally inert.

There is a second, related path: `/proc/1/cmdline` exposes bwrap's full argv,
which includes every `--setenv NAME VALUE` pair. That is a leak of the
*allowed* values (not the dropped ones), and it also fingerprints the host
(`--ro-bind` source paths, `--bind` of `~/.cargo`, etc.). It is the same
root cause — `/proc` is not sandboxed against the bwrap parent.

### Why it matters for the allowlist plan

An allowlist does not fix this. `OnlyAllow` with a tight pattern list would
still leave `/proc/1/environ` exposing the host environment wholesale, because
bwrap never clears *its own* environ — it only clears the child's.

The good news is that the fix does **not** require removing procfs. If bwrap
is spawned with a scrubbed environment in the first place, the secrets never
reach it and there is nothing for `/proc/1/environ` to leak (see R1, which
shows the confirming experiment). That keeps procfs available for the Rust
toolchain, which needs `/proc/self/exe`.

Note also that the "default" claim in `doc/sandbox.md:310-311` — "the sandbox
already blocks network egress by default, so a leaked key cannot be
exfiltrated unless `network: true`" — holds only for TCP egress. A leaked key
can still be written to a file, a git commit, or exfiltrated over the
abstract Unix sockets that survive `--unshare-net` (see Finding 3).

---

## Finding 2 — the denylist leaks the whole desktop environment

`DEFAULT_ENV_DISALLOW` matches only `*_KEY`, `*_SECRET`, `*_TOKEN`, `AWS_*`
and `SSH_AUTH_SOCK`. On the author's host, prior's process environment held
**66 variables; the policy dropped 4 and let 62 through** — and the 62 were
entirely the *session* variables, which no pattern targets:

```
$ comm -23 <(parent env names) <(names passed via --setenv)
DEEPSEEK_API_KEY
OPENROUTER_API_KEY
SSH_AUTH_SOCK
XIAOMI_MIMO_API_KEY
```

Those four are the only ones the denylist caught. Everything below survived
into the sandbox. Grouped:

- **X11 / GUI:** `DISPLAY`, `XAUTHORITY`, `XDG_SESSION_TYPE`, `XDG_SESSION_ID`,
  `XDG_SESSION_CLASS`, `XDG_SEAT`, `XDG_SEAT_PATH`, `XDG_SESSION_PATH`,
  `XDG_CURRENT_DESKTOP`, `XDG_SESSION_DESKTOP`, `XDG_VTNR`, `DESKTOP_SESSION`,
  `DESKTOP_STARTUP_ID`, `GDMSESSION`, `GTK_MODULES`, `GTK_USE_PORTAL`,
  `QT_ACCESSIBILITY`, `QT_QPA_PLATFORMTHEME`.
- **IPC / agent sockets:** `DBUS_SESSION_BUS_ADDRESS` (`unix:path=/run/user/1000/bus`),
  `I3SOCK` (`/run/user/1000/i3/ipc-socket.2412`), `WEZTERM_UNIX_SOCKET`
  (`/run/user/1000/wezterm/gui-sock-4114215`), `GPG_AGENT_INFO`
  (`/run/user/1000/gnupg/S.gpg-agent:0:1`), `SSH_AGENT_PID` (2490),
  `KDEWM`, `WEZTERM_EXECUTABLE`.
- **Host identity / path disclosure:** `HOME`, `USER`, `LOGNAME`, `SHELL`,
  `PWD`, `OLDPWD`, `DESKTOP_STARTUP_ID` (contains the full WM command line),
  `WEZTERM_CONFIG_FILE` (`/home/gabriel/.wezterm.lua`), `NVM_DIR`,
  `XDG_DATA_DIRS`, `XDG_GREETER_DATA_DIR`, `DEBEMAIL`, `DEBFULLNAME`.
- **Third-party endpoints:** `XIAOMI_MIMO_API_BASE` (a non-`*_KEY` API base
  URL — the matching `XIAOMI_MIMO_API_KEY` is dropped, the base URL is not),
  `NVM_NODEJS_ORG_MIRROR` and friends.

### Secret-shaped names the denylist misses

These are common in the wild and are **not** matched by any of the five
default patterns:

```
PASSWORD, DOCKER_PASSWORD, PGPASSWORD, NETRC, KUBECONFIG,
GOOGLE_APPLICATION_CREDENTIALS, CLOUDSDK_CORE_PROJECT,
SSL_CERT_FILE, NPM_CONFIG_USERCONFIG,
GIT_ASKPASS, SSH_ASKPASS, HOSTALIASES, LD_PRELOAD
```

`SSH_ASKPASS` is the notable one: it names a program to run, so it is a
*code-execution* vector rather than merely a disclosure. `LD_PRELOAD` is worse
in principle, though the dynamic loader is mostly a no-op for setuid binaries
and the sandbox is not setuid.

`SSH_AUTH_SOCK` is dropped by name, but `SSH_AGENT_PID` is not — and the pid
is enough to find the agent's socket and talk to it (see Finding 3's socket
analysis: a *filesystem* socket survives `--unshare-net`).

### The gaps are structural, not a matter of adding patterns

A denylist is unbounded: every new tool invents a new secret name. The
denylist is also the wrong shape for the question being asked. The right
question is not "which secrets should we hide?" but "which variables does a
sandboxed build need?" — a small, stable set. That is an allowlist.

Prior already has the `OnlyAllow` mode implemented and tested
(`src/config.rs:1074`, `src/tools.rs:2877`). Switching the default is a
one-line change to `SandboxConfig::env_policy` (`src/config.rs:1158`).

---

## Finding 3 — X11 is reachable from inside the sandbox

### Mechanism

The X11 client library resolves `DISPLAY=:0` to a Unix socket at
`/tmp/.X11-unix/X0`. Modern X servers bind **both** a filesystem socket and an
**abstract** socket (`@/tmp/.X11-unix/X0`). The author confirmed both exist:

```
$ grep X11 /proc/net/unix | awk '{print $NF}' | sort -u
/tmp/.X11-unix/X0
@/tmp/.X11-unix/X0
```

Abstract sockets live in a **network-namespace-scoped** namespace. That gives a
precise rule:

- **`--unshare-net` (prior's default, `network: false`)** → the abstract
  socket is unreachable → X11 is blocked. CONFIRMED.
- **no `--unshare-net` (`network: true`)** → the abstract socket is reachable
  → X11 works. CONFIRMED.

### Evidence — X11 works with `network: true`

```
### A: with --unshare-net (prior default when network=false)
Error: Can't open display: (null)
Failed creating new xdo instance

### B: without --unshare-net (prior when network=true)
1257
4194367
4194306
```

And the transport is confirmed to be the abstract socket, not the filesystem
one:

```
filesystem: [Errno 2] No such file or directory
abstract:   CONNECTED
```

Note that the *filesystem* socket is separately dangerous in the opposite
direction: it is not subject to the network namespace at all. A test with a
plain filesystem socket confirmed it connects even under `--unshare-net`:

```
FILESYSTEM UNIX SOCKET CONNECTED (with --unshare-net)
```

So `--unshare-net` blocks the abstract socket but **not** filesystem Unix
sockets. If an operator binds a directory that contains a socket, that socket
is reachable regardless of the network setting.

### What is actually exposed

Not merely a display number — full control of the user's session:

```
windows:            1257 4194367 4194306 ...
active window title: Security Disclosure | Hacker News — Mozilla Firefox
clipboard read:      <clipboard contents>
XTEST extension:     present
```

The author's own X server advertises the `XTEST` extension, which permits
**synthetic keyboard and mouse input** — i.e. arbitrary input injection into
every application the user has open. A screenshot of the root window was also
captured successfully from inside the sandbox (`import -window root`, 649 KB
PNG), and the clipboard was written and read back with the sentinel value
intact.

### Why the current env policy makes this worse

`DISPLAY` and `XAUTHORITY` both survive the default denylist, so the sandboxed
command is *handed* the display to connect to. An allowlist that drops
`DISPLAY` removes the hint but does not remove the capability: a determined
command can guess `:0`, and the abstract socket is at a fixed path.

### The `network: true` interaction

`network: true` is the operator's explicit "let this command reach the
network" switch. It is reasonable to read that as *TCP* egress. It is not
reasonable to read it as "the command may drive my mouse and read my
clipboard". X11 is a **local IPC** channel, not network egress, so it should
be governed by a separate, dedicated control — not inherited from the
network knob.

---

## Cross-check: Anthropic's sandbox-runtime

`srt` (<https://github.com/anthropics/sandbox-runtime>, "Beta Research
Preview", TypeScript) is the closest comparable implementation: it uses
`sandbox-exec` on macOS and `bubblewrap` on Linux, with proxy-based network
filtering. It is a useful reference for two of the three findings, and it
confirms the third is not yet addressed anywhere.

### Their network architecture is the inverse of slirp4netns

Their Linux design (`src/sandbox/linux-sandbox-utils.ts`, "Initialize the Linux
network bridge") is:

```
host:    proxy (HTTP :3128 / SOCKS :1080)  <-- socat UNIX-LISTEN --> unix socket
sandbox: --unshare-net, unix socket bound in, socat TCP6-LISTEN:3128 -> UNIX-CONNECT
         HTTP_PROXY=http://localhost:3128
```

No `slirp4netns`, no `/dev/net/tun`, no IPv6 caveat. Their own comment
states the tradeoff plainly:

> "Linux's `--unshare-net` provides only all-or-nothing network isolation.
> Domain filtering happens at the host proxy level, not the sandbox boundary."

Two consequences for prior:

- **The proxy bridge is a viable alternative to slirp4netns** if prior ever
  wants *filtered* egress (allowlist by domain) rather than raw egress. It
  needs `socat`, which is an extra runtime dependency, and it only works for
  tools that honour `HTTP_PROXY`/`ALL_PROXY` — `curl` with `--resolve`, raw
  sockets, and many CLIs do not.
- **Their approach confirms `--unshare-net` is the right default for prior.**
  The netns is what isolates the abstract socket; the proxy is what makes
  filtered egress possible on top of it.

### They block Unix sockets with seccomp — a good answer to Finding 3

Their `allowUnixSockets` config option is documented as *"macOS only: Unix
socket paths to allow. Ignored on Linux (seccomp cannot filter by path)"*. On
Linux they instead use a **seccomp BPF filter** that denies
`socket(AF_UNIX, ...)` entirely
(`vendor/seccomp-src/seccomp-unix-block.c`).

I built their filter and confirmed it blocks X11 completely, **even with
`network: true`** (i.e. without `--unshare-net`):

```
# with their seccomp filter, no --unshare-net:
xdotool search --name ""  →  Error: Can't open display
abstract connect          →  blocked: [Errno 1] Operation not permitted

# control, same sandbox without the filter:
xdotool search --name ""  →  1257 4194367      (X11 works)
```

and confirmed it does not break TCP:

```
TCP OK - filter does not block inet sockets
```

The reason it works: connecting to a Unix socket requires first calling
`socket(AF_UNIX, …)`, so denying that syscall denies *all* Unix socket use —
including the abstract X11 socket. This is a **barrier**, not a hint, which is
strictly better than removing `DISPLAY`.

It is, however, **all-or-nothing**: it cannot permit a specific socket, so it
also breaks legitimate Unix-socket use inside the sandbox. Landlock's
`ResolveUnix` (ABI 9+) does not have that limitation — see "Cross-check:
Landlock, and the Rust alternatives" below, which revises this into a
layered recommendation (R3).

Two details in their filter are worth adopting directly:

- They deny `io_uring_setup/enter/register`, because `IORING_OP_SOCKET`
  (Linux 5.19+) creates sockets in kernel context and would bypass the
  `socket()` rule.
- They deny `AF_VSOCK`, because the network namespace does not fence VM
  sockets — on a host with a vsock transport, one connects straight out of
  the sandbox, past the proxies.

They also ship a second filter, `namespaces`, denying `unshare`/`clone` with
`CLONE_NEWUSER|CLONE_NEWNS`, plus `setns`, `mount`, `pivot_root`,
`open_tree`, `move_mount`, `fsopen`, `fsconfig`, `fsmount`, `fspick` and
`mount_setattr`. That prevents a sandboxed process from making a new user
namespace and escaping the netns it was given.

### Their env model is different, and Finding 1 applies to them too

They **never emit `--clearenv`** (0 occurrences in
`linux-sandbox-utils.ts`). They inherit the full parent environment and use
`--unsetenv` for specific denied names, plus `--setenv` for masked
credentials. So:

- They do **not** have Finding 2 (no leaky denylist) — their env handling is
  deny-by-name, but for explicit configured names rather than a glob default.
- They **do** have Finding 1, in a worse form: they mount `--proc /proc` and
  run with `--unshare-pid`, so `/proc/1/environ` exposes the host environment
  to the sandboxed command. Their `namespaces` seccomp filter does not help —
  reading `/proc/<pid>/environ` is a plain file read, not a namespace
  operation.

This is a useful cross-check: Finding 1 is not an exotic edge case, it is what
a production-quality reference implementation does today. It is also not
something they appear to have addressed, which suggests it is worth prior
addressing explicitly rather than assuming the reference has it covered.

Their credential *masking* design is also worth noting as a third option for
the env problem: instead of dropping a secret, replace it with a sentinel and
have the proxy substitute the real value back on egress to permitted hosts.
That is more machinery than prior needs today, but it is the design that
supports "the command can use the credential, but only for this domain".

### Two differences worth adopting regardless

- **They always bind `--dev /dev`.** prior currently mounts neither `/dev` nor
  `/proc` by default, which my tests showed breaks `git` outright
  (`fatal: could not open '/dev/null' for reading and writing`). Mounting
  `/dev` is the more usable default and costs nothing.
- **They always mount a fresh `/proc`**, for the reason given in Finding 1:
  without it the sandbox can read the host's `/proc` and learn what is running
  outside. prior should do the same, accepting that it makes R1 mandatory.

---

## Cross-check: Landlock, and the Rust alternatives

The survey at `~/src/research/261021_sandbox_survey.md` covers the landscape
comprehensively; this section records what I verified by hand against the
`nono` checkout at `~/src/nono`, because it changes the recommendation for
R3.

### Landlock is a better fit than bwrap for the X11 problem

Landlock is an LSM, not a namespace tool: an unprivileged process
irreversibly restricts its own ambient rights, and the restrictions are
inherited by children. It needs no user namespace, and it has two relevant
properties that bwrap does not:

- **ABI 6 (Linux 6.12): `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`** — restricts a
  sandboxed process from connecting to an abstract UNIX socket created
  outside its Landlock domain. That is *exactly* the X11 vector, since the
  X11 server's socket lives in the host's abstract namespace.
- **ABI 9 (Linux 7.1): `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`** — kernel-enforced
  per-path control of *pathname* UNIX socket connections.

Both were verified on this host (kernel 7.1.13, Landlock ABI 9) with the
`landlock` 0.4.7 crate:

```
# abstract socket (the X11 vector)
before landlock:: CONNECTED (not blocked)
after  landlock:: BLOCKED (Operation not permitted (os error 1))

# TCP is unaffected
before landlock TCP:: TCP CONNECTED (allowed)
after  landlock TCP:: TCP CONNECTED (allowed)
```

And for pathname sockets, with `ResolveUnix` granted only under one
directory:

```
granted dir socket:   connect(/tmp/ll_ok/s.sock)   CONNECTED (allowed)
non-granted dir socket: connect(/tmp/ll_no/s.sock) BLOCKED (Permission denied)
```

This is a **better** answer than the seccomp filter in R3, for three reasons:

1. It is **per-path**, not all-or-nothing. seccomp can only deny
   `socket(AF_UNIX)` wholesale, which also breaks any legitimate Unix-socket
   plumbing (prior's `agent-browser` tool, a proxy bridge). `ResolveUnix`
   allows exactly the sockets that are named and denies the rest.
2. It **does not block `socket()`**, so tools that create a Unix socket for
   inter-process coordination keep working.
3. It is **not gated on the network setting** — the isolation comes from the
   LSM, not a netns, so `network: true` no longer implies "may drive the
   user's mouse".

### The important caveat: Landlock does not cover everything

Landlock's filesystem rules do **not** govern pathname AF_UNIX `connect()` on
kernels below ABI 9. Verified with ABI V4 rules applied and a real pathname
socket:

```
before landlock:: connect(/tmp/ll_path_test.sock) CONNECTED (not blocked)
after  landlock:: connect(/tmp/ll_path_test.sock) CONNECTED (not blocked)
```

This is not a gap in my test — it is a documented escape class. `nono`'s
NEP-0004 (`neps/0004-opt-in-linux-namespace-isolation.md`) states it plainly:

> **Pathname AF_UNIX `connect()`** is not governed by Landlock filesystem
> rules. From inside a sandbox, `/run/user/<uid>/bus` reaches the D-Bus session
> bus or the systemd user manager socket (`systemd-run` starts processes
> *outside* the sandbox), `docker.sock`, X11/Wayland, and `ssh-agent`.

So the D-Bus socket, `docker.sock`, `ssh-agent` and the systemd user manager
are reachable by pathname regardless of Landlock's filesystem rules, unless
the kernel is new enough for `ResolveUnix` (7.1+) — and even then only for
paths that were not granted.

nono's own answer is instructive: its `af_unix_mediation` countermeasure
(seccomp user-notify supervision) is **off by default and "excluded from
`nono wrap`"**, and they explain why mediation is not a sound boundary at all:

> intercept-and-continue mediation of pointer-argument syscalls is not a sound
> security boundary: the kernel documentation for seccomp user notification is
> explicit that the notified memory remains writable by the target, so the
> validated `sockaddr` bytes are subject to rewrite between check and use.
> Mediation inspects a hostile interface point-by-point; a namespace removes the
> interface wholesale.

That reasoning applies to prior too. **Reachability removal beats
mediation**, which is why the netns (`--unshare-net`) remains the right
primary control for the *abstract* socket, and why R3's seccomp filter is
best understood as a *complement* rather than the primary defence.

### What `nono` does and does not settle

`nono` (`nolabs-ai/nono`, Apache-2.0) is the most complete Rust option and
the closest analogue to `srt`. Relevant to prior:

- It caps its **enforced** Landlock ABI at **V6**
  (`ABI_PROBE_ORDER = [V6, V5, V4, V3, V2, V1]`,
  `crates/nono/src/sandbox/linux.rs:426`), while the `landlock` crate it
  ships (0.4.7) exposes V9. So nono *can* scope abstract sockets but *cannot*
  yet use `ResolveUnix`. NEP-0004 tracks the catch-up as its own work item.
- It denies abstract-namespace sockets explicitly: "Abstract-namespace
  sockets (`sun_path[0] == '\0'`) and unnamed sockets are never covered by a
  grant […] Those kinds are denied by the sandbox's
  `decide_network_notification` policy on Linux"
  (`crates/nono/src/capability.rs:340`).
- Its namespace isolation is **opt-in and default-off**
  (`linux.isolate` / `--isolate`), deliberately, so that enforcement does not
  vary host to host. prior's bwrap namespaces are unconditional, which is
  the opposite trade.
- Its Landlock path is **immune to the Ubuntu 24.04 AppArmor userns
  restriction** that breaks bwrap — a real deployment advantage for a tool
  installed on arbitrary laptops.

nono does **not** address Finding 1. Its `scrub.rs` is output hygiene
(redacting secrets from audit records), not environment isolation, and it does
not scrub the *parent's* environment the way R1 requires.

### Recommendation change

R3 is revised: **Landlock becomes the primary X11 control where available,
with seccomp and the netns as layered fallbacks.** See R3 for the revised
text. The new ordering is:

1. `--unshare-net` (already prior's default) — removes the abstract socket by
   unreachability. Works on every kernel, and is what makes `network: true`
   safe.
2. Landlock `ResolveUnix` (ABI 9+) or abstract-socket scope (ABI 6+) — adds
   per-path control, so the control no longer depends on the netns and
   pathname sockets can be handled too.
3. seccomp deny `socket(AF_UNIX)` — the blunt fallback for kernels without
   Landlock, and the one that also covers `AF_VSOCK` and `io_uring`.

prior should not adopt nono as a dependency: it is a large surface (a
supervisor process, a proxy, a trust store, an audit log) that duplicates
policy prior already has. But it is an excellent **reference**, and its
Landlock + seccomp + namespace layering is a sound model to copy.

---

## Additional observations (not separately requested, found during the audit)

- **`/dev` is usually absent from the sandbox.** With the default config
  (no bindings) prior mounts no `/dev`, so `echo hi > /dev/null` fails with
  `Directory nonexistent` and `/proc` is likewise absent. Only a config that
  binds `/dev` or `/proc` gets them. This is fail-closed and correct, but it
  surprises tools that expect `/dev/null` — and `git` fails without it.
- **`/etc` is ro-bound whole** when no `/etc` binding is configured, which
  exposes `/etc/hosts`, `/etc/ssl` (CA store), `/etc/localtime` and
  `/etc/passwd` — mild host fingerprinting, no secrets.
- **The UTS namespace is not unshared**, so `hostname` inside the sandbox
  returns the host hostname (`pad`). Note that `--unshare-uts` alone does not
  change this — a new UTS namespace still inherits the hostname until
  `--hostname` is given:
  `--unshare-uts --hostname sandbox` reports `sandbox`. Closing the leak
  needs both flags.
- **The IPC namespace is not unshared**, so the sandbox sees the host's
  System V shared-memory segments (`ipcs -m` lists 8 of them). `--unshare-ipc`
  closes this; it is also the namespace that would matter for X11's MIT-SHM
  path, which is a faster and more direct way to read framebuffers than
  `XGetImage`.
- **`--cap-drop ALL` is a no-op for an unprivileged user.** Measured with and
  without the flag, `CapEff`/`CapBnd` are `0000000000000000` either way —
  the user never held capabilities to begin with, so there is nothing to drop.
  The flag is not harmful, but it is not contributing anything.
- **The sandbox does run in a new user namespace** (the `user` ns inode
  differs from the host's), but the uid is **not** remapped to root:
  `/proc/self/uid_map` inside the sandbox reads `1000 1000 1`, i.e. the host
  uid maps to itself. So the sandboxed command is still uid 1000 with the
  host user's real privileges on anything that *is* bound. "Hide the host
  home directory" is achieved by not binding it, not by uid isolation.
- **The `ipc`, `uts` and `cgroup` namespaces are shared with the host** (the
  ns inodes are identical inside and outside), so `--unshare-ipc` and
  `--unshare-uts` are both currently missing.

---

## Recommendations

Ordered by priority. R1 and R2 are implemented; R3 and R4 are not.

### R1 — Scrub bwrap's *own* environment before it spawns the payload (prerequisite for R2/R3)

**Implemented** — see "Implementation notes" above.

The env filter cannot be trusted while `--proc /proc` is present, because the
bwrap parent's environ still holds the host secrets. The fix is not to remove
procfs — it is to make sure **bwrap itself** never inherits the host
environment in the first place.

If prior launches bwrap with a scrubbed environment (only the vars bwrap
itself needs — in practice just `PATH`), then bwrap's `/proc/1/environ` contains
only that scrubbed set, and the child inherits nothing extra. The bypass
disappears entirely. CONFIRMED:

```
$ ANTHROPIC_API_KEY=sk-ant-SECRET SSH_AUTH_SOCK=... DBUS_SESSION_BUS_ADDRESS=... \
  env -i PATH="$PATH" HOME="$HOME" \
  bwrap ... --proc /proc --clearenv --setenv PATH "$PATH" -- sh -c '...'

--- via /proc/1/environ (bwrap parent) ---
  NONE - clean
--- via /proc/1/cmdline ---
  (no secrets)
```

Note the mechanism: the secrets never reach bwrap, so there is nothing for
`/proc/1/environ` to leak. The `--setenv` values the *policy* permits are
still visible via `/proc/1/cmdline` — that is the remaining, much smaller
exposure, and it is inherent to bwrap's argument-vector interface.

In Rust this means clearing the environment of the `bwrap` child process
before `exec`, rather than relying on `--clearenv` alone (which only affects
the *child* of bwrap, not bwrap itself).

prior spawns bwrap through **duct** (`src/tools.rs:1915`, `src/tools.rs:1939`)
and through `std::process::Command` (`src/tools.rs:1177`). Both need the
scrub:

- `duct` already calls `command.env_clear()` and then applies the
  expression's env map, but that map is *seeded from the parent process* by
  default (`duct-1.1.2/src/lib.rs:1643`). Calling `.full_env(...)` replaces
  the map wholesale, which is exactly the primitive needed. Verified against
  the real library:

  ```
  default duct     -> MY_SECRET=leaked_value_123
  duct .full_env   -> NONE-CLEAN
  ```

- For the `std::process::Command` path (`sandbox_list_lines`), add
  `.env_clear()` and set only the vars bwrap needs.

bwrap itself needs very little from its environment: `PATH` (to resolve the
program), and `HOME`/`LANG` are harmless to keep. Note that bwrap is invoked
as `bwrap` (not an absolute path), so `PATH` must be present in *bwrap's* own
environment for the exec to succeed.

**Do not remove procfs as the primary fix.** procfs is load-bearing: both
`rustc` and `cargo` fail with
`Unable to proceed. Could not locate working directory.: no /proc/self/exe
available. Is /proc mounted?` when it is absent, and `git` additionally needs
`/dev/null`. A Rust-oriented coding agent would break immediately. Keeping
procfs and scrubbing bwrap's environ is both safer and more usable.

Two caveats worth recording:

- This makes the *env filter* trustworthy, but `/proc/1/cmdline` still exposes
  the permitted `--setenv` values and the full set of `--ro-bind`/`--bind`
  source paths. That is host fingerprinting, not secret leakage.
- If a binding ever exposes a host **socket** path, that socket is reachable
  regardless of the env policy (see R4).

Recommended: scrub bwrap's environment at spawn time; keep procfs, and mount
it by default (see R3's `--dev`/`--proc` note).

### R2 — Switch the default env policy to an allowlist

**Implemented** — see "Implementation notes" above.

Make `OnlyAllow` the default in `SandboxConfig::env_policy`
(`src/config.rs:1158`), with a pattern list covering what a build actually
needs:

```
PATH, HOME, TERM, LANG, LC_*, USER, LOGNAME, SHELL,
CARGO_HOME, RUSTUP_HOME, RUSTUP_TOOLCHAIN, XDG_CACHE_HOME, XDG_CONFIG_HOME,
XDG_DATA_HOME, XDG_STATE_HOME, TZ, SOURCE_DATE_EPOCH
```

Three deviations from the list as originally proposed, all deliberate:

- **`PWD` dropped.** bwrap's `--clearenv` sets `PWD` to the in-namespace cwd,
  which is what the command should see. A host `PWD` would name a path that
  need not exist inside the sandbox.
- **`TMPDIR` dropped from the list, and set to `/tmp` by the sandbox.** `/tmp`
  inside the sandbox is a fresh tmpfs; the host's `TMPDIR` names a path that
  is not mapped in at all, so inheriting it would point tools outside the
  sandbox's scratch area. The sandbox sets it explicitly instead.
- **`GIT_*` dropped, and `CI` dropped.** `GIT_SSH_COMMAND`, `GIT_ASKPASS` and
  `GIT_CONFIG_*` are behavioural hooks that *execute* programs, so they belong
  on the deny side even though the `GIT_` prefix reads as benign. `CI` is
  left out only because it changes tool behaviour in ways that are easy to get
  wrong (`CI=true` makes many tools non-interactive); add it explicitly if
  wanted.

Observations that shape the list:

- `GIT_*` deserves a decision rather than a default: `GIT_SSH_COMMAND`,
  `GIT_ASKPASS` and `GIT_CONFIG_*` are *behavioural* hooks that execute
  programs, so they belong on the deny side by default even though `GIT_*`
  reads naturally as benign. If `GIT_*` is allowed wholesale, `GIT_ASKPASS`
  must be excluded explicitly.
- `LC_*` and `TZ` are needed for reproducible output; `LC_ALL`/`LANG` are
  already relied on by the current tests.
- `TMPDIR` is set by the sandbox rather than inherited — see above.

The deny list does not disappear — it becomes a **second** layer on top of the
allowlist (R1 and R2 are complementary). A sensible end state is
"allowlist, then remove these specific names from the allowed set", so that a
name that is both needed and dangerous (`SSH_AUTH_SOCK` is not needed;
`GIT_ASKPASS` might be) can be excluded without a second policy mode.

Note that `EnvPolicy` is currently a single mutually-exclusive mode
("you cannot combine a blacklist and a whitelist in one policy",
`src/config.rs:1046-1048`, unchanged). Supporting "allowlist, then subtract" needs
either a third mode or a separate `deny` list. Worth deciding explicitly,
since the config surface is user-facing and documented in
`settings.toml.example:109-137` and `doc/sandbox.md:275-366`.

`srt` offers a third option worth noting for the future: **masking** rather
than dropping — replace a secret with a sentinel and have the egress proxy
substitute the real value back for permitted hosts. That is more machinery
than prior needs today, but it is the design that supports "the command may
use the credential, but only for this domain".

#### Implementation notes — R1 and R2 (done)

R1 and R2 are implemented. The two are complementary, and R1 is the
prerequisite: without it the allowlist stays decorative, because bwrap's own
`/proc/1/environ` would still hold everything the allowlist dropped.

**R1 — scrub bwrap's own environment.** `config::bwrap_scrub_env` returns the
environment bwrap itself is spawned with: `PATH`, and nothing else. Both spawn
sites use it — the two `duct` paths via `.full_env(...)` (which replaces
duct's inherited environment wholesale) and the `std::process::Command` path in
`sandbox_list_lines` via `.env_clear()` plus explicit vars.

Two things the implementation turned up that the recommendation above did not
state:

- **`HOME` is not needed.** The recommendation had bwrap keeping `HOME` so it
  could resolve `~` in bind arguments. It cannot: bwrap does not expand `~` at
  all (`bwrap: Can't find source path ~/fakebin`), and prior expands those
  itself in `resolve_with_home` before building the args. The scrub set is
  therefore `PATH` alone.
- **bwrap's `PATH` is only needed to find the `bwrap` binary.** The
  *sandboxed* program is resolved against the **child's** `PATH` — bwrap
  `execvp`s the payload with the environment it just set up. So bwrap's own
  `PATH` and the command's `PATH` are independent, and the policy still
  governs the command's.

The regression test is `bwrap_own_environ_does_not_leak_host_secrets` in
`src/tools.rs`. Two details make it non-vacuous, both found the hard way:

- The default sandbox has **no procfs**, so `/proc/1/environ` does not exist
  and the test would pass trivially. The test binds `/proc` explicitly.
- The default sandbox has **no `/dev`**, so `2>/dev/null` fails with
  "cannot create /dev/null: Directory nonexistent" — and because the
  redirection is set up before the command runs, the *whole* pipeline never
  executes, so the test sees an empty result and passes. The test reads
  `/proc/1/environ` with no redirection.

With the fix reverted, both tests fail and dump the entire host environment
(`XAUTHORITY`, `DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, ...). With it, they
pass. A second test, `bwrap_own_environ_is_scrubbed_not_merely_filtered`,
uses a variable that is *not* a secret, to show that no value-based policy
could have caught this.

**R2 — allowlist by default.** `config::DEFAULT_ENV_ALLOW` is the new
default, and `EnvPolicy::OnlyAllow` is the default mode. The list is the one
in the recommendation above, minus `TMPDIR` (see below) and minus `GIT_*`
(kept out deliberately: `GIT_SSH_COMMAND`, `GIT_ASKPASS` and `GIT_CONFIG_*`
are behavioural hooks that *execute* programs).

`TMPDIR` is handled differently than recommended. Rather than inheriting the
host's `TMPDIR` and rewriting it, the sandbox **sets `TMPDIR=/tmp` itself**
(`config::DEFAULT_FORCED_ENV`) — `/tmp` inside the sandbox is a fresh tmpfs,
so the host's `TMPDIR` names a path that is not mapped in at all. `TMPDIR` is
therefore not on the allowlist, and the forced value is emitted *after* the
policy's `--setenv` values, so it wins bwrap's "last `--setenv` wins"
ordering.

The `EnvPolicy` mode question raised above is still open: "allowlist, then
subtract" is not implemented. What *is* implemented is that an explicit
`sandbox.env` block is authoritative in full — the built-in allowlist and the
forced `TMPDIR` are not mixed in behind the operator's back, so the old
denylist behaviour is still available by asking for it explicitly.

### R3 — Block X11 independently of the network knob (layered)

Do not let `network: true` imply "may drive the user's mouse". X11 is a
**local IPC** channel, not network egress, and it should be governed by a
dedicated control rather than inherited from the network knob.

Three layers, strongest first. The ordering changed after checking Landlock
(see "Cross-check: Landlock, and the Rust alternatives") — Landlock is a
better primary control than seccomp, because it can be *per-path*.

**Layer 1 — Landlock, where the kernel supports it (primary).** Landlock is
an LSM: an unprivileged process restricts its own ambient rights, the
restriction is inherited by children, and it needs no user namespace. Two
relevant access rights, both verified on this host (kernel 7.1.13, ABI 9):

- **ABI 6 (Linux 6.12): `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`** — blocks
  connecting to abstract sockets created outside the sandbox's Landlock
  domain. This is the X11 vector exactly.
- **ABI 9 (Linux 7.1): `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`** — per-path control
  of *pathname* socket connects, which is what closes the D-Bus /
  `docker.sock` / `ssh-agent` class.

Measured, using the `landlock` 0.4.7 crate:

```
# abstract socket (the X11 vector)
before landlock:: CONNECTED (not blocked)
after  landlock:: BLOCKED (Operation not permitted (os error 1))
# TCP is unaffected
after  landlock TCP:: TCP CONNECTED (allowed)

# pathname sockets, ResolveUnix granted under one directory only
granted dir socket:   connect(/tmp/ll_ok/s.sock)   CONNECTED (allowed)
non-granted dir socket: connect(/tmp/ll_no/s.sock) BLOCKED (Permission denied)
```

This is strictly better than a blanket seccomp deny: it is per-path, it does
not block `socket()` itself, and it does not depend on the netns.

The `landlock` crate is a small, dependency-light addition (it is pure
`libc` syscalls). Two cautions from the crate's own docs and nono's NEP-0004:

- **Pin an explicit ABI.** The crate deliberately refuses to expose "all
  access flags for the running kernel" — a ruleset built against "whatever
  this kernel supports" silently changes semantics when the crate is updated.
- **Handling `ResolveUnix` without granting anything flips existing profiles
  into deny-all-unix-connects** on V9 kernels: fail-closed, but breaking.
  It needs curated default grants, the same class of problem nono tracks as
  #1827.

**Layer 2 — the network namespace (reachability).** `--unshare-net` already
removes the abstract socket by unreachability, and it is prior's default.
This is the layer that makes `network: true` safe, and it is the one that
works on every kernel. nono's own reasoning is worth adopting as a
principle:

> Mediation inspects a hostile interface point-by-point; a namespace removes
> the interface wholesale.

**Layer 3 — seccomp (fallback, and the only layer that covers vsock and
io_uring).** Deny `socket(AF_UNIX, …)` so the sandboxed command cannot
create a Unix socket at all. `srt`'s filter
(`vendor/seccomp-src/seccomp-unix-block.c`) is a working reference and was
verified here to block X11 with `network: true`. The filter should deny, at
minimum:

- `socket(AF_UNIX, …)` — blocks X11, D-Bus, and every other Unix socket.
- `socket(AF_VSOCK, …)` — VM sockets are not fenced by the network namespace.
- `io_uring_setup` / `io_uring_enter` / `io_uring_register` — `IORING_OP_SOCKET`
  creates sockets in kernel context and bypasses the `socket()` rule.
- Optionally the `namespaces` rules from `srt` (deny `unshare`/`clone` with
  `CLONE_NEWUSER|CLONE_NEWNS`, `setns`, `mount`, `pivot_root`, …) so a
  sandboxed process cannot escape by making a new user namespace.

bwrap supports this natively: `bwrap --seccomp FD` (an FD, not a path).

**Constraint to design around.** Denying `socket(AF_UNIX)` also denies
*creating* the client socket needed to reach a Unix socket that was bound
into the sandbox — and, more importantly for prior, it denies the socket
creation that ordinary tooling does for inter-process coordination. Verified:

```
s=socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
→ PermissionError: [Errno 1] Operation not permitted
```

So any Unix-socket-based plumbing (an `agent-browser` control socket, a
proxy bridge) must either present a **TCP** endpoint to sandboxed code, or
have its socket created *before* the filter is applied — `srt` does the latter
by starting its `socat` relays before `apply-seccomp`, and the former by
using `TCP6-LISTEN:3128` inside the sandbox. prior should pick one
deliberately rather than discover this at runtime.

**Secondary control — always drop `DISPLAY`, `XAUTHORITY`, `WAYLAND_DISPLAY`
and `XDG_RUNTIME_DIR`** from the environment, in *both* policy modes. These are
never needed by a build and are the enabling hint for Finding 3. Under an
allowlist (R2) they are excluded automatically; under the current denylist
they are not, so this should be added to the deny list as a stopgap for
operators who keep the denylist.

**Also in scope for R3:**

- **Mount `/dev` and `/proc` by default.** `srt` does both, and the
  alternative is a sandbox where `git` fails on a missing `/dev/null` and
  `cargo` fails on a missing `/proc/self/exe`. Mounting `/proc` makes R1
  mandatory — which is the correct dependency, not an argument against it.
- **Add `--unshare-ipc`** regardless. It closes the shared-memory exposure and
  the MIT-SHM framebuffer path, and it is the namespace that X11's
  shared-memory transport actually uses. It is a free win, and it also stops
  the sandbox from listing the host's SysV segments.
- **Consider `--unshare-uts --hostname sandbox`** to stop the host hostname
  leaking. Note that `--unshare-uts` alone is not enough — the new namespace
  still reports the host hostname until `--hostname` is given.

A reasonable target state: X11 is blocked in *all* configurations, and
`network: true` means TCP/UDP egress only. Concretely, in priority order:

1. `--unshare-net` stays the default (already true) and is what makes
   `network: true` safe.
2. Landlock is layered on where the kernel supports ABI 6+ (abstract socket
   scope) and ABI 9+ (`ResolveUnix` for pathname sockets), with a
   capability probe and a graceful fallback to layer 3.
3. The seccomp filter is the fallback for kernels without Landlock, and the
   only layer that covers `AF_VSOCK` and `io_uring`.
4. `DISPLAY` / `XAUTHORITY` / `WAYLAND_DISPLAY` / `XDG_RUNTIME_DIR` are
   dropped from the environment in both policy modes, so the sandboxed
   command is not even *told* where the display is.

The order matters: Landlock first because it is per-path and does not break
legitimate Unix-socket use; seccomp only where Landlock is unavailable,
because it is all-or-nothing.

### R4 — Make the Unix-socket surface explicit

`--unshare-net` blocks abstract sockets but **not** filesystem sockets. A
directory binding can therefore re-open a channel the network namespace
closed. Two complementary steps:

- Document this in `doc/sandbox.md` next to the `--unshare-net` claim, which
  currently reads as if it blocks all egress.
- Consider dropping the `network` knob's implication and instead describing
  the two independent axes: *network namespace* (TCP/UDP + abstract sockets)
  and *filesystem bindings* (filesystem sockets). An operator who binds
  `~/.config/gcloud` may be handing over a credential helper socket without
  realising it.

### R5 — Close the documentation gap

**Partly done.** `doc/sandbox.md:275-366` ("Environment isolation") and
`settings.toml.example:109-137` have been updated for R1 and R2: the default
is now documented as the allowlist, the reason a denylist cannot be the
default is given (it leaks the desktop/session variables), and the bwrap
scrub is described. The caveat about `/proc/1/cmdline` is recorded.

Still open:

- The old claim that "the sandbox already blocks network egress by default, so
  a leaked key cannot be exfiltrated unless `network: true`" has been removed
  from the `env` section, but the **`--unshare-net` claim** elsewhere in
  `doc/sandbox.md` still reads as if it blocks all egress. It does not: the
  filesystem-socket channel (R4) defeats it.
- The R3 work (a dedicated X11 control) is not done, so the doc still cannot
  promise that the X server is unreachable.

### R6 — Tests

**Partly done.** The existing env tests covered the filter in isolation, and
would not have caught any of the three findings, because all three are about
the *composition* of the filter with the rest of the bwrap invocation. Added
for R1 and R2:

- `bwrap_own_environ_does_not_leak_host_secrets` — the test that would have
  caught Finding 1, and the regression test for the R1 fix. It must mount
  `/proc` to be meaningful; a test without `/proc` passes vacuously, which is
  exactly the blind spot in the original suite.
- `bwrap_own_environ_is_scrubbed_not_merely_filtered` — same, via a variable
  that is not a secret, so no value-based policy could have caught it.
- `bwrap_scrub_env_is_minimal` — pins the shape of the scrub set.
- `sandbox_list_lines_works_with_a_scrubbed_environment` — covers the second
  spawn site, which uses `std::process::Command` rather than duct.
- `default_policy_drops_host_desktop_and_session_vars` — asserts that
  `DISPLAY`, `XAUTHORITY`, `DBUS_SESSION_BUS_ADDRESS`, `XDG_RUNTIME_DIR`,
  `GPG_AGENT_INFO`, `SSH_AGENT_PID` and `I3SOCK` do not survive the default
  policy.
- `default_policy_omits_pwd_and_tmpdir_but_sandbox_sets_tmpdir` and
  `explicit_env_policy_suppresses_the_built_in_defaults` — pin the `PWD` /
  `TMPDIR` decisions and the "an explicit block is authoritative in full" rule.

Still wanted, for R3/R4:

- A test asserting that the seccomp filter blocks `socket(AF_UNIX, …)` while
  leaving `AF_INET` alone, and that `AF_VSOCK` is blocked.
- A test asserting that with `network: true` the X server is still
  unreachable (i.e. that X11 blocking is independent of the network knob).
  This needs a socket-level assertion rather than an `xdpyinfo` probe, since
  `xdpyinfo` may not be installed.

---

## Appendix — reproducing the findings

All commands are plain `bwrap` invocations that mirror prior's
`build_bwrap_args` output. `bubblewrap 0.12.0`, Debian, i3/X11.

**Env leak inventory** (Finding 2):

```sh
bwrap --ro-bind /usr /usr --ro-bind /bin /bin --ro-bind /lib /lib \
      --ro-bind /lib64 /lib64 --ro-bind /etc /etc --ro-bind "$PWD" "$PWD" \
      --size 268435456 --tmpfs /tmp --unshare-net --unshare-pid \
      --cap-drop ALL --die-with-parent --clearenv \
      --setenv PATH "$PATH" --setenv HOME "$HOME" --setenv TERM "$TERM" \
      --setenv LANG "$LANG" -- /usr/bin/env | sort
```

**`/proc` bypass** (Finding 1):

```sh
MY_SECRET=leaked bwrap ... --proc /proc --unshare-pid --clearenv \
  -- sh -c 'tr "\0" "\n" < /proc/1/environ | grep MY_SECRET'
```

**The R1 fix** (scrub bwrap's own environment):

```sh
# leak
MY_SECRET=leaked bwrap ... --proc /proc --clearenv \
  -- sh -c 'tr "\0" "\n" < /proc/1/environ | grep MY_SECRET'

# clean — the secret never reaches bwrap, so there is nothing to leak
# (`PATH` is all bwrap needs; `HOME` is not used by bwrap at all)
MY_SECRET=leaked env -i PATH="$PATH" bwrap ... --proc /proc --clearenv \
  -- sh -c 'tr "\0" "\n" < /proc/1/environ | grep MY_SECRET || echo NONE-CLEAN'
```

Note for anyone reproducing this by hand: the *default* sandbox has neither
`/proc` nor `/dev` bound, so a test that reads `/proc/1/environ` must bind
`/proc`, and must not redirect stderr to `/dev/null` (the redirection fails
before the command runs, so the pipeline never executes and the check passes
vacuously). Both pitfalls are described under R1's implementation notes.

**X11 reachability** (Finding 3):

```sh
# blocked (default: --unshare-net)
bwrap ... --unshare-net --setenv DISPLAY :0 -- xdotool search --name ""

# reachable (network: true)
bwrap ... --setenv DISPLAY :0 -- xdotool search --name ""
```

**Abstract vs filesystem socket** (Findings 3 and 4):

```sh
python3 -c "
import socket
for label, addr in [('filesystem','/tmp/.X11-unix/X0'), ('abstract','\0/tmp/.X11-unix/X0')]:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try: s.connect(addr); print(f'{label}: CONNECTED')
    except Exception as e: print(f'{label}: {e}')"
```

**The seccomp X11 block** (R3), using `srt`'s filter:

```sh
git clone --depth 1 https://github.com/anthropics/sandbox-runtime.git
gcc -o /tmp/seccomp-unix-block \
    sandbox-runtime/vendor/seccomp-src/seccomp-unix-block.c -lseccomp
/tmp/seccomp-unix-block /tmp/unix-block.bpf

# with the filter: X11 blocked even with network:true
bwrap ... --seccomp 3 --setenv DISPLAY :0 3</tmp/unix-block.bpf -- xdotool search --name ""
# → Error: Can't open display
```
