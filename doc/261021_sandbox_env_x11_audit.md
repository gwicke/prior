# Sandbox env-var and X11 audit

*Scope: the `bwrap` sandbox that prior builds in `src/tools.rs::build_bwrap_args`. Audited
2026-10-21 against `bubblewrap 0.12.0`, live on the author's host (Debian, i3/X11, Wayland absent).
Findings below are backed by the experiments in "Evidence" — every claim marked CONFIRMED was
reproduced, not inferred.*

## Status

**R1, R2, and R3a/b/c/e/f are implemented.** **R3d, R3g, R3h, R4, R5 and R6 are not.** R3 was
revised on 2026-10-22 after testing the design against four reference implementations and this host:
it is no longer a Landlock/seccomp ladder but a bridge design (R3a–R3h), with Landlock moved to R4
where it is the only fix. The Linux half then landed the same day: `--unshare-net` is now
unconditional, and `network = true` grants egress through a SOCKS5 bridge whose listener is created
inside the sandbox's network namespace (`src/egress.rs`, `src/bin/prior-egress.rs`). Handover notes —
what to read first, what order to build it in, and what is already settled — are at the end of the
document ("Where to start next").

The revision was prompted by one question: *can `network: true` keep the network namespace, so that
the X11 vector disappears by construction instead of needing three layered controls?* The answer is
yes, and both reference implementations already do some version of it — see "Cross-check:
`nono`'s passed-listener bridge" and "Why `network: true` must not share the network namespace".

## Summary

Three findings, in descending severity:

1. **The env filter is bypassable whenever `/proc` is mounted.** The bwrap parent's
   `/proc/1/environ` still holds the *host* environment, so a sandboxed command can read every
   secret the filter dropped. CONFIRMED. A fresh `/proc` does **not** help — see "Does a fresh
   `/proc` help?" below. The fix is to scrub bwrap's *own* environment at spawn time (R1).
2. **The default env policy is a denylist and leaks the entire desktop/session environment.** On the
   author's host it dropped 4 of 66 variables and let 62 through, including `DISPLAY`, `XAUTHORITY`,
   `SSH_AGENT_PID`, `DBUS_SESSION_BUS_ADDRESS`, `GPG_AGENT_INFO` and every WM/IPC socket path.
   CONFIRMED.
3. **X11 is reachable from inside the sandbox**, over the *abstract* Unix socket, whenever
   `network: true` is set — full window enumeration, title reads, clipboard read/write and `XTEST`
   synthetic input are all available. CONFIRMED. `DISPLAY`/`XAUTHORITY` survive the env filter, so
   the sandboxed command is *told* where the display is.

Consequences for the plan of record:

- An allowlist alone is **not** enough. It has to be paired with R1, or the filter stays decorative.
- Finding 3's fix is not "remove `DISPLAY`" (a hint) and — as of the 2026-10-22 revision — not a
  Landlock/seccomp ladder either. It is a **network contract**: the netns stays on unconditionally,
  and `network: true` means *egress through a bridge*, not *share the host netns*. That removes the
  abstract socket by unreachability on every kernel, needs no LSM and no ABI probe, and is the one
  shape that also has a cheap macOS implementation (R3).

Five reference implementations are used throughout:

- Anthropic's [sandbox-runtime](https://github.com/anthropics/sandbox-runtime) (`srt`), the closest
  comparable implementation. It shares Finding 1 (see "Cross-check: Anthropic's sandbox-runtime")
  and supplies the macOS network model and the shape of the SOCKS5 policy.
- `nono`'s NEP-0004, which specifies the Linux bridge as a **passed listening fd** rather than a
  bound socket path — the design R3 adopts, and better than `srt`'s (see "Cross-check: `nono`'s
  passed-listener bridge").
- `zerobox` (Codex's crates), whose `linux-sandbox/src/proxy_routing.rs` is a working socat-free
  bridge of the same family, and several lines of which are load-bearing evidence below.
- Landlock itself, which R3 no longer uses for the X11 vector but which remains the only fix for
  the binding-reopens-a-socket class (R4). See "Cross-check: Landlock, and the Rust alternatives".
- Anthropic's `sandbox-runtime` again, for the macOS side: its Seatbelt profile needs **no bridge at
  all**, which is the inverse of the Linux problem and a large part of why mac support is cheap.

---

## How the sandbox is built today

`build_bwrap_args` (`src/tools.rs:1704`) assembles, in order:

- `--ro-bind` for each of `ESSENTIAL_RO_DIRS` = `/usr /bin /lib /lib64`, plus one per entry in
  `ESSENTIAL_ETC` when no `/etc` binding is configured (a working set, not the whole directory —
  R3e, implemented).
- Fresh `--dev /dev` and `--proc /proc` by default, unless a binding already targets either.
- User `ro`/`rw` bindings; a binding whose in-sandbox target is `/dev` or `/proc` is translated to
  bwrap's `--dev` / `--proc`.
- A `--tmpfs /tmp` scratch area (unless a user binding covers `/tmp`).
- `--unshare-net` unless `network: true` (unconditional once R3a lands); `--unshare-pid` and
  `--cap-drop ALL` by default; `--unshare-ipc --unshare-uts --hostname sandbox` always
  (R3e, implemented); `--die-with-parent`.
- `--clearenv` followed by one `--setenv NAME VALUE` per variable the env policy permits, then the
  sandbox's own forced values (`src/tools.rs:1850-1876`).

The env policy itself (`src/config.rs:1043-1200`) is a two-mode glob filter:

- `EnvMode::Disallow` — keep the whole environment minus matching names.
- `EnvMode::OnlyAllow` — keep only matching names.

The **default**, when `[tool_config.bash.sandbox.env]` is unset, is now `OnlyAllow` with
`DEFAULT_ENV_ALLOW` (`src/config.rs:1119`) — the change recommended as R2 below, and implemented.
`DEFAULT_ENV_DISALLOW` is retained as an explicit opt-in for operators who want the old behaviour.

Separately, bwrap itself is spawned with a scrubbed environment of its own (`PATH` and nothing else,
`config::bwrap_scrub_env` at `src/config.rs:1219`). See R1 below.

---

## Finding 1 — the env filter is bypassable via `/proc` (severity: high)

### Mechanism

`bwrap` is the parent of the sandboxed command. It inherits the host environment and then calls
`clearenv()` + `setenv()` for the child, but
**bwrap's own `/proc/<pid>/environ` still contains the unmodified host environment**. When prior
mounts a fresh procfs with `--proc /proc`, the sandboxed command can read the bwrap parent's environ
and recover every variable the policy dropped.

`--unshare-pid` does not help: bwrap's reaper is PID 1 *inside* the new namespace, and its
`/proc/1/environ` is the host environment.

### Evidence

```text
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

```text
$ ANTHROPIC_API_KEY=sk-ant-SECRET SSH_AUTH_SOCK=/run/user/1000/ssh-agent.sock bwrap ...
child env has API key? []
parent /proc/1/environ:
ANTHROPIC_API_KEY=sk-ant-SECRET
SSH_AUTH_SOCK=/run/user/1000/ssh-agent.sock
```

And against the *live* sandbox this audit ran inside (prior's own bwrap, PID 1 of my namespace),
where the env filter had correctly removed the keys from the child environment:

```text
child sees DEEPSEEK_API_KEY?   [<empty>]
child sees SSH_AUTH_SOCK?      [<empty>]

but /proc/1/environ (bwrap parent) exposes:
DEEPSEEK_API_KEY=<REDACTED-VALUE-PRESENT>
OPENROUTER_API_KEY=<REDACTED-VALUE-PRESENT>
SSH_AUTH_SOCK=<REDACTED-VALUE-PRESENT>
XIAOMI_MIMO_API_KEY=<REDACTED-VALUE-PRESENT>
```

### Does a fresh `/proc` help?

**No.** This is worth stating explicitly, because mounting a fresh procfs looks like it should close
the hole and does not. A fresh `/proc` changes *which* processes are visible; it does not change
*whose* environment PID 1 has.

With `--unshare-pid` — which prior enables by default — PID 1 inside the namespace **is** the bwrap
parent:

```text
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

|                    | fresh `/proc` | `--unshare-pid` | env filter effective?           |
| ------------------ | ------------- | --------------- | ------------------------------- |
| no `/proc` mount   | —             | irrelevant      | **yes** (nothing to read)       |
| fresh `/proc`      | yes           | yes             | **no** (PID 1 = bwrap)          |
| fresh `/proc`      | yes           | no              | **no** (host processes visible) |
| host `/proc` bound | no            | yes             | **no** (worst: host + bwrap)    |

What a fresh `/proc` *does* buy is real: it hides the host's process list and the other host
processes' environments. Without it, a sandbox can enumerate the host's processes. That is a genuine
isolation win, and it is why prior should keep `--proc` — but it does not touch Finding 1.

### Scope

The bypass requires `--proc /proc` to be present. prior only emits that when the operator's config
binds `/proc` (translated at `src/tools.rs:1797`). So:

- **Default config (no bindings)** — no `/proc`, no bypass. The denylist is the only thing standing
  between the model and the host env, and it is leaky (Finding 2).
- **Config with a `/proc` binding** — the env filter is *entirely* inert. The `OnlyAllow` mode would
  be equally inert.

There is a second, related path: `/proc/1/cmdline` exposes bwrap's full argv, which includes every
`--setenv NAME VALUE` pair. That is a leak of the *allowed* values (not the dropped ones), and it
also fingerprints the host (`--ro-bind` source paths, `--bind` of `~/.cargo`, etc.). It is the same
root cause — `/proc` is not sandboxed against the bwrap parent.

### Why it matters for the allowlist plan

An allowlist does not fix this. `OnlyAllow` with a tight pattern list would still leave
`/proc/1/environ` exposing the host environment wholesale, because bwrap never clears *its own*
environ — it only clears the child's.

The good news is that the fix does **not** require removing procfs. If bwrap is spawned with a
scrubbed environment in the first place, the secrets never reach it and there is nothing for
`/proc/1/environ` to leak (see R1, which shows the confirming experiment). That keeps procfs
available for the Rust toolchain, which needs `/proc/self/exe`.

Note also that the "default" claim in `doc/sandbox.md:310-311` — "the sandbox already blocks network
egress by default, so a leaked key cannot be exfiltrated unless `network: true`" — holds only for
TCP egress. A leaked key can still be written to a file, a git commit, or exfiltrated over the
abstract Unix sockets that survive `--unshare-net` (see Finding 3).

---

## Finding 2 — the denylist leaks the whole desktop environment

`DEFAULT_ENV_DISALLOW` matches only `*_KEY`, `*_SECRET`, `*_TOKEN`, `AWS_*` and `SSH_AUTH_SOCK`. On
the author's host, prior's process environment held
**66 variables; the policy dropped 4 and let 62 through** — and the 62 were entirely the *session*
variables, which no pattern targets:

```text
$ comm -23 <(parent env names) <(names passed via --setenv)
DEEPSEEK_API_KEY
OPENROUTER_API_KEY
SSH_AUTH_SOCK
XIAOMI_MIMO_API_KEY
```

Those four are the only ones the denylist caught. Everything below survived into the sandbox.
Grouped:

- **X11 / GUI:** `DISPLAY`, `XAUTHORITY`, `XDG_SESSION_TYPE`, `XDG_SESSION_ID`, `XDG_SESSION_CLASS`,
  `XDG_SEAT`, `XDG_SEAT_PATH`, `XDG_SESSION_PATH`, `XDG_CURRENT_DESKTOP`, `XDG_SESSION_DESKTOP`,
  `XDG_VTNR`, `DESKTOP_SESSION`, `DESKTOP_STARTUP_ID`, `GDMSESSION`, `GTK_MODULES`,
  `GTK_USE_PORTAL`, `QT_ACCESSIBILITY`, `QT_QPA_PLATFORMTHEME`.
- **IPC / agent sockets:** `DBUS_SESSION_BUS_ADDRESS` (`unix:path=/run/user/1000/bus`), `I3SOCK`
  (`/run/user/1000/i3/ipc-socket.2412`), `WEZTERM_UNIX_SOCKET`
  (`/run/user/1000/wezterm/gui-sock-4114215`), `GPG_AGENT_INFO`
  (`/run/user/1000/gnupg/S.gpg-agent:0:1`), `SSH_AGENT_PID` (2490), `KDEWM`, `WEZTERM_EXECUTABLE`.
- **Host identity / path disclosure:** `HOME`, `USER`, `LOGNAME`, `SHELL`, `PWD`, `OLDPWD`,
  `DESKTOP_STARTUP_ID` (contains the full WM command line), `WEZTERM_CONFIG_FILE`
  (`/home/gabriel/.wezterm.lua`), `NVM_DIR`, `XDG_DATA_DIRS`, `XDG_GREETER_DATA_DIR`, `DEBEMAIL`,
  `DEBFULLNAME`.
- **Third-party endpoints:** `XIAOMI_MIMO_API_BASE` (a non-`*_KEY` API base URL — the matching
  `XIAOMI_MIMO_API_KEY` is dropped, the base URL is not), `NVM_NODEJS_ORG_MIRROR` and friends.

### Secret-shaped names the denylist misses

These are common in the wild and are **not** matched by any of the five default patterns:

```text
PASSWORD, DOCKER_PASSWORD, PGPASSWORD, NETRC, KUBECONFIG,
GOOGLE_APPLICATION_CREDENTIALS, CLOUDSDK_CORE_PROJECT,
SSL_CERT_FILE, NPM_CONFIG_USERCONFIG,
GIT_ASKPASS, SSH_ASKPASS, HOSTALIASES, LD_PRELOAD
```

`SSH_ASKPASS` is the notable one: it names a program to run, so it is a *code-execution* vector
rather than merely a disclosure. `LD_PRELOAD` is worse in principle, though the dynamic loader is
mostly a no-op for setuid binaries and the sandbox is not setuid.

`SSH_AUTH_SOCK` is dropped by name, but `SSH_AGENT_PID` is not — and the pid is enough to find the
agent's socket and talk to it (see Finding 3's socket analysis: a *filesystem* socket survives
`--unshare-net`).

### The gaps are structural, not a matter of adding patterns

A denylist is unbounded: every new tool invents a new secret name. The denylist is also the wrong
shape for the question being asked. The right question is not "which secrets should we hide?" but
"which variables does a sandboxed build need?" — a small, stable set. That is an allowlist.

Prior already has the `OnlyAllow` mode implemented and tested (`src/config.rs:1074`,
`src/tools.rs:2877`). Switching the default is a one-line change to `SandboxConfig::env_policy`
(`src/config.rs:1158`).

---

## Finding 3 — X11 is reachable from inside the sandbox

### Mechanism

The X11 client library resolves `DISPLAY=:0` to a Unix socket at `/tmp/.X11-unix/X0`. Modern X
servers bind **both** a filesystem socket and an **abstract** socket (`@/tmp/.X11-unix/X0`). The
author confirmed both exist:

```text
$ grep X11 /proc/net/unix | awk '{print $NF}' | sort -u
/tmp/.X11-unix/X0
@/tmp/.X11-unix/X0
```

Abstract sockets live in a **network-namespace-scoped** namespace. That gives a precise rule:

- **`--unshare-net` (prior's default, `network: false`)** → the abstract socket is unreachable → X11
  is blocked. CONFIRMED.
- **no `--unshare-net` (`network: true`)** → the abstract socket is reachable → X11 works.
  CONFIRMED.

### Evidence — X11 works with `network: true`

```text
--- A: with --unshare-net (prior default when network=false)
Error: Can't open display: (null)
Failed creating new xdo instance

--- B: without --unshare-net (prior when network=true)
1257
4194367
4194306
```

And the transport is confirmed to be the abstract socket, not the filesystem one:

```text
filesystem: [Errno 2] No such file or directory
abstract:   CONNECTED
```

Note that the *filesystem* socket is separately dangerous in the opposite direction: it is not
subject to the network namespace at all. A test with a plain filesystem socket confirmed it connects
even under `--unshare-net`:

```text
FILESYSTEM UNIX SOCKET CONNECTED (with --unshare-net)
```

So `--unshare-net` blocks the abstract socket but **not** filesystem Unix sockets. If an operator
binds a directory that contains a socket, that socket is reachable regardless of the network
setting.

### What is actually exposed

Not merely a display number — full control of the user's session:

```text
windows:            1257 4194367 4194306 ...
active window title: Security Disclosure | Hacker News — Mozilla Firefox
clipboard read:      <clipboard contents>
XTEST extension:     present
```

The author's own X server advertises the `XTEST` extension, which permits
**synthetic keyboard and mouse input** — i.e. arbitrary input injection into every application the
user has open. A screenshot of the root window was also captured successfully from inside the
sandbox (`import -window root`, 649 KB PNG), and the clipboard was written and read back with the
sentinel value intact.

### Why the current env policy makes this worse

`DISPLAY` and `XAUTHORITY` both survive the default denylist, so the sandboxed command is *handed*
the display to connect to. An allowlist that drops `DISPLAY` removes the hint but does not remove
the capability: a determined command can guess `:0`, and the abstract socket is at a fixed path.

### The `network: true` interaction

`network: true` is the operator's explicit "let this command reach the network" switch. It is
reasonable to read that as *TCP* egress. It is not reasonable to read it as "the command may drive
my mouse and read my clipboard". X11 is a **local IPC** channel, not network egress, so it must not
be inherited from the network knob.

The 2026-10-21 conclusion here was "govern X11 by a separate control, layered on top of a shared
netns". The 2026-10-22 revision draws the line one step earlier: **the netns is that control**, so
the right fix is to stop tying it to the network knob at all, and make `network: true` a bridge
instead. See "Why `network: true` must not share the network namespace". That is the smallest
change that satisfies this section's own argument, and it needs no LSM.

---

## Cross-check: Anthropic's sandbox-runtime

`srt` (<https://github.com/anthropics/sandbox-runtime>, "Beta Research Preview", TypeScript) is the
closest comparable implementation: it uses `sandbox-exec` on macOS and `bubblewrap` on Linux, with
proxy-based network filtering. It is a useful reference for two of the three findings; for the third
it is the model for the *policy*, and a demonstration of the mechanism prior should **not** copy
(bound socket path rather than a passed fd).

### Their network architecture is the inverse of slirp4netns

Their Linux design (`src/sandbox/linux-sandbox-utils.ts`, "Initialize the Linux network bridge") is:

```text
host:    proxy (HTTP :3128 / SOCKS :1080)  <-- socat UNIX-LISTEN --> unix socket
sandbox: --unshare-net, unix socket bound in, socat TCP6-LISTEN:3128 -> UNIX-CONNECT
         HTTP_PROXY=http://localhost:3128
```

No `slirp4netns`, no `/dev/net/tun`, no IPv6 caveat. Their own comment states the tradeoff plainly:

> "Linux's `--unshare-net` provides only all-or-nothing network isolation. Domain filtering happens
> at the host proxy level, not the sandbox boundary."

Consequences for prior:

- **This is the shape R3 adopts.** `--unshare-net` unconditionally, egress through a bridge. What
  prior would do differently is the *mechanism* of the bridge: not `socat` (an extra runtime
  dependency, and one that has to exist inside the sandbox too) and not a bound socket path (which
  re-opens the binding channel R4 is about), but a **listening fd passed out of the namespace** —
  see "Cross-check: `nono`'s passed-listener bridge".
- **Two of their limitations are worth inheriting as decisions rather than bugs.** The bridge only
  carries protocols that honour a proxy variable (`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`); `curl
  --resolve`, raw sockets and CLIs that ignore proxy env do not go through it. That is a *feature*
  under deny-default — those tools simply have no egress — but it must be documented, because it is
  a behavioural change from today's `network: true`.
- **Their SOCKS5 filtering is the policy model to copy**, even though it is TypeScript. See
  "Their SOCKS5 is the policy engine prior needs".

### They block Unix sockets with seccomp — a good answer to Finding 3

Their `allowUnixSockets` config option is documented as
*"macOS only: Unix socket paths to allow. Ignored on Linux (seccomp cannot filter by path)"*. On
Linux they instead use a **seccomp BPF filter** that denies `socket(AF_UNIX, ...)` entirely
(`vendor/seccomp-src/seccomp-unix-block.c`).

I built their filter and confirmed it blocks X11 completely, **even with `network: true`** (i.e.
without `--unshare-net`):

```text
# with their seccomp filter, no --unshare-net:
xdotool search --name ""  →  Error: Can't open display
abstract connect          →  blocked: [Errno 1] Operation not permitted

# control, same sandbox without the filter:
xdotool search --name ""  →  1257 4194367      (X11 works)
```

and confirmed it does not break TCP:

```text
TCP OK - filter does not block inet sockets
```

The reason it works: connecting to a Unix socket requires first calling `socket(AF_UNIX, …)`, so
denying that syscall denies *all* Unix socket use — including the abstract X11 socket. This is a
**barrier**, not a hint, which is strictly better than removing `DISPLAY`.

It is, however, **all-or-nothing**: it cannot permit a specific socket, so it also breaks legitimate
Unix-socket use inside the sandbox. Landlock's `ResolveUnix` (ABI 9+) does not have that
limitation — see "Cross-check: Landlock, and the Rust alternatives" below. Neither is R3's primary
control after the 2026-10-22 revision: the netns stays on, so the abstract socket is unreachable
before either is consulted. seccomp remains as the last layer for `AF_VSOCK` and `io_uring`, which
no namespace and no path rule fences.

Two details in their filter are worth adopting directly:

- They deny `io_uring_setup/enter/register`, because `IORING_OP_SOCKET` (Linux 5.19+) creates
  sockets in kernel context and would bypass the `socket()` rule.
- They deny `AF_VSOCK`, because the network namespace does not fence VM sockets — on a host with a
  vsock transport, one connects straight out of the sandbox, past the proxies.

They also ship a second filter, `namespaces`, denying `unshare`/`clone` with
`CLONE_NEWUSER|CLONE_NEWNS`, plus `setns`, `mount`, `pivot_root`, `open_tree`, `move_mount`,
`fsopen`, `fsconfig`, `fsmount`, `fspick` and `mount_setattr`. That prevents a sandboxed process
from making a new user namespace and escaping the netns it was given.

### Their env model is different, and Finding 1 applies to them too

They **never emit `--clearenv`** (0 occurrences in `linux-sandbox-utils.ts`). They inherit the full
parent environment and use `--unsetenv` for specific denied names, plus `--setenv` for masked
credentials. So:

- They do **not** have Finding 2 (no leaky denylist) — their env handling is deny-by-name, but for
  explicit configured names rather than a glob default.
- They **do** have Finding 1, in a worse form: they mount `--proc /proc` and run with
  `--unshare-pid`, so `/proc/1/environ` exposes the host environment to the sandboxed command. Their
  `namespaces` seccomp filter does not help — reading `/proc/<pid>/environ` is a plain file read,
  not a namespace operation.

This is a useful cross-check: Finding 1 is not an exotic edge case, it is what a production-quality
reference implementation does today. It is also not something they appear to have addressed, which
suggests it is worth prior addressing explicitly rather than assuming the reference has it covered.

Their credential *masking* design is also worth noting as a third option for the env problem:
instead of dropping a secret, replace it with a sentinel and have the proxy substitute the real
value back on egress to permitted hosts. That is more machinery than prior needs today, but it is
the design that supports "the command can use the credential, but only for this domain".

### Their SOCKS5 is the policy engine prior needs

`src/sandbox/socks-proxy.ts` (480 lines) is the closest thing to a specification for what prior's
egress policy should decide, and it is worth reading for the *ordering* rather than the code:

- **Auth first, then the allowlist, then dial.** The per-command SOCKS username carries an
  attribution suffix (`srt.<encodedCommand>`), so a denial can be logged against the invocation that
  caused it. prior needs the *attribution*, not the token: one sandbox per tool call is already an
  identity. Adopt the token only if several sandboxes ever share one bridge.
- **`refuseOpaqueTunnels`** — a switch to refuse every `CONNECT` at the handshake. That is how they
  express "this profile gets HTTP-proxied traffic only, no opaque TCP".
- **Direct dials go through `lookupFor`, and a refused resolution is answered "not allowed by
  ruleset"** (`resolved-address-guard.ts`) — i.e. DNS is resolved proxy-side, in the supervisor, so
  the sandbox never learns an address it cannot reach anyway.
- **`allowUnixSockets` is documented as macOS-only** ("Ignored on Linux (seccomp cannot filter by
  path)"). On macOS Seatbelt *can* filter by path, so the same config field means different things
  per platform. prior's config should not inherit that ambiguity — see R3's config note.

They do **not** filter `CONNECT` by port, and their HTTP proxy is 1439 lines plus 1514 for TLS MITM
because they support credential masking (certificate generation, per-host CA, body substitution,
AWS SigV4 re-signing). **prior needs none of that**: SOCKS5 `CONNECT` is opaque, so filtering on
`host:port` and splicing bytes is the whole job. That is the difference between ~300 lines and
~3000, and it is why R3 chooses SOCKS5-only.

### Two differences worth adopting regardless

- **They always bind `--dev /dev`.** prior currently mounts neither `/dev` nor `/proc` by default,
  which my tests showed breaks `git` outright
  (`fatal: could not open '/dev/null' for reading and writing`). Mounting `/dev` is the more usable
  default and costs nothing.
- **They always mount a fresh `/proc`**, for the reason given in Finding 1: without it the sandbox
  can read the host's `/proc` and learn what is running outside. prior should do the same, accepting
  that it makes R1 mandatory.

---

## Cross-check: `nono`'s passed-listener bridge

`nono`'s NEP-0004 (`~/src/nono/neps/0004-opt-in-linux-namespace-isolation.md`) specifies the Linux
side of the same contract `srt` implements, and specifies it **better**. Their words:

> **Proxy bridge (Phase 1, ProxyOnly only).** A socket stays in the network namespace it was created
> in, whichever process later holds its file descriptor. The bridge uses that property, so nothing
> runs inside the namespace except the workload and no new filesystem endpoint exists:
>
> 1. In the bootstrap child, after `unshare` and bringing up `lo`, the child creates a TCP socket,
>    binds it to `127.0.0.1:<port>` *inside* the new netns … and calls `listen`.
> 2. The child sends the listening fd to the supervisor over the existing bootstrap socketpair
>    (`SCM_RIGHTS`), then closes its own copy before Landlock is applied and before `exec`. The
>    workload never holds the listener.
> 3. The supervisor, outside the namespace, `accept()`s on the passed fd. Each accepted connection
>    belongs to the child's netns and is handed to `nono-proxy`'s existing connection handler
>    in-process, exactly as a connection from the host TCP listener would.

And their own comparison against the alternative:

> Compared with an in-namespace relay forwarding to a proxy Unix socket, this design has no relay
> process, no relay sandbox, no Unix-socket listener in `nono-proxy`, no supervisor-owned socket
> directory, and no socket grant in the child's capability set. The bridge's only state is a
> listening fd owned by the supervisor.

That last point is the reason R3 adopts this mechanism over `srt`'s. `srt` binds a socket *path*
into the sandbox (`linux-sandbox-utils.ts:1519`, `claude-http-<id>.sock`), which is exactly the
channel R4 warns about: a filesystem socket survives `--unshare-net`, and a directory binding can
re-expose it. A passed fd has **no path**, so there is nothing to bind, nothing to re-expose, and
nothing to grant in the capability set. The class of bug disappears rather than being documented.

**Verified on this host.** Built the NEP-0004 sequence by hand (Python `send_fds`/`recv_fds` as a
stand-in for the supervisor's `SCM_RIGHTS`), with bwrap carrying `--unshare-net`:

```text
host: received listening fd [4] msg b'L'
shim: listener bound in-ns: ('127.0.0.1', 3128)
HTTP: HTTP/1.1 200 OK
proxy log: ['ALLOW example.com:80']
```

So a TCP listener created inside the netns, accepted on from outside, carries real traffic. The
host's own proxy never has a socket in the sandbox's filesystem.

### Two implementation facts that shrink the work

- **`duct` can pass fds.** It has no `pass_fds` argument, but `Expression::before_spawn` hands you a
  `&mut std::process::Command` (`duct-1.1.2/src/lib.rs:913`), which does. Verified with a
  standalone crate: the child wrote to an inherited fd and the parent read it —
  `host got via fd: "via-duct"`. So `build_bwrap_expr` (`src/tools.rs:1889`) can keep using `duct`;
  this does not force a rewrite onto raw `Command`.
- **`lo` is already up inside bwrap's netns.** `ip link show lo` inside `--unshare-net` reports
  `lo: <LOOPBACK,UP,LOWER_UP>`. `zerobox` carries ~60 lines to bring it up by hand
  (`linux-sandbox/src/proxy_routing.rs:560-621`, a raw `SIOCGIFFLAGS`/`SIOCSIFADDR` dance plus an
  `EADDRNOTAVAIL` retry path in `bind_local_loopback_listener`). **prior does not need any of it** —
  bind `127.0.0.1:<port>` directly. (Verified: binding `127.0.0.1:0` inside the netns succeeds.)

### What `zerobox` is worth

`zerobox`'s `proxy_routing.rs` (832 lines) is a working socat-free bridge of the same family and is
useful as a second opinion on the *sequencing*: host bridge spawn → readiness pipe → local bridge →
env rewrite → cleanup worker that reaps stale socket dirs by owner pid. Read it for `fork` +
`PIPE_CLOEXEC` readiness handshakes and `PR_SET_PDEATHSIG` hardening
(`harden_bridge_process`). But it bridges through socket paths in a directory
(`proxy-route-<n>.sock`), so it inherits the R4 class that NEP-0004 avoids, and ~half of its 832
lines is the socket-dir lifecycle prior would not need.

Its `network-proxy` crate (9.4k lines) and `nono-proxy` (22.6k lines) are **not** viable
dependencies for prior: both drag in a full HTTP proxy, TLS MITM, credential/OAuth machinery, and
`zerobox-network-proxy` pins `rama = "=0.3.0-alpha.4"` across six exact-pinned alpha crates. prior
already has `tokio` and `libc`, which is all the bridge needs.

---

## Why `network: true` must not share the network namespace

This is the decision R3 is built on, so it is argued here rather than buried in the recommendation.

Today `network: true` means "omit `--unshare-net`", which is what makes Finding 3 reachable. The
alternative is to keep `--unshare-net` always and define `network: true` as *egress through the
bridge*. Both were measured on this host, same sandbox, same client, only the netns differing:

```text
== unshare-net (proposed)
x11_abstract: BLOCKED ([Errno 111] Connection refused)
direct_tcp:   BLOCKED ([Errno 101] Network is unreachable)
proxy_ok:     HTTP/1.1 200 OK
proxy_denied: rep code 2

== shared netns (today's network: true)
x11_abstract: CONNECTED          <-- finding 3
direct_tcp:   CONNECTED          <-- unfiltered egress
proxy_ok:     HTTP/1.1 200 OK
proxy_denied: rep code 2
```

The proposed side keeps everything `network: true` is *for* and drops everything it is not. Denials
are attributable at the bridge (`ALLOW example.com:80`, `DENY-HOST evil.invalid`,
`DENY-PORT example.com:22`), which today they are not at all.

Three objections, and their answers:

- *"A filter is weaker than a namespace."* Agreed, and that is why the netns stays: reachability is
  still removed by the namespace, and the bridge is the only way out. nono's own principle, adopted
  as prior's: *"Mediation inspects a hostile interface point-by-point; a namespace removes the
  interface wholesale."*
- *"Tools that ignore proxy variables get no egress."* Correct, and intended. It is also true of
  `network: false` today, so it is a narrowing, not a new failure mode.
- *"It is more code."* Yes — ~300 lines and a supervisor change, against three layered controls
  (Landlock ABI probe, `ResolveUnix` default-grant curation, seccomp BPF) that would each need
  their own fallback ladder and would not transfer to macOS. It is fewer moving parts to *own*.

The consequence worth stating plainly: **prior's `network` knob becomes a policy, not a boolean.**
See R3's config note for the schema.

---

## Cross-check: Landlock, and the Rust alternatives

The survey at `doc/261021_sandbox_survey.md` covers the landscape comprehensively; this section
records what I verified by hand against the `nono` checkout at `~/src/nono`. It changed R3 once
(the 2026-10-21 revision made Landlock primary), and the 2026-10-22 revision changed it back: the
netns keeps the abstract-socket job, and Landlock moves to R4, where it is the only fix for
*specific* sockets rather than a substitute for reachability removal.

### Landlock can close the X11 vector, if the netns is not doing it

Landlock is an LSM, not a namespace tool: an unprivileged process irreversibly restricts its own
ambient rights, and the restrictions are inherited by children. It needs no user namespace, and it
has two relevant properties that bwrap does not:

- **ABI 6 (Linux 6.12): `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`** — restricts a sandboxed process from
  connecting to an abstract UNIX socket created outside its Landlock domain. That is *exactly* the
  X11 vector, since the X11 server's socket lives in the host's abstract namespace.
- **ABI 9 (Linux 7.1): `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`** — kernel-enforced per-path control of
  *pathname* UNIX socket connections.

Both were verified on this host (kernel 7.1.13, Landlock ABI 9) with the `landlock` 0.4.7 crate:

```text
# abstract socket (the X11 vector)
before landlock:: CONNECTED (not blocked)
after  landlock:: BLOCKED (Operation not permitted (os error 1))

# TCP is unaffected
before landlock TCP:: TCP CONNECTED (allowed)
after  landlock TCP:: TCP CONNECTED (allowed)
```

And for pathname sockets, with `ResolveUnix` granted only under one directory:

```text
granted dir socket:   connect(/tmp/ll_ok/s.sock)   CONNECTED (allowed)
non-granted dir socket: connect(/tmp/ll_no/s.sock) BLOCKED (Permission denied)
```

This is a **better** answer than a seccomp filter, for three reasons:

1. It is **per-path**, not all-or-nothing. seccomp can only deny `socket(AF_UNIX)` wholesale, which
   also breaks any legitimate Unix-socket plumbing (`agent-browser`'s control socket, the egress
   bridge's own listener). `ResolveUnix` allows exactly the sockets that are named and denies the
   rest.
2. It **does not block `socket()`**, so tools that create a Unix socket for inter-process
   coordination keep working.
3. It is **not gated on the network setting** — the isolation comes from the LSM, not a netns.

But it is **not the primary X11 control**, and that is the correction the 2026-10-22 revision makes.
Points 1–3 are all arguments against *seccomp*, and seccomp is now the third layer. Against the
netns, Landlock loses on every axis that matters here: it needs an ABI probe and a fallback ladder;
`ResolveUnix` is ABI 9+ (Linux 7.1+), which almost no host has; granting it without curated paths
flips existing profiles into deny-all; and it is a hundred lines of policy that has no macOS
analogue. The netns does the same job on every kernel, for free, and `nono`'s own reasoning says why
— *"a namespace removes the interface wholesale"*.

Landlock earns its place at R4 instead, on the question the netns cannot answer: **which specific
sockets should remain reachable.**

### The important caveat: Landlock does not cover everything

Landlock's filesystem rules do **not** govern pathname AF_UNIX `connect()` on kernels below ABI 9.
Verified with ABI V4 rules applied and a real pathname socket:

```text
before landlock:: connect(/tmp/ll_path_test.sock) CONNECTED (not blocked)
after  landlock:: connect(/tmp/ll_path_test.sock) CONNECTED (not blocked)
```

This is not a gap in my test — it is a documented escape class. `nono`'s NEP-0004
(`neps/0004-opt-in-linux-namespace-isolation.md`) states it plainly:

> **Pathname AF_UNIX `connect()`** is not governed by Landlock filesystem rules. From inside a
> sandbox, `/run/user/<uid>/bus` reaches the D-Bus session bus or the systemd user manager socket
> (`systemd-run` starts processes *outside* the sandbox), `docker.sock`, X11/Wayland, and
> `ssh-agent`.

So the D-Bus socket, `docker.sock`, `ssh-agent` and the systemd user manager are reachable by
pathname regardless of Landlock's filesystem rules, unless the kernel is new enough for
`ResolveUnix` (7.1+) — and even then only for paths that were not granted.

nono's own answer is instructive: its `af_unix_mediation` countermeasure (seccomp user-notify
supervision) is **off by default and "excluded from `nono wrap`"**, and they explain why mediation
is not a sound boundary at all:

> intercept-and-continue mediation of pointer-argument syscalls is not a sound security boundary:
> the kernel documentation for seccomp user notification is explicit that the notified memory
> remains writable by the target, so the validated `sockaddr` bytes are subject to rewrite between
> check and use. Mediation inspects a hostile interface point-by-point; a namespace removes the
> interface wholesale.

That reasoning applies to prior too, and it is the load-bearing sentence for R3's revision:
**reachability removal beats mediation.** The netns (`--unshare-net`) is the right primary control
for the *abstract* socket. Landlock `ResolveUnix` is the right control for *pathname* sockets that
a binding legitimately exposes (R4). seccomp is a complement, not the primary defence.

### What `nono` does and does not settle

`nono` (`nolabs-ai/nono`, Apache-2.0) is the most complete Rust option and the closest analogue to
`srt`. Relevant to prior:

- It caps its **enforced** Landlock ABI at **V6** (`ABI_PROBE_ORDER = [V6, V5, V4, V3, V2, V1]`,
  `crates/nono/src/sandbox/linux.rs:426`), while the `landlock` crate it ships (0.4.7) exposes V9.
  So nono *can* scope abstract sockets but *cannot* yet use `ResolveUnix`. NEP-0004 tracks the
  catch-up as its own work item.
- It denies abstract-namespace sockets explicitly: "Abstract-namespace sockets
  (`sun_path[0] == '\0'`) and unnamed sockets are never covered by a grant […] Those kinds are
  denied by the sandbox's `decide_network_notification` policy on Linux"
  (`crates/nono/src/capability.rs:340`).
- Its namespace isolation is **opt-in and default-off** (`linux.isolate` / `--isolate`),
  deliberately, so that enforcement does not vary host to host. prior's bwrap namespaces are
  unconditional, which is the opposite trade.
- Its Landlock path is **immune to the Ubuntu 24.04 AppArmor userns restriction** that breaks bwrap
  — a real deployment advantage for a tool installed on arbitrary laptops.

nono does **not** address Finding 1. Its `scrub.rs` is output hygiene (redacting secrets from audit
records), not environment isolation, and it does not scrub the *parent's* environment the way R1
requires.

### Recommendation change (superseded)

The 2026-10-21 revision made **Landlock the primary X11 control, with seccomp and the netns as
layered fallbacks**, ordered `unshare-net` → Landlock → seccomp. **That ordering is superseded.** It
was correct about seccomp (per-path beats all-or-nothing) and wrong about what the netns is for: if
the netns stays on unconditionally, the abstract socket is unreachable on every kernel and neither
Landlock nor seccomp is needed for Finding 3.

The current ordering is in R3 and R4:

1. **`--unshare-net` unconditionally** — abstract sockets unreachable; `network: true` becomes a
   bridge, not a shared netns (R3a).
2. **seccomp** for `AF_VSOCK` and `io_uring` only — the two classes no namespace and no path rule
   fences (R3d).
3. **Landlock `ResolveUnix`** for *pathname* sockets exposed by a binding — the R4 question, not the
   R3 one.

prior should still not adopt nono as a dependency: it is a large surface (a supervisor process, a
proxy, a trust store, an audit log) that duplicates policy prior already has. But it is an excellent
**reference**, and NEP-0004's passed-listener bridge is the mechanism R3 adopts.

---

## Additional observations (not separately requested, found during the audit)

- **`/dev` and `/proc` used to be absent from the sandbox.** With the default config (no bindings)
  prior mounted neither, so `echo hi > /dev/null` failed with `Directory nonexistent`. Only a config
  that bound them got them, which left the default namespace unable to run `git` or `cargo`.
  **Fixed in R3e**: both are mounted fresh by default.
- **`/etc` used to be ro-bound whole** when no `/etc` binding was configured, exposing `/etc/hosts`,
  `/etc/ssl`, `/etc/localtime` and `/etc/passwd` — mild host fingerprinting, no secrets.
  **Fixed in R3e**: a named working set (`ESSENTIAL_ETC`) replaces it.
- **The UTS namespace is not unshared**, so `hostname` inside the sandbox returns the host hostname
  (`pad`). Note that `--unshare-uts` alone does not change this — a new UTS namespace still inherits
  the hostname until `--hostname` is given: `--unshare-uts --hostname sandbox` reports `sandbox`.
  Closing the leak needs both flags. **Fixed in R3e.**
- **The IPC namespace is not unshared**, so the sandbox sees the host's System V shared-memory
  segments (`ipcs -m` lists 8 of them). `--unshare-ipc` closes this; it is also the namespace that
  would matter for X11's MIT-SHM path, which is a faster and more direct way to read framebuffers
  than `XGetImage`. Confirmed the flag is free to add: `network: true` does not depend on it.
  **Fixed in R3e.**
- **`--cap-drop ALL` is a no-op for an unprivileged user.** Measured with and without the flag,
  `CapEff`/`CapBnd` are `0000000000000000` either way — the user never held capabilities to begin
  with, so there is nothing to drop. The flag is not harmful, but it is not contributing anything.
- **The sandbox does run in a new user namespace** (the `user` ns inode differs from the host's),
  but the uid is **not** remapped to root: `/proc/self/uid_map` inside the sandbox reads
  `1000 1000 1`, i.e. the host uid maps to itself. So the sandboxed command is still uid 1000 with
  the host user's real privileges on anything that *is* bound. "Hide the host home directory" is
  achieved by not binding it, not by uid isolation.
- **The `ipc`, `uts` and `cgroup` namespaces are shared with the host** (the ns inodes are identical
  inside and outside), so `--unshare-ipc` and `--unshare-uts` are both currently missing.
- **Loopback is already up inside a fresh netns.** `ip link show lo` inside `--unshare-net` reports
  `lo: <LOOPBACK,UP,LOWER_UP>`, and binding `127.0.0.1:0` from inside succeeds. Two consequences for
  R3: the egress bridge can bind directly (no `ioctl` dance), and a sandboxed command *can* run its
  own local TCP servers — which is what `agent-browser` and any test suite that listens on
  `127.0.0.1` needs. This is the same conclusion nono reached separately, and it is why they made
  namespace-private loopback an explicit opt-in rather than something to be fought.
- **A single socket *file* can be ro-bound into the sandbox** at an arbitrary path
  (`--ro-bind /host/x.sock /run/prior-egress.sock`), and connecting to it works. This is why "bind a
  directory" is not required for a socket channel — and also why R4's warning cannot be satisfied by
  "don't bind directories": the operator is one `--ro-bind` away from exposing any socket, and prior
  offers `ro`/`rw` *path* bindings, which accept files.

---

## Recommendations

Ordered by priority. **R1, R2, and all of R3 except R3d are implemented** (R3a/b/c/e/f landed
2026-10-22, in that order). **R3d, and R4/R5/R6, are not.** R3 was revised on 2026-10-22 (bridge
design, not a Landlock ladder); R4 absorbed the Landlock work that R3 gave up.

### R1 — Scrub bwrap's *own* environment before it spawns the payload (prerequisite for R2/R3)

**Implemented** — see "Implementation notes" above.

The env filter cannot be trusted while `--proc /proc` is present, because the bwrap parent's environ
still holds the host secrets. The fix is not to remove procfs — it is to make sure **bwrap itself**
never inherits the host environment in the first place.

If prior launches bwrap with a scrubbed environment (only the vars bwrap itself needs — in practice
just `PATH`), then bwrap's `/proc/1/environ` contains only that scrubbed set, and the child inherits
nothing extra. The bypass disappears entirely. CONFIRMED:

```text
$ ANTHROPIC_API_KEY=sk-ant-SECRET SSH_AUTH_SOCK=... DBUS_SESSION_BUS_ADDRESS=... \
  env -i PATH="$PATH" HOME="$HOME" \
  bwrap ... --proc /proc --clearenv --setenv PATH "$PATH" -- sh -c '...'

--- via /proc/1/environ (bwrap parent) ---
  NONE - clean
--- via /proc/1/cmdline ---
  (no secrets)
```

Note the mechanism: the secrets never reach bwrap, so there is nothing for `/proc/1/environ` to
leak. The `--setenv` values the *policy* permits are still visible via `/proc/1/cmdline` — that is
the remaining, much smaller exposure, and it is inherent to bwrap's argument-vector interface.

In Rust this means clearing the environment of the `bwrap` child process before `exec`, rather than
relying on `--clearenv` alone (which only affects the *child* of bwrap, not bwrap itself).

prior spawns bwrap through **duct** (`src/tools.rs:1915`, `src/tools.rs:1939`) and through
`std::process::Command` (`src/tools.rs:1177`). Both need the scrub:

- `duct` already calls `command.env_clear()` and then applies the expression's env map, but that map
  is *seeded from the parent process* by default (`duct-1.1.2/src/lib.rs:1643`). Calling
  `.full_env(...)` replaces the map wholesale, which is exactly the primitive needed. Verified
  against the real library:

  ```text
  default duct     -> MY_SECRET=leaked_value_123
  duct .full_env   -> NONE-CLEAN
  ```

- For the `std::process::Command` path (`sandbox_list_lines`), add `.env_clear()` and set only the
  vars bwrap needs.

bwrap itself needs very little from its environment: `PATH` (to resolve the program), and
`HOME`/`LANG` are harmless to keep. Note that bwrap is invoked as `bwrap` (not an absolute path), so
`PATH` must be present in *bwrap's* own environment for the exec to succeed.

**Do not remove procfs as the primary fix.** procfs is load-bearing: both `rustc` and `cargo` fail
with
`Unable to proceed. Could not locate working directory.: no /proc/self/exe available. Is /proc mounted?`
when it is absent, and `git` additionally needs `/dev/null`. A Rust-oriented coding agent would
break immediately. Keeping procfs and scrubbing bwrap's environ is both safer and more usable.

Two caveats worth recording:

- This makes the *env filter* trustworthy, but `/proc/1/cmdline` still exposes the permitted
  `--setenv` values and the full set of `--ro-bind`/`--bind` source paths. That is host
  fingerprinting, not secret leakage.
- If a binding ever exposes a host **socket** path, that socket is reachable regardless of the env
  policy (see R4).

Recommended: scrub bwrap's environment at spawn time; keep procfs, and mount it by default (see R3's
`--dev`/`--proc` note).

### R2 — Switch the default env policy to an allowlist

**Implemented** — see "Implementation notes" above.

Make `OnlyAllow` the default in `SandboxConfig::env_policy` (`src/config.rs:1158`), with a pattern
list covering what a build actually needs:

```text
PATH, HOME, TERM, LANG, LC_*, USER, LOGNAME, SHELL,
CARGO_HOME, RUSTUP_HOME, RUSTUP_TOOLCHAIN, XDG_CACHE_HOME, XDG_CONFIG_HOME,
XDG_DATA_HOME, XDG_STATE_HOME, TZ, SOURCE_DATE_EPOCH
```

Three deviations from the list as originally proposed, all deliberate:

- **`PWD` dropped.** bwrap's `--clearenv` sets `PWD` to the in-namespace cwd, which is what the
  command should see. A host `PWD` would name a path that need not exist inside the sandbox.
- **`TMPDIR` dropped from the list, and set to `/tmp` by the sandbox.** `/tmp` inside the sandbox is
  a fresh tmpfs; the host's `TMPDIR` names a path that is not mapped in at all, so inheriting it
  would point tools outside the sandbox's scratch area. The sandbox sets it explicitly instead.
- **`GIT_*` dropped, and `CI` dropped.** `GIT_SSH_COMMAND`, `GIT_ASKPASS` and `GIT_CONFIG_*` are
  behavioural hooks that *execute* programs, so they belong on the deny side even though the `GIT_`
  prefix reads as benign. `CI` is left out only because it changes tool behaviour in ways that are
  easy to get wrong (`CI=true` makes many tools non-interactive); add it explicitly if wanted.

Observations that shape the list:

- `GIT_*` deserves a decision rather than a default: `GIT_SSH_COMMAND`, `GIT_ASKPASS` and
  `GIT_CONFIG_*` are *behavioural* hooks that execute programs, so they belong on the deny side by
  default even though `GIT_*` reads naturally as benign. If `GIT_*` is allowed wholesale,
  `GIT_ASKPASS` must be excluded explicitly.
- `LC_*` and `TZ` are needed for reproducible output; `LC_ALL`/`LANG` are already relied on by the
  current tests.
- `TMPDIR` is set by the sandbox rather than inherited — see above.

The deny list does not disappear — it becomes a **second** layer on top of the allowlist (R1 and R2
are complementary). A sensible end state is "allowlist, then remove these specific names from the
allowed set", so that a name that is both needed and dangerous (`SSH_AUTH_SOCK` is not needed;
`GIT_ASKPASS` might be) can be excluded without a second policy mode.

Note that `EnvPolicy` is currently a single mutually-exclusive mode ("you cannot combine a blacklist
and a whitelist in one policy", `src/config.rs:1046-1048`, unchanged). Supporting "allowlist, then
subtract" needs either a third mode or a separate `deny` list. Worth deciding explicitly, since the
config surface is user-facing and documented in `settings.toml.example:109-137` and
`doc/sandbox.md:275-366`.

`srt` offers a third option worth noting for the future: **masking** rather than dropping — replace
a secret with a sentinel and have the egress proxy substitute the real value back for permitted
hosts. That is more machinery than prior needs today, but it is the design that supports "the
command may use the credential, but only for this domain".

#### Implementation notes — R1 and R2 (done)

R1 and R2 are implemented. The two are complementary, and R1 is the prerequisite: without it the
allowlist stays decorative, because bwrap's own `/proc/1/environ` would still hold everything the
allowlist dropped.

**R1 — scrub bwrap's own environment.** `config::bwrap_scrub_env` returns the environment bwrap
itself is spawned with: `PATH`, and nothing else. Both spawn sites use it — the two `duct` paths via
`.full_env(...)` (which replaces duct's inherited environment wholesale) and the
`std::process::Command` path in `sandbox_list_lines` via `.env_clear()` plus explicit vars.

Two things the implementation turned up that the recommendation above did not state:

- **`HOME` is not needed.** The recommendation had bwrap keeping `HOME` so it could resolve `~` in
  bind arguments. It cannot: bwrap does not expand `~` at all
  (`bwrap: Can't find source path ~/fakebin`), and prior expands those itself in `resolve_with_home`
  before building the args. The scrub set is therefore `PATH` alone.
- **bwrap's `PATH` is only needed to find the `bwrap` binary.** The *sandboxed* program is resolved
  against the **child's** `PATH` — bwrap `execvp`s the payload with the environment it just set up.
  So bwrap's own `PATH` and the command's `PATH` are independent, and the policy still governs the
  command's.

The regression test is `bwrap_own_environ_does_not_leak_host_secrets` in `src/tools.rs`. Two details
make it non-vacuous, both found the hard way:

- The default sandbox has **no procfs**, so `/proc/1/environ` does not exist and the test would pass
  trivially. The test binds `/proc` explicitly.
- The default sandbox has **no `/dev`**, so `2>/dev/null` fails with "cannot create /dev/null:
  Directory nonexistent" — and because the redirection is set up before the command runs, the
  *whole* pipeline never executes, so the test sees an empty result and passes. The test reads
  `/proc/1/environ` with no redirection.

With the fix reverted, both tests fail and dump the entire host environment (`XAUTHORITY`,
`DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, ...). With it, they pass. A second test,
`bwrap_own_environ_is_scrubbed_not_merely_filtered`, uses a variable that is *not* a secret, to show
that no value-based policy could have caught this.

**R2 — allowlist by default.** `config::DEFAULT_ENV_ALLOW` is the new default, and
`EnvPolicy::OnlyAllow` is the default mode. The list is the one in the recommendation above, minus
`TMPDIR` (see below) and minus `GIT_*` (kept out deliberately: `GIT_SSH_COMMAND`, `GIT_ASKPASS` and
`GIT_CONFIG_*` are behavioural hooks that *execute* programs).

`TMPDIR` is handled differently than recommended. Rather than inheriting the host's `TMPDIR` and
rewriting it, the sandbox **sets `TMPDIR=/tmp` itself** (`config::DEFAULT_FORCED_ENV`) — `/tmp`
inside the sandbox is a fresh tmpfs, so the host's `TMPDIR` names a path that is not mapped in at
all. `TMPDIR` is therefore not on the allowlist, and the forced value is emitted *after* the
policy's `--setenv` values, so it wins bwrap's "last `--setenv` wins" ordering.

The `EnvPolicy` mode question raised above is still open: "allowlist, then subtract" is not
implemented. What *is* implemented is that an explicit `sandbox.env` block is authoritative in full
— the built-in allowlist and the forced `TMPDIR` are not mixed in behind the operator's back, so the
old denylist behaviour is still available by asking for it explicitly.

### R3 — Make `network` a policy, not a boolean (the egress bridge)

**Not implemented.** Revised 2026-10-22; supersedes the Landlock/seccomp ladder.

The requirement: do not let `network: true` imply "may drive the user's mouse". X11 is **local
IPC**, not network egress, so it must not be inherited from the network knob. The 2026-10-21
revision tried to satisfy that with three layered controls on top of a shared netns. The corrected
design is smaller: **keep `--unshare-net` unconditionally, and redefine `network: true` as egress
through a bridge.** Then the abstract socket is unreachable on every kernel, with no LSM, no ABI
probe, and no fallback ladder — and the same policy shape has a three-line macOS implementation.

The measurements behind this are in "Why `network: true` must not share the network namespace", the
bridge mechanism is in "Cross-check: `nono`'s passed-listener bridge", and the policy model is in
"Cross-check: Anthropic's sandbox-runtime". This section is the specification.

#### R3a — The network contract — IMPLEMENTED 2026-10-22

- `--unshare-net` is emitted **always**. `network` no longer controls the netns.
- `network: true` means: prior starts a bridge, publishes `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`
  pointing at it, and the sandbox's only route out is that bridge.
- `network: false` (default) means no bridge and no proxy variables — unchanged from today.
- The bridge is a **listening fd created inside the netns and accepted from outside**, per
  `nono`'s NEP-0004. No socket path exists in the sandbox filesystem, so there is nothing for a
  binding to re-expose (which is R4's class, handled by construction rather than by a rule).
- No `socat`, no `slirp4netns`, no `/dev/net/tun`. Verified: `lo` is already up inside the netns, so
  the bridge binds `127.0.0.1:<port>` directly.

Consequences to document, because they are operator-visible:

- **Tools that ignore proxy variables get no egress.** `curl --resolve`, raw sockets, and CLIs that
  do not read `ALL_PROXY` fail closed. Intended; they also fail closed today under `network: false`.
- **Host loopback services become unreachable** from the sandbox. Correct under deny-default, but it
  is a change for anyone using `network: true` to reach a local dev server.
- **UDP gets nothing.** SOCKS5 `CONNECT` is TCP-only. DNS still works, because resolution happens in
  the bridge (`socks5h` semantics — the sandbox never learns an address it cannot use).

#### R3b — The bridge mechanism (the part that is subtle) — IMPLEMENTED 2026-10-22

Follow NEP-0004's sequence exactly; the deviations from `zerobox` and `srt` are deliberate and
explained. There is one unavoidable shape change to prior's spawn path, and it is the main risk in
this item: **the supervisor must outlive the child**, because it holds the listener and pumps bytes
for the sandbox's whole lifetime. `build_bwrap_expr` returns a `duct::Expression` that the caller
`.read()`s to completion (`src/tools.rs:1889`, `:1939`); the accepted listener cannot live inside
that expression's lifetime.

Sketch, in terms of prior's existing code:

1. **Create the socketpair and fork the bootstrap child** before building the bwrap argv.
   `bwrap` becomes the bootstrap child's payload rather than `duct`'s direct target, or the child is
   a small pre-exec step. Either way the fd must survive into the netns: `Command::pass_fds` via
   `duct`'s `before_spawn` (`duct-1.1.2/src/lib.rs:913`) — verified working.
2. **In the child, after the netns exists.** The ordering matters: the socket must be created in the
   new namespace, so it has to happen inside bwrap's namespace, not before it. Bind `127.0.0.1:0`,
   `listen`, and send the fd out over the socketpair with `SCM_RIGHTS`; then close the local copy
   and `exec` the workload. The workload never holds a listener.
3. **In the supervisor** (prior), `recvmsg` the fd, wrap it in a `TcpListener`, and run an accept
   loop; for each connection, parse SOCKS5, consult the allowlist, dial, and splice.
4. **Publish the endpoint** to the sandbox as proxy env vars. Because the listener lives in the
   sandbox's netns, the port is whatever was bound in step 2 — return it over the same socketpair
   alongside the fd.

**An ordering constraint that the sketch above hides.** The proxy variables are part of bwrap's argv
(the env policy emits `--setenv`), but the port is not known until the child has bound inside the
netns — which is *after* the argv is built. Two ways out, both used in the references:

- **Fix the port number.** NEP-0004 binds `127.0.0.1:<port>` where `<port>` is *the same number the
  host proxy uses*, precisely so the child environment can be built before the bind. This is the
  simplest fix and probably what prior should do: pick the port, put it in the argv, bind it in the
  child, fail the launch if it is taken.
- **Or pass the env in over the bootstrap socketpair**, which NEP-0004 does for *other* proxy values
  (`activate_proxy_routes_in_netns` rewrites them after `unshare`, before `exec`). More general,
  more moving parts.

Related: `ALL_PROXY=socks5h://127.0.0.1:<port>` is the variable that matters for a SOCKS-only
bridge. `HTTPS_PROXY`/`HTTP_PROXY` pointing at a `socks5h://` URL is accepted by `curl` but not
universally by other clients, so **either set only `ALL_PROXY`, or set all three and document the
expectation**. That choice belongs with R3a's operator-facing consequences.

**Measured (2026-10-22), answering the question above.** All three variables set to
`socks5h://127.0.0.1:<port>` were exercised against a TCP-loopback SOCKS5 relay — i.e. the *macOS*
transport shape from R3g, which is also what the Linux bridge presents to clients:

```text
curl https://example.com/         HTTP 200
curl http://example.com/          HTTP 200   (plain HTTP also works via socks5h)
git ls-remote https://github.com/octocat/Hello-World   -> refs/heads/master
```

So `curl` and `git` both honour `socks5h` for plain HTTP **and** HTTPS; setting all three is the
right default and needs no per-client carve-out for prior's tool set. Two things this does *not*
cover, both expected under the design:

- **A client that speaks only `http://` proxy syntax.** `curl --proxy http://…` uses absolute-form
  `GET` for plain HTTP and `CONNECT` for TLS, and a SOCKS-only bridge answers `405` to the former
  (measured). Clients given `ALL_PROXY=socks5h://…` are unaffected; only a hand-configured
  `http_proxy=http://…` is. Worth knowing if an operator overrides the variables.
- **`curl --proxy socks5h:///path/to/socket`** — SOCKS over a *Unix path* is not supported by
  libcurl (`Could not resolve proxy: tmp`, measured). This is exactly why the bridge must present a
  **loopback TCP** endpoint and not a bound socket path. It is an independent confirmation of the
  R3a design decision, from the client side.

**How it was actually implemented (2026-10-22).** The spike found a simpler shape than the sketch
above, and the code follows it. Read `src/egress.rs` and `src/bin/prior-egress.rs`; this is the map.

- **Where the bind happens: a helper binary, `prior-egress`.** It is bwrap's payload. It binds
  `127.0.0.1:0` inside the netns, sends the listening fd out, sets the proxy variables, and `exec`s
  the real command. Resolved and bound absolutely, exactly like `bsed` (`egress::resolve_shim`), with
  the same `target/<profile>/deps` fallback so the end-to-end test exercises the built binary.
- **No `pass_fds`, and no fd-number guesswork.** `Command::pass_fds` is still unstable, and `duct`
  exposes it only via `before_spawn`. Instead the bootstrap socketpair is created **before** the
  spawn with the child end *not* `CLOEXEC` (parent end is). It survives `exec` into bwrap with the
  **same fd number on both sides** — measured, not assumed — so the shim learns it from
  `PRIOR_EGRESS_FD`, which `build_bwrap_args` emits as an ordinary `--setenv`. This is why the fd
  needs no argv slot and why the argv keeps the same shape with and without egress.
- **The port is chosen in the shim, not passed in.** The ordering constraint the sketch worried
  about ("port unknown when the argv is built") disappears: the shim owns both the bind and the
  `exec`, so it reads the bound port and sets `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY` itself, just
  before `exec`. No fixed port, no second `--setenv` rewrite over the bootstrap socket.
- **`prlimit` ordering:** the shim sits *outside* `memory_wrapped_target`, so the memory cap applies
  to the command the shim execs, not to the fd-passing step.
- **The supervisor outlives the child without restructuring `duct`.** The reader is started first
  (`expr.reader()` — bwrap, and thus the shim, is already running), then `recv_fd` takes the
  listener, and the `EgressBridge` is held for the whole read in `capture_bounded_with_egress`.
  Dropping it stops the accept loop. The `duct` read-to-completion shape is unchanged; the bridge is
  just a value alive alongside it.
- **The accept loop is non-blocking and polled.** The obvious "wake a blocking `accept` by connecting
  to its address" trick **does not work here**: the listener is bound in the *sandbox's* netns, so
  the supervisor — in the host netns — cannot reach that address, and `join` hangs forever. This was
  a real bug during implementation; the loop polls a stop flag instead.
- **The fallback was not needed.** The socket-file design stays recorded above as the
  known-inferior alternative (R4 class: a path a binding could re-expose), but the fd-passing design
  fit the `duct` path cleanly, so it is what shipped.

Tests that pin the mechanism (all in `src/egress.rs` and `src/tools.rs`): `socks5_connect_splices_*`
and `socks5_denies_*` exercise the protocol with no bwrap; `egress_argv_binds_shim_and_publishes_fd`
pins the argv; `egress_bridge_carries_traffic_out_of_the_netns` runs the real shim under
`--unshare-net`, asserts `curl` reaches a host-loopback upstream through the bridge *and* that the
abstract X11 socket is still unreachable in the same launch;
`egress_allowlist_denies_unlisted_destination` proves the allowlist actually gates;
`direct_tcp_egress_is_blocked_even_with_network_enabled` proves a direct socket still fails closed.

Two open design questions (both **answered** by the implementation above, kept for the record):


- **Where does the in-namespace bind happen?** bwrap has no "run this before exec" hook, so either
  (a) a tiny shim as bwrap's payload that binds+sends+execs, which is what NEP-0004 does, or
  (b) a two-phase launch. (a) is simpler and matches the reference; it means prior needs a small
  helper binary or an `sh -c` fragment. Prefer a helper binary for the same reason `bsed` is
  resolved absolutely (`src/tools.rs:767`) — but note it must be bound into the sandbox.
- **Does this interact with `prlimit` (`memory_wrapped_target`, `src/tools.rs:1671`)?** It wraps the
  payload; the bind-and-send shim would sit outside it. Check the ordering.

There is a **fallback worth keeping in the design** even though it is not the primary: if the
two-phase/shim approach proves too invasive for the `duct` path, a single socket file can be
`--ro-bind`-ed in and connected to (`CONFIRMED`: `--ro-bind /host/x.sock /run/prior-egress.sock`
then connect works). That is `zerobox`'s and `srt`'s approach, it is simpler, and its only real
disadvantage is the R4 class — a path exists that a binding could re-expose. Ship the fd-passing
design if it fits; record the socket-file design as the known-inferior alternative so the decision
is not re-litigated after a compaction.

#### R3c — The policy engine (SOCKS5, host+port, ~300 lines) — IMPLEMENTED 2026-10-22

SOCKS5 `CONNECT` only. No HTTP proxy, no TLS MITM, no credential masking — those are 1439 + 1514
lines in `srt` and exist for a feature prior does not have.

Decision order, modelled on `srt`'s `socks-proxy.ts`:

1. Greeting: **no auth**, for now. `srt` uses SOCKS username/password to carry a per-command token
   so a denial can be attributed (`srt.<encodedCommand>`); prior runs one sandbox per tool call, so
   the *tool call* is the attribution unit and the bridge logs it directly. If prior ever
   multiplexes several sandboxes onto one bridge, adopt `srt`'s token scheme — that is why it
   exists, and it is the cheap version of the same idea.
2. Read the `CONNECT` request. Resolve the hostname **in the bridge** (`socks5h`), never in the
   sandbox.
3. Apply the allowlist on `host:port`. Deny → SOCKS reply `0x02`, and log the denial with the tool
   call / command that caused it. A denial with no attribution is not actionable for the model or
   the operator, and it is the single most useful thing this engine produces.
4. Dial the resolved address, reply `0x00`, splice both directions until EOF.

Verified behaviour on this host, with the sandbox under `--unshare-net` and the bridge as the only
route:

```text
[1] allowed host -> rep 0        HTTP/1.1 200 OK
[2] denied host  -> rep 2        proxy log: DENY-HOST evil.invalid
[3] denied port  -> rep 2        proxy log: DENY-PORT example.com:22
[4] direct TCP   -> blocked      [Errno 101] Network is unreachable
[5] X11 abstract -> blocked      [Errno 111] Connection refused
```

**Implemented in `src/egress.rs`.** Notes on the parts the spec left open:

- **Allowlist matching** (`egress::host_allowed`): entry is `host` or `host:port`; empty list ⇒ any
  destination (the `network = true` default); a leading `*.` matches subdomains *and only*
  subdomains (`*.crates.io` matches `static.crates.io`, not `crates.io`, and not
  `a.crates.io.evil`). Case-insensitive. This is the only place the policy is applied, which is what
  R3g wants: the macOS backend reuses it unchanged.
- **Attribution** is the bridge's decision log (`EgressBridge::decisions`), lines `ALLOW h:p`,
  `DENY h:p`, `FAIL h:p`, `DENY-RESOLVE h:p`. Per the ordering above there is no SOCKS auth token;
  one bridge per tool call makes the call the unit.
- **A CONNECT is denied before dialling**, so a refused destination never learns the sandbox tried —
  and the denial is recorded, which is what makes it actionable.
- **Hardening deferred:** `PR_SET_PDEATHSIG` on the shim and `AF_VSOCK`/`io_uring` seccomp are R3d,
  not this item. They do not affect correctness of the bridge; they harden a killed-prior or
  hostile-syscall edge.

Two hardening details to copy from `srt`/`zerobox`: deny `AF_VSOCK` and `io_uring` at the seccomp
layer (R3d) because they route around `connect()`; and set `PR_SET_PDEATHSIG` in any helper process
so a killed prior does not leave a bridge listening into a dead namespace (`zerobox`'s
`harden_bridge_process`).

#### R3d — seccomp, demoted to the two cases nothing else covers

Not the X11 fix. `bwrap --seccomp FD` (an FD, not a path), denying only:

- `socket(AF_VSOCK, …)` — the netns does not fence VM sockets.
- `io_uring_setup` / `io_uring_enter` / `io_uring_register` — `IORING_OP_SOCKET` and
  `IORING_OP_CONNECT` create and use sockets in kernel context, bypassing any `connect()`-based rule
  *and* the netns check for the socket path.

**Do not** deny `socket(AF_UNIX)` wholesale. It breaks `agent-browser`'s control socket
(`src/tools.rs:2043`) and the bridge's own plumbing, and the netns already removed the abstract
socket it was meant to protect. `srt` needs that hammer because it does not have the bridge; prior
does not.

#### R3e — Environment hygiene (IMPLEMENTED 2026-10-22)

- `DISPLAY`, `XAUTHORITY`, `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS`,
  `SSH_AGENT_PID` are excluded by the R2 allowlist, and are now also in `DEFAULT_ENV_DISALLOW` for
  operators who opt back into the denylist. These are the *hint* for Finding 3, not the barrier, but
  not being told where the display is costs nothing.
- `--unshare-ipc` unconditionally. Closes the host SysV segment exposure and the MIT-SHM
  framebuffer path. Free.
- `--dev /dev` and `--proc /proc` by default. `git` fails without `/dev/null` and `cargo` without
  `/proc/self/exe`; mounting `/proc` makes R1 mandatory, which is the correct dependency.
- `--unshare-uts --hostname sandbox`. Both flags — `--unshare-uts` alone still reports the host
  hostname.
- **`/etc` narrowed to `ESSENTIAL_ETC`** (added during implementation; it was an "additional
  observation" rather than a numbered item). The whole directory used to be bound, exposing
  `/etc/passwd`, `/etc/hosts` and `/etc/os-release`. The working set is `/etc/alternatives` (without
  which all of `/usr/bin`'s alternatives symlinks dangle — `awk` is one), the loader cache, NSS
  config, the CA store, timezone, shell profile and `mime.types`/`mailcap`/`w3m`. Absent entries are
  skipped, because bwrap errors on a missing bind source. Verified: `awk`, `getent hosts`, DNS, TLS
  CA store, `git`, `rustc`, `node` all still work; `/etc/os-release` is absent.
  - Two bwrap behaviours make the plain list sufficient: it dereferences a symlink *source* (so
    `/etc/resolv.conf` → `/run/NetworkManager/...` needs no host-specific remap) and auto-creates
    missing parent directories for a bind target.

#### R3f — The config schema (this is what makes macOS cheap)

**IMPLEMENTED 2026-10-22 as [`NetworkPolicy`]** (`src/config.rs`), with the empty-allowlist question
settled the opposite way to this document's earlier lean. `network` is no longer `Option<bool>`; it
accepts a bare boolean (unchanged spelling) or a table, and serializes back to the terse form so a
rewritten config stays diff-friendly:

```text
network = false                                  # default: no egress, netns unshared
network = true                                   # open destinations, STILL netns-unshared
[tool_config.bash.sandbox.network]               # optional narrowing on top of `true`
enabled = true
allow   = ["github.com:443", "*.crates.io:443"]
```

- **`enabled = true, allow = []` is "open but host-safe", not deny-all** (decided 2026-10-22).
  Rationale: the reachability guarantee comes from the *namespace*, not the allowlist, so an empty
  allowlist is not a bypass. The safety property — abstract X11 socket and host `127.0.0.1`
  unreachable — holds for every enabled form, because `--unshare-net` is unconditional and egress
  goes through the bridge. "Open but safe for the host" is the intended first-class default;
  operators narrow with `allow` when they want to. To permit nothing, use `false`.
- Consequence for ordering: "host-safe" has no meaning until the bridge exists, so **R3b must land
  before the flag can be flipped unconditional**. Until then `build_bwrap_args` keeps
  `--unshare-net` conditional on `enabled`, because unsharing with no path out would silently remove
  all egress, including `agent_browser`. The flag flip and R3b are one change.
- A bare `network = true` is therefore *not* deprecated: it is exactly `enabled = true, allow = []`.

Do **not** reuse `srt`'s `allowUnixSockets` field: it is macOS-only and means two different things
per platform. If prior ever wants a socket allowlist, it is a separate key, and on Linux it belongs
to R4's Landlock work, not to the network policy.

#### R3g — macOS: the same policy, three rules, no bridge process

This is why the schema above is the deliverable and the Linux bridge is an implementation detail.
**R3c's policy engine is shared; only the transport differs.**

On macOS the transport is a plain TCP listener on host loopback — the bridge *is* the proxy, with no
namespace and no fd passing, because the sandbox shares the host's network stack. Seatbelt then
expresses the whole policy declaratively. `sandbox-runtime`'s
`src/sandbox/macos-sandbox-utils.ts:1318-1344` is the reference:

```scheme
(deny default)
(allow network-outbound (remote ip "localhost:3128"))
(allow network-bind (local ip "localhost:3128"))
(allow network-inbound (local ip "localhost:3128"))
```

No socat, no socket bind, no relay process, no passed fd. The asymmetry is the reverse of intuition:
**macOS is the easy platform for this, Linux is the hard one.**

Concretely, the platform split is one trait:

```text
Linux:   bind inside the netns -> pass the listening fd out -> accept in prior
macOS:   bind on host loopback -> accept in prior
both:    SOCKS5 policy engine (R3c) -> allowlist -> dial -> splice
```

Which also means **R3c can be written and tested on either platform first**, against a host-loopback
listener, before the Linux fd plumbing exists. That is the cheapest path to a working policy engine
and it is worth taking.

Notes for whoever implements it:

- **Host loopback on macOS is not private.** Any process running as the same user can connect to
  `127.0.0.1:3128` and use the bridge. That is acceptable (they already have the user's network
  access) but it means the bridge must hold no authority the sandbox lacks: it dials only what the
  allowlist permits and holds no credentials. If that ever changes, `srt`'s `proxyAuthToken` (SOCKS
  username/password) becomes necessary — the same mechanism that carries their attribution suffix.
- `localhost:<port>` does **not** match `::ffff:127.0.0.1`. Runtimes that connect to loopback over
  dual-stack sockets (Java) need `AF_INET`; `srt` injects `JAVA_TOOL_OPTIONS` for exactly this.
  Check whether that matters for prior's tools before porting the workaround.
- The profile needs `(allow network*)` replaced wholesale, not amended. `srt` emits
  `(allow network*)` when no restriction is wanted (`:1257`) — prior has no such mode.
- Seatbelt **can** filter Unix sockets by path (`(allow network-outbound (remote unix-socket
  (subpath …)))`), which Linux cannot do below Landlock ABI 9 (`srt` documents `allowUnixSockets` as
  macOS-only for this reason). mac is *ahead* of Linux on that axis; do not design the policy model
  around Linux's weak spot.
- **UDP on mac is already available**, and needs no architecture: Seatbelt is mediation-based and
  deny-default, so `(allow network-outbound (remote udp "*:443"))` is just another rule, with no
  bridge and no abstract namespace to protect. If a UDP tier is ever wanted, macOS *is* that tier;
  the only question is policy breadth — an enumerated allowlist, or a wildcard that has to be
  justified. Do not let the Linux deferral in R3h imply a mac limitation; the asymmetry runs the
  other way.

#### R3h — What is deferred, and why

- **Filtering by domain for HTTPS** requires seeing SNI, which means either TLS MITM (1500 lines,
  a CA to manage) or an allowlist on `host:port` only. prior takes the latter: `github.com:443` is
  allowed as an endpoint, not as a URL. Say so in the docs, because "allowlist by domain" raises an
  expectation SOCKS5 cannot meet.
- **Credential masking** (`srt`'s sentinel substitution) is out of scope. It is the right design for
  "the agent may use the key, only here", but it is strictly more machinery than prior needs now.
- **UDP** is out of scope; see R3a. If it is ever genuinely demanded, the shape to build is
  **`--unshare-net` plus a user-mode IP stack** (`pasta` or `slirp4netns`), not Landlock mediation.
  Reasons, recorded so the option is not re-derived:
  - *Why not Landlock.* Keeping the host netns and closing the abstract vector with
    `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET` (ABI 6 / Linux 6.12) plus `ResolveUnix` (ABI 9 /
    Linux 7.1) does work, and was verified on a 7.1 host. But it trades unreachability for
    mediation, which is the property R3 was written to stop depending on: a coverage gap degrades
    silently to "X11 reachable" instead of failing closed. A 6.12 floor is ~2027 on ordinary hosts;
    a 7.1 floor is much worse.
  - *Why not iptables/nftables alone.* Netfilter filters IP packets and is blind to `AF_UNIX`, so
    it contributes nothing to the X11 finding — the netns already covers that. Worse, there is no
    interface to filter on until one exists, and creating one unprivileged is the hard part: a veth
    pair can be created inside `unshare -Urn`, but its peer cannot be moved to the host netns
    (`ip link set v1 netns 1` fails), so a veth uplink means host root. Once `pasta`/`slirp4netns`
    does provide the interface, **in-sandbox nftables rules are the clean unprivileged way to get
    per-sandbox egress ACLs** and are worth remembering. They filter by IP, though, while the policy
    is written in hostnames; resolve-and-pin breaks on CDNs, round-robin and TTL churn. The SOCKS5
    bridge sees names at the layer the policy is expressed in.
  - *Why not SOCKS5 `UDP ASSOCIATE`.* Specified, but implemented by almost nothing outside a couple
    of browsers. It is a protocol that can be written and will not be used.
  - The cost of the user-mode stack is real: an external helper binary, host-side stack attack
    surface, and no per-destination allowlist unless the tool grows one — so the tier may be worse
    than having no UDP at all. Check first what actually needs UDP: QUIC falls back to TCP, DNS is
    resolved proxy-side, which leaves peer-to-peer, WebRTC and mDNS. prior's tool surface plausibly
    never touches those.

### R4 — The Unix-socket surface: document it, then constrain it with Landlock

**Not implemented.** This absorbed the Landlock work that R3 gave up, and it is now the *only*
place Landlock is proposed.

`--unshare-net` blocks abstract sockets but **not** filesystem sockets. R3's bridge removes the
abstract vector for good, but a binding can still expose a pathname socket — and prior's `ro`/`rw`
bindings accept *files*, so a single socket file is enough:

```text
--ro-bind /host/agent.sock /run/x.sock  →  connect("/run/x.sock") CONNECTED   (CONFIRMED)
```

So "don't bind directories containing sockets" is not a rule prior can enforce; the operator is one
path binding away. Three steps, in order of cost:

**(a) Document the two axes.** `doc/sandbox.md`'s `--unshare-net` claim currently reads as if it
blocks all egress. It blocks the abstract namespace and TCP; it does not block pathname sockets that
a binding exposes. Describe the two independent axes explicitly: *network* (TCP/UDP + abstract
sockets, governed by R3's policy) and *filesystem bindings* (pathname sockets, governed by nothing
today). An operator who binds `~/.config/gcloud` or `~/.docker` may be handing over a credential
helper socket without realising it, and that should be said next to the binding docs, not just here.

**(b) Consider warning on socket-shaped bindings.** prior can `stat` a binding source at argv-build
time (`build_bwrap_args`, `src/tools.rs:1712`) and know it is a socket. A warning (or a
`sandbox.allow_socket_binds` opt-in) is cheap and catches the common accident. It is a *hint*, not a
control — say so — but hints are how operators learn.

**(c) Landlock `ResolveUnix` where the kernel has it.** This is the only kernel-enforced per-path
control available, and it is the correct home for the work R3 no longer needs:

- **ABI 9 (Linux 7.1+): `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`** — per-path control of pathname socket
  connects. Verified on this host: with `ResolveUnix` granted under one directory only,
  `connect(/tmp/ll_ok/s.sock)` succeeded and `connect(/tmp/ll_no/s.sock)` was `EPERM`.
- **ABI 6 (Linux 6.12+): `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`** — belt-and-braces for the abstract
  vector R3 already removed by unreachability. Worth setting if trivially available, not worth a
  fallback ladder on its own.

Three cautions, all from the `landlock` crate's docs and nono's NEP-0004:

- **Pin an explicit ABI.** The crate deliberately refuses to expose "all flags for the running
  kernel", because a ruleset built against "whatever this kernel supports" silently changes meaning
  on a crate update.
- **Granting `ResolveUnix` without any grant flips a profile to deny-all pathname connects** on
  kernels that support it — fail-closed, but breaking. It needs curated default grants, which is
  nono's open item #1827 and is the real cost of this step.
- **Kernel coverage is the limit.** ABI 9 is Linux 7.1. On any ordinary 2026 host, only step (a) and
  (b) apply. Do not design for (c) being present.

Do **not** propose seccomp `socket(AF_UNIX)` here either: it cannot filter by path, so it denies
`agent-browser`'s own socket and the bridge, which is precisely the trade R3d rejects.

### R5 — Close the documentation gap

**Partly done.** `doc/sandbox.md:275-366` ("Environment isolation") and
`settings.toml.example:109-137` have been updated for R1 and R2: the default is now documented as
the allowlist, the reason a denylist cannot be the default is given (it leaks the desktop/session
variables), and the bwrap scrub is described. The caveat about `/proc/1/cmdline` is recorded.

Still open:

- The old claim that "the sandbox already blocks network egress by default, so a leaked key cannot
  be exfiltrated unless `network: true`" has been removed from the `env` section, but the
  **`--unshare-net` claim** elsewhere in `doc/sandbox.md` still reads as if it blocks all egress. It
  does not: the filesystem-socket channel (R4) defeats it.
- The R3 work (a dedicated X11 control) is not done, so the doc still cannot promise that the X
  server is unreachable.
- **R3 changes what `network: true` means**, so `doc/sandbox.md`'s network section needs a rewrite
  when R3a lands: netns always on, egress through the bridge, proxy variables published, tools that
  ignore them fail closed, host loopback unreachable, no UDP. That is a larger doc change than R1/R2
  needed, and it is user-facing.
- **`settings.toml.example` needs the `network` table** (R3f) once the schema lands, including the
  "empty allowlist means deny all" rule and the fate of bare `network = true`.

### R6 — Tests

**Partly done.** The existing env tests covered the filter in isolation, and would not have caught
any of the three findings, because all three are about the *composition* of the filter with the rest
of the bwrap invocation. Added for R1 and R2:

- `bwrap_own_environ_does_not_leak_host_secrets` — the test that would have caught Finding 1, and
  the regression test for the R1 fix. It must mount `/proc` to be meaningful; a test without `/proc`
  passes vacuously, which is exactly the blind spot in the original suite.
- `bwrap_own_environ_is_scrubbed_not_merely_filtered` — same, via a variable that is not a secret,
  so no value-based policy could have caught it.
- `bwrap_scrub_env_is_minimal` — pins the shape of the scrub set.
- `sandbox_list_lines_works_with_a_scrubbed_environment` — covers the second spawn site, which uses
  `std::process::Command` rather than duct.
- `default_policy_drops_host_desktop_and_session_vars` — asserts that `DISPLAY`, `XAUTHORITY`,
  `DBUS_SESSION_BUS_ADDRESS`, `XDG_RUNTIME_DIR`, `GPG_AGENT_INFO`, `SSH_AGENT_PID` and `I3SOCK` do
  not survive the default policy.
- `default_policy_omits_pwd_and_tmpdir_but_sandbox_sets_tmpdir` and
  `explicit_env_policy_suppresses_the_built_in_defaults` — pin the `PWD` / `TMPDIR` decisions and
  the "an explicit block is authoritative in full" rule.

Still wanted, for R3/R4. The first two are the ones that would have caught Finding 3 in the first
place, and both should be **socket-level assertions, not `xdotool`/`xdpyinfo` probes** — those may
not be installed, and a missing binary looks identical to a blocked one.

- `x11_abstract_socket_unreachable_with_network_enabled` — with `network: true`, `connect("\0/tmp/
  .X11-unix/X0")` fails. This is the regression test for Finding 3 under the new contract.
- `bridge_carries_allowed_host_and_refuses_others` — through `ALL_PROXY`, an allowlisted
  `host:port` gets a SOCKS `0x00`, a non-allowlisted one gets `0x02`, and a log line names the
  denial. Needs a loopback-only target so the test does not depend on the internet.
- `direct_tcp_is_blocked_even_with_network_enabled` — `connect(1.1.1.1:80)` fails with
  `ENETUNREACH`, proving the bridge is the only route.
- `empty_allowlist_denies_everything` — pins the fail-closed default from R3f (the rule most likely
  to regress into "allow all").
- `sandbox_sees_own_loopback` — binding `127.0.0.1:0` inside the sandbox works, so `agent-browser`
  and local test servers keep functioning under the always-on netns.
- For R3d: `AF_VSOCK` blocked, `io_uring_setup` blocked, `AF_INET`/`AF_UNIX` *not* blocked (the last
  is the assertion that stops someone re-adding the wholesale Unix-socket deny).
- For R4(b): a socket-shaped binding produces the warning; a directory binding does not.

---

## Appendix — reproducing the findings

All commands are plain `bwrap` invocations that mirror prior's `build_bwrap_args` output.
`bubblewrap 0.12.0`, Debian, i3/X11.

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

Note for anyone reproducing this by hand: the *default* sandbox has neither `/proc` nor `/dev`
bound, so a test that reads `/proc/1/environ` must bind `/proc`, and must not redirect stderr to
`/dev/null` (the redirection fails before the command runs, so the pipeline never executes and the
check passes vacuously). Both pitfalls are described under R1's implementation notes.

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

**The bridge, end to end** (R3b/R3c) — the shape R3 adopts, and the measurement that decided it:

```sh
# Supervisor side: a SOCKS5 server on a unix socket (stand-in for prior's bridge).
# Child side: bwrap --unshare-net, and inside it a relay listening on
# 127.0.0.1:3128 that forwards to that socket.
bwrap ... --unshare-net --unshare-pid --unshare-ipc --bind /tmp/sy /tmp/sy \
      --dev /dev --proc /proc -- /bin/sh -c 'python3 bridge.py & python3 probe.py'
```

Result (same sandbox, only the netns differing):

```text
== --unshare-net
x11_abstract: BLOCKED ([Errno 111] Connection refused)
direct_tcp:   BLOCKED ([Errno 101] Network is unreachable)
proxy_ok:     CONNECTED   HTTP/1.1 200 OK
proxy_denied: rep code 2  (proxy log: DENY-HOST evil.invalid, DENY-PORT example.com:22)

== shared netns (today's network: true)
x11_abstract: CONNECTED
direct_tcp:   CONNECTED
```

**The passed-listener variant** (NEP-0004's mechanism, which R3b prefers over the socket-file relay
above). The child binds inside the new netns and sends the *listening fd* out:

```python
# child, after unshare, inside the new netns
srv = socket.socket(); srv.bind(("127.0.0.1", 3128)); srv.listen(32)
socket.send_fds(boot_socketpair, [b"L"], [srv.fileno()])
# supervisor: recv_fds(...) -> socket.socket(fileno=fd).accept() ...
```

Result:

```text
host: received listening fd [4] msg b'L'
shim: listener bound in-ns: ('127.0.0.1', 3128)
HTTP: HTTP/1.1 200 OK
proxy log: ['ALLOW example.com:80']
```

And the `duct` fd-inheritance check, since prior spawns through `duct`:

```rust
// duct has no pass_fds, but before_spawn hands over &mut std::process::Command
duct::cmd("sh", ["-c", &script])
    .full_env(vec![("PATH", path)])
    .read()   // child wrote to an inherited fd
// → host got via fd: "via-duct"
```

**The seccomp X11 block** (`srt`'s filter, measured but **not** adopted by R3 — R3d keeps only the
vsock/io_uring half):

```sh
git clone --depth 1 https://github.com/anthropics/sandbox-runtime.git
gcc -o /tmp/seccomp-unix-block \
    sandbox-runtime/vendor/seccomp-src/seccomp-unix-block.c -lseccomp
/tmp/seccomp-unix-block /tmp/unix-block.bpf

# with the filter: X11 blocked even with network:true
bwrap ... --seccomp 3 --setenv DISPLAY :0 3</tmp/unix-block.bpf -- xdotool search --name ""
# → Error: Can't open display
```

---

## Where to start next (handover notes)

Written so that R3 can be picked up cold after a context reset. Everything here is either a verified
fact or an explicitly open decision; nothing needs to be re-derived.

**Read first:** R3a (the contract), R3b (the mechanism), R3f (the schema), R3g (the macOS split
that keeps the policy shared). Everything else is detail.

**Order of work, and why:**

1. **R3e (hygiene) — DONE**, **R3f (schema) — DONE**, **R3a (contract) — DONE**, **R3b (bridge) —
   DONE**, **R3c (policy engine) — DONE.** All landed 2026-10-22. The full chain is in place:
   `--unshare-net` is unconditional, `network = true` spawns the `prior-egress` shim, the shim binds
   in the netns and hands the listening fd out, and the SOCKS5 engine in `src/egress.rs` applies the
   allowlist. See "How it was actually implemented" under R3b for the shape, and R3c for the engine.
2. **R3d (seccomp) is the next Linux item**, and it is small and independent: `AF_VSOCK` and
   `io_uring` only. `PR_SET_PDEATHSIG` on the shim is the other half of the same hardening pass and
   can ride with it.
3. **R3g (macOS)** can start any time now: the schema and the SOCKS5 engine are both done, and the
   engine is platform-neutral (`egress::host_allowed`, `serve_socks5`'s decision order). mac needs no
   fd passing — a host-loopback listener plus three Seatbelt allow rules is the whole transport.
   Build the profile generator in one module and make the applier a strategy; do **not** back onto
   `zerobox`/`nono` as dependencies.
4. **R4** is independent of all of the above and can run in parallel with any step. Its (a) and (b)
   parts are documentation and a warning; only (c) needs the `landlock` crate.

**Facts already established, do not re-test:**

- `--unshare-net` blocks the abstract socket and TCP; it does **not** block pathname sockets that a
  binding exposes. Same-sandbox A/B measured.
- `lo` is already UP inside a fresh bwrap netns; `bind 127.0.0.1:0` succeeds from inside. No
  `ioctl` dance needed (this is what makes ~60 lines of `zerobox` unnecessary).
- A listening fd created inside the netns can be sent to a process outside it and will carry real
  traffic. Verified with `send_fds`/`recv_fds`.
- A non-`CLOEXEC` fd from a `socketpair` survives `exec` into `bwrap` **and** into the payload with
  the *same number* on both sides — measured through `duct`. This is why the implementation needs
  neither `pass_fds` (still unstable) nor a `before_spawn` hook: the number travels in
  `PRIOR_EGRESS_FD`. `duct` *can* pass fds via `before_spawn`, but the simpler route is used.
- SOCKS5 `CONNECT` relay with host+port filtering works under `--unshare-net`, and denials are
  distinguishable (`0x02`) from dial failures.
- A single socket *file* can be `--ro-bind`-ed in and connected to — the fallback mechanism, and the
  proof of R4's premise.
- Seatbelt needs no bridge on macOS: `(allow network-outbound (remote ip "localhost:3128"))` plus
  two more allow lines is the whole implementation (`srt:macos-sandbox-utils.ts:1318-1344`).
- **`curl` and `git` both honour `ALL_PROXY=socks5h://127.0.0.1:<port>` for plain HTTP and HTTPS**
  (measured against a TCP-loopback relay, i.e. R3g's transport shape). No per-client workaround is
  needed for prior's tool set, and this is the main unknown that stood in the way of R3g. See the
  "Measured" note under R3b.
- **SOCKS over a Unix *path* is unsupported by libcurl** (`--proxy socks5h:///path` fails to parse
  the proxy). Independent confirmation that the bridge must expose loopback TCP, not a bound socket
  path — so R3a's design is forced from the client side too.

**Open decisions, deliberately not made above:**

- ~~Bare `network = true`~~ — **settled 2026-10-22**: open destinations, still netns-unshared
  ("open but host-safe"). An allowlist narrows; it is not the safety mechanism. See R3f.
- ~~Bridge as a helper binary or an `sh -c` fragment?~~ — **settled by implementation**: a helper
  binary, `prior-egress`, bound in and resolved absolutely like `bsed`. An `sh -c` fragment cannot
  carry the fd plumbing cleanly and would need a shell in the payload.
- Does R4's socket-binding warning default to warn, or require an opt-in flag? (R4b)
- Should `agent-browser`'s control socket stay on AF_UNIX (needs no seccomp carve-out under this
  design) or move to TCP over the namespace loopback? (R3d/R3e — currently it works either way,
  since R3d does not deny `socket(AF_UNIX)`.)

**Do not re-litigate:**

- Landlock as the primary X11 control. It was the 2026-10-21 position and it is wrong once the netns
  stays on. Landlock belongs to R4.
- Wholesale seccomp `deny socket(AF_UNIX)`. It breaks `agent-browser` and the bridge; R3d keeps only
  `AF_VSOCK` and `io_uring`.
- Adopting `nono-proxy` or `zerobox-network-proxy` as dependencies. Both are 9k–22k lines with a
  full HTTP/TLS/OAuth surface, and one exact-pins six `rama` alpha crates.
