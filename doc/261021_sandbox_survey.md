# Sandbox Survey: Process-Level Isolation for Coding Agents on Linux and macOS

**Date**: 2026-10-21 **Scope**: User-space, process-level sandboxing primitives and toolchains for
running untrusted or semi-trusted code (LLM-generated code, coding-agent shell commands, package
installs) on a developer's own machine. Container runtimes and hosted microVM platforms are covered
only where they are the realistic alternative.

**Method note.** Every version, download count, date, license, and backend claim below was pulled
from the live sources on 2026-10-21 — crates.io API (`/api/v1/crates/<name>`) for crate metadata,
`raw.githubusercontent.com` for `Cargo.toml` / `README.md` / `LICENSE`, and the GitHub HTML page for
`"stargazerCount"` and `"spdxId"` (the REST API rate limit was exhausted during research, so the
HTML-embedded JSON payload was used instead). Where a crate's `license` field was `null` on
crates.io, the license was read from the repository's `Cargo.toml` or `LICENSE` file instead; both
sources are noted. Claims about *behavior* are quoted or paraphrased from the projects' own
documentation, not from independent testing — no sandbox here was executed or audited.

**`nono` was read from source.** A read-only checkout of `nolabs-ai/nono` was available at
`~/src/nono`, at commit `766fe84b` ("chore(deps): update Rust TLS dependencies (#2175)"), which
matches the commit GitHub reported for `main` on the same date. §4.1 and §4.1.1 are based on that
checkout — `crates/nono/src/sandbox/{linux,macos}.rs`, `docs/cli/internals/security-model.mdx`,
`docs/cli/internals/seatbelt.mdx`, and `neps/0004-opt-in-linux-namespace-isolation.md` — rather than
on the rendered documentation site alone. Where source and docs disagree, the source is correct and
the discrepancy is called out (see the `security-lists.toml` note in §9).

**Scope limit.** Only `nono` was inspected at source level. Every other project's claims rest on its
published README/docs, and their internals were not verified.

---

## Table of contents

- [Executive summary](#executive-summary)
- [1. The landscape in one table](#1-the-landscape-in-one-table)
- [2. OS primitives — the actual enforcement mechanisms](#2-os-primitives--the-actual-enforcement-mechanisms)
  - [2.1 Linux](#21-linux)
  - [2.2 macOS](#22-macos)
- [3. Standalone sandbox tools (non-Rust)](#3-standalone-sandbox-tools-non-rust)
- [4. Rust libraries and CLIs](#4-rust-libraries-and-clis)
  - [4.1 Full-featured cross-platform](#41-full-featured-cross-platform)
  - [4.2 Agent-oriented CLIs](#42-agent-oriented-clis)
  - [4.3 Building blocks](#43-building-blocks)
  - [4.4 MicroVM tier in Rust](#44-microvm-tier-in-rust)
- [5. What the major agents actually use](#5-what-the-major-agents-actually-use)
- [6. The macOS `sandbox-exec` problem](#6-the-macos-sandbox-exec-problem)
- [7. The Linux user-namespace problem](#7-the-linux-user-namespace-problem)
- [8. Decision guide](#8-decision-guide)
- [9. Gaps and open questions](#9-gaps-and-open-questions)
- [Appendix A — Version and popularity snapshot](#appendix-a--version-and-popularity-snapshot)
- [Appendix B — Sources](#appendix-b--sources)

---

## Executive summary

- **The strongest process-level primitive on Linux is Landlock + seccomp**, not bubblewrap.
  Bubblewrap is a *namespace* tool; Landlock is an *LSM* that lets an unprivileged process
  irreversibly restrict its own ambient rights. Landlock needs no user namespace, so it is immune to
  the Ubuntu 24.04+ `apparmor_restrict_unprivileged_userns` restriction that breaks bubblewrap.
  Codex CLI defaults to Landlock on Linux; Claude Code uses bubblewrap.
- **On macOS there is exactly one primitive** — Seatbelt (TrustedBSD MAC). `sandbox-exec(1)` is
  deprecated but is still the only *documented* way to apply it to arbitrary processes. There is no
  public replacement; Apple's own tracker issue asks for one and is unresolved. Several Rust
  projects therefore call the private `sandbox_init()` API directly to sidestep the deprecation
  warning.
- **The reference implementation to read is `anthropics/sandbox-runtime` (`srt`)** — it is the
  canonical "agent sandbox runtime": Seatbelt/bubblewrap + proxy-based network filtering +
  credential masking/injection. It is TypeScript, and it is what Claude Code's Bash tool is built
  on.
- **The closest Rust equivalents are `nono`, `zerobox`, `mxc-sdk`, `cageforge`, and `heel`.**
  `zerobox` is literally OpenAI Codex's sandboxing crates, vendored and renamed; `nono` is the most
  complete feature set of any Rust option.
- **Rust has no shortage of sandboxes; it has a shortage of *converged* ones.** Every project
  reimplements the same Seatbelt-profile generation and the same proxy. There is no equivalent of
  `libseccomp`'s stability or of a shared Rust `sandbox-runtime` crate that everyone builds on.
  (There is no `codex-sandboxing` on crates.io — Codex's crates are workspace-internal only.)
- **None of these is an escape-proof boundary, and the good projects say so.** `nono` states the
  distinction better than anyone: process-level sandboxing is a *capability model*, not a guest/host
  isolation model — it "reduces the authority available to a process" but "does not create a
  separate kernel or hardware boundary". `ai-jail` scopes itself to "trusted-but-fallible agent's
  mistakes, not a motivated attacker". If the threat model is hostile code, you want a microVM, and
  the capability layer goes *inside* it. §4.1.1 quotes nono's own list of reported escape classes.
- **The most under-rated failure mode is IPC, not the filesystem.** Landlock's filesystem rules do
  not govern pathname AF_UNIX `connect()`. A sandboxed process can still reach the D-Bus session
  bus, the systemd user manager (`systemd-run` starts processes *outside* the sandbox), the Docker
  socket, X11/Wayland, and `ssh-agent`. Abstract AF_UNIX is scoped only on Landlock v6+ kernels and
  is **fail-open below v6**. Any Linux design that has not addressed this class has a hole.

---

## 1. The landscape in one table

Isolation tiers, strongest first. "Process-level" means no VM and no container runtime.

- **Tier**: MicroVM
  - **Mechanism**: KVM / Virtualization.framework
  - **Linux**: Firecracker, libkrun, Cloud Hypervisor
  - **macOS**: `apple/container`, Tart, Lima
  - **Startup**: ~125 ms
  - **Overhead**: ~5 MB
  - **Typical users**: E2B, Vercel, Modal, Firecracker, AWS Lambda
- **Tier**: App kernel
  - **Mechanism**: Userspace syscall interception
  - **Linux**: gVisor (`runsc`), syd
  - **macOS**: —
  - **Startup**: ~100 ms
  - **Overhead**: 5–15% syscall
  - **Typical users**: Cloud Run, App Engine, Anthropic Claude web
- **Tier**: **Namespace**
  - **Mechanism**: user/mount/pid/net namespaces
  - **Linux**: **bubblewrap**, nsjail, Firejail, minijail
  - **macOS**: — (no userns)
  - **Startup**: single-digit ms
  - **Overhead**: negligible
  - **Typical users**: Flatpak, Claude Code (Linux), Codex
- **Tier**: **LSM**
  - **Mechanism**: Landlock + seccomp-bpf
  - **Linux**: **Landlock** (`landlock` crate), `landlock-restrict`, island
  - **macOS**: —
  - **Startup**: ~1 ms
  - **Overhead**: ~0
  - **Typical users**: Codex CLI (Linux), NVIDIA OpenShell
- **Tier**: **MAC**
  - **Mechanism**: TrustedBSD SBPL
  - **Linux**: —
  - **macOS**: **Seatbelt** / `sandbox-exec` / `sandbox_init()`
  - **Startup**: ~1 ms
  - **Overhead**: ~0
  - **Typical users**: Claude Code (macOS), Codex (macOS), Gemini CLI
- **Tier**: Container
  - **Mechanism**: OCI runtime
  - **Linux**: runc/crun, Podman, Docker
  - **macOS**: Docker Desktop, `apple/container`
  - **Startup**: 10–50 ms
  - **Overhead**: low
  - **Typical users**: Daytona, devcontainers
- **Tier**: In-process
  - **Mechanism**: interpreters
  - **Linux**: Deno permissions, WASM/WASI, bashkit
  - **macOS**: same
  - **Startup**: ~1 ms
  - **Overhead**: minimal
  - **Typical users**: WebContainers, Vercel `just-bash`

Two observations that the rest of this document expands on:

1. **Linux has two credible process-level answers; macOS has one.** The Linux choice is bubblewrap
   (namespaces) *or* Landlock (LSM). The macOS choice is Seatbelt, full stop.
2. **Namespace sandboxes and LSM sandboxes are not on a security ladder.** They are different
   mechanisms with different failure modes. A Landlock sandbox shares the host kernel and the host's
   *filesystem inode* view, but has no namespace to escape from. A bubblewrap sandbox gets a whole
   new mount tree but still shares the kernel. Neither is "stronger" in general; the relevant
   question is what the sandbox's escape would give you.

---

## 2. OS primitives — the actual enforcement mechanisms

### 2.1 Linux

#### Landlock

Landlock is a stackable Linux Security Module that lets
**any process, including unprivileged ones, restrict its own ambient rights**. Restrictions are
inherited by children and cannot be loosened once applied (`ruleset.restrict_self()`).

ABI timeline, from the `ABI` enum in `landlock-lsm/rust-landlock` (`src/compat.rs`), which cites the
introducing kernel commit for each version:

|Kernel|ABI|New capability                                                                       |
|------|---|-------------------------------------------------------------------------------------|
|5.13  |v1 |Basic filesystem access control                                                      |
|5.19  |v2 |`REFER` — rename/link across directories                                             |
|6.2   |v3 |`TRUNCATE`                                                                           |
|6.7   |v4 |TCP bind and connect filtering (`NET_BIND_TCP`, `NET_CONNECT_TCP`)                   |
|6.10  |v5 |`IOCTL_DEV` — device ioctl filtering                                                 |
|6.12  |v6 |`SIGNAL` and abstract UNIX socket scoping                                            |
|6.15  |v7 |Audit-log control: `log_same_exec()`, `log_new_exec()`, `log_subdomains()`           |
|7.0   |v8 |`all_threads()` — atomically apply the sandbox to every thread of the calling process|
|7.1   |v9 |`AccessFs::ResolveUnix` — control connections to *pathname* UNIX domain sockets      |
|7.2   |v10|Latest known to the crate; its `From<i32>` impl maps anything higher to V10          |

Note the practical consequence: **network filtering requires kernel 6.7+**. On 5.13–6.6 you get
filesystem control only.

The `landlock` crate deliberately refuses to expose "all access flags for the running kernel". From
`src/compat.rs`:

> Without `ABI`, it would be hazardous to rely on the full set of access flags […] a simple
> `cargo update` can result in a new version of this crate […] This crate cannot give any guarantee
> concerning the new restrictions resulting from an ABI change.

That is a subtle trap for sandbox builders: the kernel feature set moves, and a ruleset built
against "whatever this kernel supports" silently changes semantics when the crate is updated. Pin an
explicit ABI instead.

The reference Rust crate is `landlock` (`landlock-lsm/rust-landlock`), v0.4.7 (2026-07-27), 19.0M
downloads, MIT/Apache-2.0. Sibling crates from the same org: `go-landlock` (v0.x, 364 stars),
`island` (the CLI, v0.x, 352 stars, **explicitly "not yet ready for production use"**),
`landlockconfig`, `rust-landlock`, and `landlock-observability` (an `lltop` monitoring tool).

#### seccomp-bpf

Syscall filtering. The Rust crate of record is `seccompiler` (`rust-vmm/seccompiler`), v0.5.0
(2025-03-07), 25.4M downloads. `syscallz` (`kpcyrd/syscallz-rs`, 117k downloads) is a smaller
alternative, last updated 2023-09-28.

#### Namespaces

`bubblewrap` (`containers/bubblewrap`) is the unprivileged namespace sandbox that Flatpak is built
on. Its README is unusually candid about what it is and is not:

> bubblewrap is a tool for constructing sandbox environments. bubblewrap is not a complete,
> ready-made sandbox with a specific security policy. […] the level of protection between the
> sandboxed processes and the host system is entirely determined by the arguments passed to
> bubblewrap.

It uses `PR_SET_NO_NEW_PRIVS` to disable setuid escapes. The setuid mode was removed.

Alternatives in the same tier:

- **nsjail** (`google/nsjail`, Apache-2.0) — namespaces + seccomp + cgroups, protobuf config,
  `--chroot`/`pivot_root`, resource limits, pasta userland networking. Still actively developed
  (last push 2026-10-02). Originally for Google's CI/CTF infrastructure.
  **Requires building from source** — the README lists
  `autoconf bison flex gcc g++ git libprotobuf-dev libnl-route-3-dev libtool make pkg-config protobuf-compiler`.
  This makes it a poor fit for a tool that must install cleanly on an arbitrary developer machine.
- **Firejail** (`netblue30/firejail`, GPL-2.0, 7.7k stars) — SUID sandbox with a large profile
  library, mature, but desktop-oriented and SUID-based (an attack surface bwrap does not have).
- **minijail** (`google/minijail`, BSD-3-Clause, 393 stars) — the ChromeOS/Android launcher;
  namespaces + seccomp. Built for embedded targets, not general use.

### 2.2 macOS

#### Seatbelt / App Sandbox

Seatbelt is the TrustedBSD MAC framework that sandboxes App Store apps and Safari. It is the
**only** process-sandboxing mechanism on macOS. It is configured with SBPL (Scheme-like) profiles.

Three ways to apply it:

- **Mechanism**: `sandbox-exec(1)` CLI
  - **Status**: **Deprecated.** Prints
    `WARNING: sandbox-exec is deprecated. Consider adopting the App Sandbox instead.` Still present
    in macOS 15/26 and still the only *documented* path.
- **Mechanism**: `sandbox_init()` (private, in `libsandbox`)
  - **Status**: Undocumented but stable for over a decade. Used by `nono`.
- **Mechanism**: `sandbox_apply_container()` (newer, private)
  - **Status**: Mentioned by `nono` as the "newer" API it does *not* use.

The App Sandbox entitlement route (`com.apple.security.app-sandbox`) requires code signing with
Xcode and is designed for App Store GUI apps — not for wrapping an arbitrary third-party CLI.

Endpoint Security framework offers observation and authorization callbacks but
**no declarative profile** and no per-process confinement at exec time. System Extensions are
distribution-gated. None of these substitute for Seatbelt.

Chromium, Chrome, and many others use Seatbelt directly. `sandboxicon.com/tech/seatbelt/` notes it
"is the macOS counterpart to Landlock" — which is the right framing: a kernel-enforced MAC policy
that a process opts into.

---

## 3. Standalone sandbox tools (non-Rust)

- **Tool**: **Anthropic `srt`** (`anthropics/sandbox-runtime`)
  - **Platform**: macOS, Linux, Windows
  - **Mechanism**: Seatbelt / bubblewrap / restricted token + HTTP+SOCKS proxy
  - **License**: Apache-2.0, 5,626 stars
  - **Notes**: **The reference.** TypeScript; Claude Code's Bash tool is built on it. npm
    `@anthropic-ai/sandbox-runtime` at v0.0.81
- **Tool**: **Codex CLI** (`openai/codex`)
  - **Platform**: macOS, Linux, Windows
  - **Mechanism**: Seatbelt / Landlock+seccomp / restricted token
  - **License**: Apache-2.0, 130k stars
  - **Notes**: Default-on sandbox with `read-only` / `workspace-write` / `danger-full-access` modes
- **Tool**: **Firejail**
  - **Platform**: Linux
  - **Mechanism**: SUID namespaces + seccomp + caps
  - **License**: GPL-2.0
  - **Notes**: Profile library, desktop focus
- **Tool**: **nsjail**
  - **Platform**: Linux
  - **Mechanism**: namespaces + seccomp + cgroups
  - **License**: Apache-2.0
  - **Notes**: Protobuf config; build-from-source
- **Tool**: **gVisor** (`runsc`)
  - **Platform**: Linux
  - **Mechanism**: userspace app kernel
  - **License**: Apache-2.0, 19.7k stars
  - **Notes**: Cloud Run / App Engine / Functions
- **Tool**: **Firecracker**
  - **Platform**: Linux (KVM)
  - **Mechanism**: microVM
  - **License**: Apache-2.0
  - **Notes**: Lambda, Fargate, Vercel, E2B
- **Tool**: **minijail**
  - **Platform**: Linux
  - **Mechanism**: namespaces + seccomp
  - **License**: BSD-3-Clause
  - **Notes**: ChromeOS/Android
- **Tool**: **landlock-restrict**
  - **Platform**: Linux
  - **Mechanism**: Landlock
  - **License**: —
  - **Notes**: Part of the Landlock project; Go implementation with a CLI
- **Tool**: **island**
  - **Platform**: Linux
  - **Mechanism**: Landlock
  - **License**: Apache-2.0
  - **Notes**: 352 stars; **"Work in progress […] not yet ready for production use"**
- **Tool**: **Apple `container`**
  - **Platform**: macOS (arm64), macOS 26+
  - **Mechanism**: microVM via Virtualization.framework
  - **License**: Apple, 50.8k stars
  - **Notes**: OCI-compatible, per-container IPs, sub-second boot
- **Tool**: **Tart**
  - **Platform**: macOS (arm64)
  - **Mechanism**: Virtualization.framework
  - **License**: —
  - **Notes**: macOS *and* Linux guest VMs; OCI-registry images
- **Tool**: **Lima**
  - **Platform**: macOS
  - **Mechanism**: QEMU/Apple Virtualization
  - **License**: Apache-2.0, 22.1k stars
  - **Notes**: Linux VMs on macOS
- **Tool**: **Docker Sandboxes (`sbx`)**
  - **Platform**: Docker Desktop
  - **Mechanism**: microVM
  - **License**: —
  - **Notes**: Purpose-built CLI for Claude Code, Codex, Copilot, Gemini, Kiro
- **Tool**: **`fence`** (`fencesandbox/fence`)
  - **Platform**: macOS, Linux
  - **Mechanism**: Seatbelt / bubblewrap + proxy
  - **License**: —
  - **Notes**: Go; network denied by default, per-domain allow, agent permission manager
- **Tool**: **`ai-jail`**
  - **Platform**: Linux, macOS
  - **Mechanism**: bubblewrap + Landlock + seccomp / Seatbelt
  - **License**: GPL-3.0
  - **Notes**: Rust — see §4.2
- **Tool**: **`agent-safehouse`**
  - **Platform**: macOS
  - **Mechanism**: `sandbox-exec`
  - **License**: Apache-2.0, 2.1k stars
  - **Notes**: Deny-first Seatbelt profile system with per-agent profiles
- **Tool**: **`nontrivial` agent sandboxes** (`eugene1g`, `webcoyote/sandvault`,
  `bkircher/seatbelt`)
  - **Platform**: macOS
  - **Mechanism**: Seatbelt
  - **License**: various
  - **Notes**: Small, focused wrappers

**Not bwrap alternatives but worth naming:** `nono` and `fence` are the two that most closely
replicate `srt`'s semantics in a non-Rust form. `ai-jail` is the Rust one.

---

## 4. Rust libraries and CLIs

### 4.1 Full-featured cross-platform

#### `nono` — `nolabs-ai/nono`

**Apache-2.0 · 4,609 stars · 317 forks · crate v0.80.0 (2026-10-13) · 664,223 downloads**

By far the most-downloaded Rust sandbox crate. Self-described as "agent runtime security - zero
trust, zero setup, zero latency agent sandboxing". From the Sigstore team.

*Verified against a local read-only checkout at `766fe84b` ("chore(deps): update Rust TLS
dependencies (#2175)").*

**The most important thing to know about nono is that it is deliberately not a guest/host isolation
boundary.** From `docs/cli/internals/security-model.mdx`:

> nono provides fine-grained **capability control** sandboxing […]
> **This is intentionally a different model from a guest/host isolation boundary sandbox model**. It
> reduces the authority available to a process and any child process it spawns, but it does not
> create a separate kernel or hardware boundary. It is a capability model, not a VM model.

> The result is not an escape-proof sandbox. It is a practical capability system for controlling how
> agents use tools, designed to compose with stronger isolation when the deployment requires it.

That framing is the single most useful thing in this survey, and it applies to every project here.
Process-level sandboxing reduces the *authority* of a process; it does not give it a different
kernel. nono says so explicitly and then documents how to compose with an outer boundary (§4.1.1).

**Architecture:**

- **Linux backend:** Landlock via the `landlock` crate, plus a static seccomp baseline. The
  `ABI_PROBE_ORDER` in `crates/nono/src/sandbox/linux.rs` is `[V6, V5, V4, V3, V2, V1]` — nono caps
  its *enforced* ABI at v6 even though the shipped `landlock` crate 0.4.7 exposes v9. NEP-0004
  describes this as "the lag is in nono's own code (`DetectedAbi` and the feature listing stop at
  V6)" and tracks the catch-up as routine work.
- **macOS backend:** generates an SBPL profile and applies it via **`sandbox_init()`** — declared
  `unsafe extern "C"` in `crates/nono/src/sandbox/macos.rs`, with the comment "These are private
  APIs but have been stable for years". It also uses the *documented* `sandbox_extension_*` family
  (`sandbox.h`) to issue HMAC-SHA256-authenticated, path-specific capability tokens from an
  unsandboxed supervisor to a sandboxed process.
- **Filesystem model:** "allow discovery, deny content" — `file-read-metadata` is allowed on
  sensitive paths so programs can `stat` and fail gracefully, while `file-read-data` is denied. This
  mirrors TCC behavior. Verified in `macos.rs` (e.g. line 1048 emits
  `(deny file-read-data (subpath ...))`).
- **Network:** a dedicated `nono-proxy` crate (hyper/hyper-util) for HTTP + domain filtering, with a
  documented fail-closed matrix: proxy crash → no network, invalid token → 403, host not in
  allowlist → 403, DNS rebinding to a denied CIDR → 403, upstream TLS failure → 502.
- **Beyond sandboxing:** credential injection via proxy, Sigstore verification of skills/hooks
  (`crates/nono/src/trust/`), a Merkleized tamper-evident audit log (`crates/nono/src/audit.rs`),
  content-addressable rollback snapshots (`crates/nono/src/undo/snapshot.rs`), policy packs
  published to a registry (`nono pull <namespace>/<name>`, `nono search`), FFI bindings for Rust,
  Python, and C.

**Why it matters here:** it is the only Rust project that reproduces the *whole* `srt` feature set
*and* moves off the deprecated macOS binary — and the only one that documents its own escape classes
at length.

##### 4.1.1 nono's known escape classes (from NEP-0004)

`neps/0004-opt-in-linux-namespace-isolation.md` is unusually candid, and worth reading in full
before trusting nono as a boundary. It documents escape classes that had been
*reported against nono*:

- **Pathname AF_UNIX `connect()` is not governed by Landlock filesystem rules.** From inside a
  sandbox, `/run/user/<uid>/bus` reaches the D-Bus session bus or the systemd user manager socket
  (`systemd-run` starts processes *outside* the sandbox), `docker.sock`, X11/Wayland, and
  `ssh-agent`.
- **Abstract AF_UNIX sockets** are scoped only on Landlock V6+ kernels via
  `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`. Below V6 the default `IpcMode::SharedMemoryOnly` continues
  *without* scoping — "the class is fail-open below V6."
- **Non-TCP network** (UDP, ICMP, raw) is invisible to Landlock V4 network rules and is handled only
  at socket-family granularity by the static seccomp baseline; the proxy cannot carry non-HTTP
  protocols at all.
- **nono's existing countermeasure** for the pathname class (`linux.af_unix_mediation = "pathname"`,
  seccomp user-notify supervision) is "off by default, excluded from `nono wrap` and WSL2, and has
  proven operationally fragile in the field (#1128, #1399, #1420, #1715, #1970)."

The reasoning behind replacing mediation with namespaces is worth quoting, because it applies to
*any* seccomp-user-notify-based sandbox:

> intercept-and-continue mediation of pointer-argument syscalls is not a sound security boundary:
> the kernel documentation for seccomp user notification is explicit that the notified memory
> remains writable by the target, so the validated `sockaddr` bytes are subject to rewrite between
> check and use. Mediation inspects a hostile interface point-by-point; a namespace removes the
> interface wholesale.

nono's answer is `linux.isolate` / `--isolate`: opt-in, permanently default-off namespaces (user +
network + IPC + UTS) layered *under* the same Landlock + seccomp policy. "It removes reachability
rather than mediating it." The rationale for keeping it off by default is the same Ubuntu/AppArmor
constraint described in §7 — "A default that changed enforcement from host to host would violate
fail-secure behavior. When isolation is requested and the host refuses it, nono exits; it never
downgrades."

nono also states the fail-secure network matrix and the audit-durability caveat: the append-only
hash chain is written during session finalization, not per request, so "if the supervisor or proxy
is forcibly terminated mid-session, recent network events may be lost."

**Composing with an outer boundary** — nono's own deployment table:

- **Deployment**: nono on the host
  - **Outer isolation**: Shared host kernel and user account
  - **Capability granularity**: Fine-grained tool and operation policy
- **Deployment**: Standard container
  - **Outer isolation**: Namespaces, cgroups, shared host kernel
  - **Capability granularity**: Coarse mounts and network namespace policy
- **Deployment**: Hardened container runtime
  - **Outer isolation**: Runtime-specific isolation
  - **Capability granularity**: Coarse outer boundary
- **Deployment**: microVM
  - **Outer isolation**: Separate guest kernel and hardware virtualization
  - **Capability granularity**: Coarse virtual devices, mounts, network
- **Deployment**: nono inside a container or microVM
  - **Outer isolation**: Container/VM perimeter
  - **Capability granularity**: Fine-grained command policy inside

And its design guidance, which is the correct way to think about every tool in this survey:

1. Grant capabilities to the narrowest tool and invocation path that needs them.
2. Treat the supervisor, profile, approval backend, and resolved executable identity as
   security-sensitive parts of the trusted computing base.
3. Prefer brokered credentials and scoped network routes over raw secrets and unrestricted egress.
4. Regard every approval and bypass option as a deliberate expansion of authority.
5. Use a container or microVM when the threat model includes adversarial code escaping the
   same-user, shared-kernel boundary.

#### `zerobox` — `afshinm/zerobox`

**Apache-2.0 · 723 stars · crate v0.3.3 (2026-05-17) · 5,270 downloads**

Described as "Lightweight, cross-platform process sandboxing powered by OpenAI Codex's runtime."

This is **OpenAI Codex's sandboxing crates, vendored and renamed**. The workspace `Cargo.toml` lists
`upstream/sandboxing`, `upstream/linux-sandbox`, `upstream/windows-sandbox-rs`,
`upstream/process-hardening`, `upstream/network-proxy`, plus `crates/zerobox*`. The
`UPSTREAM_VERSION` file pins:

```text
rust-v0.131.0-alpha.22
# commit: 9b8cf56cdefb09f54564ccc295fd42f6647f558f
# synced: 2026-05-16T21:01:55Z
```

`scripts/sync.sh` re-vendors from `openai/codex` at a given tag and renames `codex-*` → `zerobox-*`.
The upstream crates map to:

|Vendored                   |Upstream Codex crate                                                  |
|---------------------------|----------------------------------------------------------------------|
|`zerobox-sandboxing`|`codex-rs/sandboxing` (contains `seatbelt.rs`, `landlock.rs`, `bwrap.rs`, `windows.rs`, `manager.rs`, `spawn.rs`, `violation.rs` and the `.sbpl` profile templates)|
|`zerobox-linux-sandbox`    |`codex-rs/linux-sandbox` (bubblewrap + `no_new_privs` + seccomp)      |
|`zerobox-network-proxy`    |`codex-rs/network-proxy`                                              |
|`zerobox-process-hardening`|`codex-rs/process-hardening`                                          |

Backends: macOS Seatbelt (`sandbox-exec`), Linux bubblewrap + seccomp + namespaces. Windows
(restricted tokens + ACLs + firewall) is "planned" in zerobox's own table — though upstream Codex
now has `windows-sandbox-rs` and `mxc-sandbox`, so a re-sync would likely pick that up.

Has Rust/TypeScript/Python SDKs and a CLI with `--allow-read`, `--deny-read`, `--allow-write`,
`--deny-write`, `--allow-net`, `--deny-net`, `--env`, `--allow-env`, `--deny-env`, `--secret`,
`--secret-host`, plus `--snapshot`/`--restore` for filesystem rollback. Self-reported overhead: ~10
ms and ~7 MB.

**Why it matters here:** if you want *Codex's* sandbox semantics in Rust, this is them, verbatim.

#### `mxc-sdk` — `microsoft/mxc`

**MIT · 2,952 stars · 161 forks · crate v1.0.1 (2026-10-17) · 1,877 downloads**

Microsoft eXecution Container: "a sandboxed code execution system for running untrusted code (model
output, plugins, and tools) on Windows, Linux, and macOS."

Backend matrix:

|Platform            |Default backend   |Other backends                                            |
|--------------------|------------------|----------------------------------------------------------|
|Windows 11 x64/ARM64|`processcontainer`|`windows_sandbox`*, `wslc`, `microvm`*, `hyperlight`*, `isolation_session`|
|Linux x64/ARM64     |`bubblewrap`      |`lxc`, `microvm`, `hyperlight`                            |
|macOS ARM64/x64     |`seatbelt`        |—                                                         |

(*experimental.) Rust, .NET, and Node SDKs. JSON-based versioned container-creation requests with
filesystem / network / UI (clipboard, display, GUI) policies, plus a diagnostics tool for
access-denied failures.

**Why it matters here:** it is the only Rust project with a *first-party* Windows process-container
backend, and the only one treating this as a product rather than a tool.

#### `cageforge` — `m62624/cageforge`

**Apache-2.0 · 1 star · crate v0.7.1 (2026-09-23) · 185 downloads**

Very young (repo created 2026-08-17) but architecturally the most considered of the new ones.
"OS-enforced isolation on Linux, Windows, and macOS", explicitly "inspired by OpenAI Codex."

- Crates are split per concern: `cageforge-backend-api`, `-bwrap`, `-cli`, `-command`, `-config`,
  `-linux`, `-macos`, `-network-proxy`, `-path`, `-permissions`, `-policy`, `-policy-compose`,
  `-windows`.
- The Linux backend "lowers the complete effective policy into Bubblewrap mounts, namespaces,
  seccomp rules, environment state, and process-lifecycle controls."
- Can **bundle a pinned Bubblewrap v0.12.0** (`linux-bundled-bubblewrap` feature) with digest
  verification, so the tool does not depend on a system `bwrap` being present or trustworthy.
- Its security model "was reviewed against the open-source Codex sandbox".

**Why it matters here:** it is the clearest example of the "policy layer over native primitives"
pattern, and the bundled-bubblewrap approach solves the "bwrap not installed / untrusted bwrap"
problem that matters for a tool you install on a stranger's laptop.

#### `heel` — `lexoliu/heel`

**MIT OR Apache-2.0 · 2 stars · crate v0.5.0 (2026-10-18) · 3,903 downloads**

"Cross-platform native sandboxing library for running untrusted code." Backends: `sandbox-exec` +
SBPL on macOS, Landlock ABI v4 + seccomp on Linux (kernel 6.7+), AppContainer + job objects on
Windows.

Distinctive design choices worth stealing:

- **Type-safe network policy at compile time** — `Sandbox<N: NetworkPolicy>` with `DenyAll`,
  `AllowAll`, `AllowList`, `CustomPolicy<F>`, `Audited<N>`. The proxy "refuses to forward to
  addresses local to the host", so an allowed hostname cannot be used to reach services on the
  machine running the sandbox.
- **"Nothing writable is executable."** The working directory, `Access::WRITE` paths, and shared
  temp dirs are all denied execute, so sandboxed code cannot drop a payload and run it. The
  deliberate exception is `Access::WRITE | Access::EXEC` on the same path — e.g. a Cargo `target/`
  directory that writes test binaries and runs them immediately.
- **Isolation levels** `strict` / `default` / `permissive`, exposed on the CLI.
- Typed IPC escape hatch over a Unix domain socket in a private directory *outside* the working
  directory, so sandboxed code cannot unlink the endpoint it depends on. Opens no network port.
- Resource limits and scheduling priority applied to the whole process tree.

### 4.2 Agent-oriented CLIs

#### `ai-jail` — `akitaonrails/ai-jail`

**GPL-3.0 · 1,392 stars · crate v2.11.1 (2026-10-21) · 3,239 downloads · Rust**

Runs AI coding agents in an OS sandbox: "bubblewrap plus Landlock, seccomp, and limits on Linux;
`sandbox-exec` on macOS."

The most aggressively documented threat model of the Rust set, and the most honest:

> **ai-jail** is *not* a lockdown or hard-security tool […] Its job is to be developer-friendly: it
> defends against a trusted-but-fallible agent's mistakes, not against a motivated attacker actively
> trying to escape.

What it does that the others don't:

- **Private `$HOME`** — the agent gets a fresh tmpfs home, not your host home.
- **Agent state mounting** — mounts the invoked harness's own credential dir (`~/.claude`,
  `~/.codex`, …) read-write so the agent starts pre-authenticated, with an opt-out
  (`--no-agent-state`).
- **Toolchain caches** — persistent jail-owned cargo/npm/go/maven caches, with the Rust toolchain
  mapped read-only, so `npm install` works on a fresh sandbox.
- **Filtered egress** with phantom-secret injection: the process sees a placeholder, the proxy
  substitutes the real value only for approved hosts. TCP/CONNECT-only, ports below 1024 refused.
- **An explicit flag matrix** where every capability is a named `--x` / `--no-x` pair: `--network`,
  `--gpu`, `--display`, `--x11`, `--audio`, `--kvm`, `--host-shm`, `--terminal-passthrough`,
  `--agent-state`, `--inherit-env`, `--macos-host-ipc`, `--worktree`, `--private-home`,
  `--toolchains`, `--github`, `--aws`, `--kube`, `--gcloud`, `--docker-config`.
- **Fails closed** on unreadable or invalid config, rather than launching with a weakened policy.
- Verifies the `bwrap` binary it uses is root-owned and not group/world-writable, and refuses a
  group-writable store without the sticky bit.
- Uses **Landlock V4** for the filtered-egress connect allow set, and a private network namespace.

#### `sandbox-runtime-rs` — `wangyedev/sandbox-runtime-rs`

**MIT · 8 stars · crate v0.1.1 (2026-01-23) · 188 downloads**

The literal Rust port of Anthropic's `srt`: same binary name (`srt`), same settings file
(`~/.srt-settings.json`), same config schema (`network.allowedDomains`, `network.deniedDomains`,
`network.allowUnixSockets`, `network.httpProxyPort`, `network.socksProxyPort`, `network.mitmProxy`,
`filesystem.denyRead`, `filesystem.allowWrite`, `filesystem.denyWrite`, `network.ignoreViolations`,
`enableWeakerNestedSandbox`, `ripgrep`, `mandatoryDenySearchDepth`, `allowPty`, `seccomp`). macOS
Seatbelt + HTTP/SOCKS5 proxy; Linux bubblewrap + seccomp + proxy over `socat` bridges. Also has a
`--control-fd` JSON-lines protocol for runtime config updates.

**Caveat:** last published 2026-01-23. It is a faithful port, not an actively developed project.

#### `skarn-sandbox` — `Rani367/Skarn`

**MIT OR Apache-2.0 · 5 stars · crate v1.0.1 (2026-06-24) · 4,485 downloads**

"OS-native process sandboxing (Seatbelt / Landlock+seccomp / AppContainer) with a unified API",
inside an MCP gateway that also does Code Mode and shell-output token compression. Uses the
`landlock` and `seccompiler` crates directly. macOS and Linux sandboxes are runtime-verified in CI
on every push; the Windows AppContainer backend is the least exercised.

#### `kernex-sandbox` — `kernex-dev/kernex`

**Apache-2.0 OR MIT · 2 stars · crate v0.10.0 (2026-06-12) · 1,701 downloads**

Seatbelt (macOS) + Landlock (Linux, kernel 5.13+) with a configurable `SandboxProfile` allow/deny
list, as one crate in a broader AI agent framework (providers, memory, MCP, pipelines). Interesting
mainly as an example of the sandbox crate being a *component* of an agent runtime rather than the
product itself.

### 4.3 Building blocks

These are the primitives, not drop-in `srt` replacements.

- **Crate**: `landlock`
  - **Version**: 0.4.7 (2026-07-27)
  - **Downloads**: 19,028,345
  - **Notes**: The Landlock Rust binding; MIT/Apache-2.0
- **Crate**: `seccompiler`
  - **Version**: 0.5.0 (2025-03-07)
  - **Downloads**: 25,451,348
  - **Notes**: seccomp-bpf, from the rust-vmm org
- **Crate**: `syd`
  - **Version**: 3.59.0 (2026-09-01)
  - **Downloads**: 3,715,817
  - **Notes**: "rock-solid application kernel" — Landlock + namespaces + ptrace + seccomp-notify;
    kernel ≥5.19. Lives on sourcehut, not GitHub
- **Crate**: `hakoniwa` / `hakoniwa-cli`
  - **Version**: 1.8.0 (2026-09-25)
  - **Downloads**: 42,974 / 22,639
  - **Notes**: namespaces + `pivot_root` + cgroups + Landlock + seccomp + pasta networking.
    Linux-only, desktop-app profiles
- **Crate**: `procjail`
  - **Version**: 0.3.2 (2026-08-07)
  - **Downloads**: 47,950
  - **Notes**: Picks the best available strategy: bubblewrap > firejail > unshare > rlimits-only.
    Strips 36 secret env vars by default
- **Crate**: `rstrict`
  - **Version**: 0.1.14 (2025-05-15)
  - **Downloads**: 8,361
  - **Notes**: Lightweight Landlock exec CLI
- **Crate**: `birdcage`
  - **Version**: 0.8.1 (**2024-04-19**)
  - **Downloads**: 153,391
  - **Notes**: Cross-platform FS+net sandbox. **GPL-3.0 and effectively unmaintained** — 2 years
    stale
- **Crate**: `syscallz`
  - **Version**: 0.17.0 (2023-09-28)
  - **Downloads**: 117,502
  - **Notes**: Minimal seccomp
- **Crate**: `extrasafe`
  - **Version**: 0.5.1 (2024-04-16)
  - **Downloads**: 37,501
  - **Notes**: Capability reduction; stale
- **Crate**: `seatbelt-rs`
  - **Version**: 0.1.0 (2026-06-28)
  - **Downloads**: 26
  - **Notes**: Thin type-safe wrapper over the macOS sandbox API
- **Crate**: `cageforge-macos`
  - **Version**: 0.7.1 (2026-09-23)
  - **Downloads**: 193
  - **Notes**: macOS backend of cageforge, published standalone
- **Crate**: `mxc-alpha-seatbelt-common`
  - **Version**: 0.7.1 (2026-08-14)
  - **Downloads**: 126
  - **Notes**: Seatbelt backend support for MXC

### 4.4 MicroVM tier in Rust

If the threat model is "hostile code", process-level sandboxing is the wrong tier and you want a
microVM. These are the Rust options.

- **Crate**: `microsandbox`
  - **Version**: 0.7.8 (2026-10-09)
  - **Downloads**: 41,736
  - **Notes**: libkrun microVMs, sub-100 ms boot, OCI-compatible, cross-platform, secrets that never
    enter the VM. Also has `microsandbox-types` (20,967 dl) for the task/wire contract
- **Crate**: `a3s-box-runtime`
  - **Version**: 3.3.0 (2026-09-23)
  - **Downloads**: 1,825
  - **Notes**: "MicroVM runtime engine — VM lifecycle, OCI images, attestation, networking"
- **Crate**: `bashkit`
  - **Version**: 0.19.0 (2026-10-15)
  - **Downloads**: 96,053
  - **Notes**: **In-process** virtual sandbox, not a VM: 167 shell commands reimplemented in Rust,
    no `fork`/`exec` at all, virtual filesystem, resource limits. A different threat model — the
    sandboxed code cannot touch the real FS unless `realfs` is enabled

`bashkit` is worth calling out because it inverts the usual assumption: instead of containing a real
process, it *reimplements* the process. Nothing executes, so there is nothing to escape from. The
trade-off is compatibility — 167 commands is not `bash`.

---

## 5. What the major agents actually use

- **Agent**: **Claude Code**
  - **macOS**: Seatbelt (`sandbox-exec`)
  - **Linux**: bubblewrap + seccomp + `socat`
  - **Windows**: unsandboxed natively (use WSL2)
  - **Cloud**: gVisor for Claude web
- **Agent**: **Codex CLI**
  - **macOS**: Seatbelt
  - **Linux**: **Landlock + seccomp**
  - **Windows**: restricted tokens / AppContainer-style
  - **Cloud**: —
- **Agent**: **Gemini CLI**
  - **macOS**: Seatbelt
  - **Linux**: Docker/Podman (`.gemini/sandbox.Dockerfile`)
  - **Windows**: —
  - **Cloud**: —
- **Agent**: **Cursor / Devin / Copilot Coding Agent**
  - **macOS**: —
  - **Linux**: —
  - **Windows**: —
  - **Cloud**: per-task hosted VM

Two observations that matter for anyone building tooling on top:

1. **The two most popular agents made opposite choices on Linux.** Claude Code uses bubblewrap;
   Codex uses Landlock. Since bubblewrap is the one that breaks on Ubuntu 24.04+ (§7), an agent that
   works under one and not the other is a normal, non-exotic support issue.
2. **Claude Code's sandbox covers the Bash tool only.** File tools (Read/Edit/Write), WebFetch, MCP
   servers, hooks, LSP servers, and `apiKeyHelper` all run *outside* it. Codex's equivalent
   `danger-full-access` mode is the same idea: the sandbox is one tool's boundary, not the agent's.

---

## 6. The macOS `sandbox-exec` problem

`sandbox-exec(1)` is deprecated. The man page says to adopt App Sandbox instead. But App Sandbox
requires the `com.apple.security.app-sandbox` entitlement, code signing, and an Xcode project — it
is designed for App Store GUI apps, not for wrapping a CLI that a user downloaded.

The clearest statement of the problem is Apple's own issue tracker. `apple/containerization#737`,
opened 2026-05-12 and still open as of 2026-09-13 (the last comment):

> **Problem**
>
> `sandbox-exec(1)` is the only documented mechanism for applying Seatbelt (TrustedBSD MAC) policies
> to arbitrary user-space processes on macOS without enrolling in the App Store or using Xcode
> entitlements.
>
> **Use case**
>
> darwin-k8s runs trusted darwin/arm64 Go binaries as native macOS processes under a Seatbelt
> profile applied via `sandbox-exec -f <profile> <binary>` […] We need answers to three questions:
>
> 1. Will `sandbox-exec` be removed in a specific future macOS version?
> 2. Is there a non-deprecated equivalent […] (e.g. a documented `sandbox_init_with_parameters` C
>    API, or a new framework)?
> 3. Will Apple Containerization eventually offer a "lite" isolation mode that sandboxes a
>    darwin/arm64 process without spinning up a Linux VM?

The only answer in that thread is a link to an Apple Feedback ticket (FB24765111) with the comment
"I submitted FB24765111 for this which seems like probably the required path for this if anyone
wants to dup it". So: **no timeline, no replacement, as of 2026-10-21.**

### How the Rust projects cope

- **Approach**: Call the private `sandbox_init()` directly
  - **Projects**: `nono` (explicitly documented), `dsh-seatbelt-sandbox` (a C11 "native loader"
    plugin)
- **Approach**: Keep using `sandbox-exec`
  - **Projects**: `ai-jail`, `zerobox`, `heel`, `agent-safehouse`, `fence`, `sandbox-runtime-rs`
- **Approach**: Move to VM/microVM isolation
  - **Projects**: `apple/container`, Tart, Lima, `mxc` (microvm backend), Docker `sbx`

The deprecation is a *banner*, not a removal: `sandbox-exec` still works in macOS 15/26. But every
project that calls it prints the warning on every invocation, and there is no schedule for removal.

**If you are choosing today:** using `sandbox_init()` directly is what `nono` does and is what the
DSH plugin does; it is stable but private, so it carries its own (small, but nonzero) risk of
breaking. Using `sandbox-exec` is stable today and carries the (larger, but unscheduled) risk of
being removed. Using a microVM sidesteps both at the cost of VM overhead and losing native macOS
semantics.

---

## 7. The Linux user-namespace problem

Landlock and bubblewrap are not interchangeable on a modern Linux desktop, and the reason is
Ubuntu's AppArmor policy.

`kernel.apparmor_restrict_unprivileged_userns=1` is the **default in Ubuntu 24.04 LTS**. Under it,
unconfined applications are *denied* user-namespace creation; an application that legitimately needs
one must run under an AppArmor profile that explicitly grants it. The affected set includes
**bubblewrap, Chrome/Chromium's `--sandbox`, Electron apps, Flatpak, Steam**, and anything built on
raw `unshare(CLONE_NEWUSER)`.

Ubuntu's own justification is that unprivileged user namespaces were an ingredient in roughly 44% of
the kernel privilege-escalation exploits in Google's survey.

The state of workarounds, per the Ubuntu updates:

- An **experimental `bwrap-userns-restrict` AppArmor profile** ships in the optional
  `apparmor-profiles` package, under `/usr/share/apparmor/extra-profiles/`, **disabled by default**.
  Its two-profile design gives `bwrap` the capabilities it needs for setup, then forces executed
  children into a stacked profile that denies capabilities.
- Replacing it with a one-line unconfined userns grant is a bad trade: it turns `/usr/bin/bwrap`
  into a trampoline around the mitigation.
- Inside Docker or Kubernetes, the container runtime's own seccomp and AppArmor policies introduce
  *separate* gates. A host per-application policy is not automatically a safe container policy, and
  `CAP_SYS_ADMIN` alone is both broad and sometimes insufficient.

### What this means per-project

|Project            |Linux backend                               |Immune to the Ubuntu restriction?|
|-------------------|--------------------------------------------|---------------------------------|
|`nono`             |Landlock (+ opt-in namespaces)              |**Yes** for the Landlock path    |
|`ai-jail`|Landlock V4 + seccomp + bubblewrap + namespaces|Partially — Landlock path yes; bwrap path no|
|`codex` / `zerobox`|bubblewrap + Landlock + seccomp             |Landlock path yes                |
|`cageforge`  |bubblewrap (optionally bundled v0.12.0)|No (unless a bundled/verified bwrap is used)|
|`mxc`              |bubblewrap (default), lxc, microvm          |No                               |
|`heel`             |Landlock ABI v4 + seccomp                   |**Yes**                          |
|`skarn-sandbox`    |Landlock + seccomp                          |**Yes**                          |

Two distinct failure modes are worth separating:

- **`bwrap` cannot start.** Ubuntu refuses the userns. Fix: install the AppArmor profile, or use a
  Landlock-based backend.
- **`bwrap` starts but nested tools fail.** Claude Code's docs call this out specifically: "Chromium
  or rootless Podman fails inside the sandbox on Linux" and "Bubblewrap fails to start inside a
  container". These are *nested* userns requests, which need
  `kernel.apparmor_restrict_unprivileged_unconfined` handling, not just the outer grant.

**Practical takeaway:** if you are building a tool that must work on an arbitrary developer laptop
in 2026, Landlock-first is the more robust default on Linux, and bubblewrap is an optional faster
path where it is known to work. `cageforge`'s bundled-bubblewrap feature and `ai-jail`'s
binary-trust checks are both answers to the "which bwrap am I actually running?" problem.

---

## 8. Decision guide

### If you are wrapping a coding agent's shell commands locally

1. **You need the `srt` feature set in Rust** → `nono`. It is the only one with proxy-based network
   filtering, credential masking, and credential injection in one package, and it uses
   `sandbox_init()` rather than the deprecated binary.
2. **You want Codex's exact semantics** → `zerobox` (it *is* Codex's crates). Re-sync with
   `scripts/sync.sh` to pick up upstream changes; note the crates.io release is from 2026-05-17 and
   the vendored pin is `rust-v0.131.0-alpha.22`.
3. **You want a good CLI and agent ergonomics** → `ai-jail`. Best-documented threat model, best flag
   matrix, best private-`$HOME` and toolchain-cache handling. GPL-3.0 — a licensing consideration
   for anything you intend to ship.
4. **You want config-schema parity with `srt`** → `sandbox-runtime-rs`. Faithful, but stale
   (2026-01-23).
5. **You want the most carefully *designed* policy model** → `cageforge`. Also the one that solves
   the bundled-bubblewrap trust problem.

### If you are building an agent runtime and want the sandbox as a component

`nono` (feature-complete), `cageforge` (policy/backend separation), `heel` (type-safe network
policy, write-then-execute protection), or `mxc-sdk` (if Windows matters).

### If the code is genuinely hostile

Process-level sandboxing is the wrong tier. Use `microsandbox` (libkrun microVMs) or
`a3s-box-runtime` in Rust, or `apple/container` / Docker `sbx` / Firecracker for non-Rust. Note that
a microVM changes the compatibility story — the agent runs in Linux, not in your host OS.

### If you are on macOS and worried about `sandbox-exec` removal

- `nono` (calls `sandbox_init()` directly) — the least-risk option that keeps native semantics.
- `mxc` or `apple/container` — the zero-risk option that gives up native semantics.
- Accept the banner and keep using `sandbox-exec` — what most projects do today.

### If you need Windows

`mxc-sdk` (Microsoft's own ProcessContainer backend) is the only first-party Rust option.
`cageforge` has a `cageforge-windows` crate. Everything else either omits Windows or expects WSL2.

---

## 9. Gaps and open questions

- **The IPC escape class is the ecosystem's blind spot.** Landlock's filesystem rules do not cover
  pathname AF_UNIX `connect()`, so a Landlock-only sandbox can still reach the D-Bus session bus,
  the systemd user manager, `docker.sock`, X11/Wayland, and `ssh-agent`. Abstract AF_UNIX needs
  Landlock v6+ and is fail-open below it. Only `nono` addresses this head-on, and its answer is
  opt-in namespaces — which collide with the Ubuntu/AppArmor constraint from §7. The other Linux
  projects in this survey (including `zerobox`/`codex` and `heel`) do not claim to cover it.
- **No shared Rust `sandbox-runtime` crate.** Codex's sandboxing crates are workspace-internal
  (`codex-sandboxing` is not on crates.io), so `zerobox` had to vendor-and-rename them. Anthropic's
  `srt` is npm-only. Every Rust project reimplements Seatbelt profile generation and the network
  proxy from scratch — at least six times in this survey (`nono`, `zerobox`, `heel`, `cageforge`,
  `ai-jail`, `skarn-sandbox`).
- **Seatbelt profile generation has no standard library.** `seatbelt-rs` is 26 downloads. The
  `.sbpl` templates are duplicated across `codex-rs/sandboxing` (4 template files:
  `seatbelt_base_policy.sbpl`, `seatbelt_network_policy.sbpl`, `seatbelt_preferences_policy.sbpl`,
  `seatbelt_read_only_platform_defaults.sbpl`) and in nono's CLI, where the system-path baseline is
  hardcoded in `crates/nono-cli/src/tool-sandbox/platform/macos.rs` (`add_macos_runtime_baseline`).
  Note that nono's `docs/cli/internals/seatbelt.mdx` says those paths are "loaded from
  `security-lists.toml`" — but **no such file exists in the repository** at `766fe84b`; the paths
  are a hardcoded array in Rust. A minor documentation drift, but a good illustration of how little
  of this ecosystem is standardised.
- **Apple has not answered the `sandbox-exec` question**, and the deprecation banner is the only
  signal. Anyone building on macOS process isolation is making a bet on unstated Apple policy.
- **Landlock's network filtering needs kernel 6.7+** (2024-01). Ubuntu 22.04 ships 5.15. Any
  Landlock-first design must degrade to "no network filtering" on older kernels, and the degradation
  must be explicit rather than silent.
- **Landlock nesting is limited to 16 levels** in the kernel; `island` flags this and plans
  "synthetic chained ruleset" support. Relevant if you nest sandbox-in-sandbox.
- **Bubblewrap's "trust problem"** — is the `bwrap` on `PATH` the real one? `ai-jail` validates
  ownership and permissions; `cageforge` can bundle and digest-verify v0.12.0. Neither `zerobox` nor
  `mxc` addresses this by default.
- **`birdcage` is dead** — GPL-3.0, last release April 2024, and still the second-most-downloaded
  Rust sandbox crate (153k). Its successor appears to be `nono` from the same problem domain.
- **No benchmark.** No project in this survey publishes a comparative overhead benchmark against the
  others. `zerobox` publishes its own (~10 ms, ~7 MB); `nono` claims "zero latency". Neither is
  comparable across tools.

---

## Appendix A — Version and popularity snapshot

All values read on 2026-10-21 from the crates.io API and GitHub HTML payloads.

### Full-featured cross-platform

|Crate      |Version|Crate updated|Downloads|GitHub stars|License          |Repo              |
|-----------|-------|-------------|---------|------------|-----------------|------------------|
|`nono`     |0.80.0 |2026-10-13   |664,223  |4,609       |Apache-2.0       |`nolabs-ai/nono`  |
|`zerobox`  |0.3.3  |2026-05-17   |5,270    |723         |Apache-2.0       |`afshinm/zerobox` |
|`mxc-sdk`  |1.0.1  |2026-10-17   |1,877    |2,952       |MIT              |`microsoft/mxc`   |
|`cageforge`|0.7.1  |2026-09-23   |185      |1           |Apache-2.0       |`m62624/cageforge`|
|`heel`     |0.5.0  |2026-10-18   |3,903    |2           |MIT OR Apache-2.0|`lexoliu/heel`    |

### Agent-oriented CLIs

- **Crate**: `ai-jail`
  - **Version**: 2.11.1
  - **Crate updated**: 2026-10-21
  - **Downloads**: 3,239
  - **GitHub stars**: 1,392
  - **License**: GPL-3.0
  - **Repo**: `akitaonrails/ai-jail`
- **Crate**: `sandbox-runtime`
  - **Version**: 0.1.1
  - **Crate updated**: 2026-01-23
  - **Downloads**: 188
  - **GitHub stars**: 8
  - **License**: MIT
  - **Repo**: `wangyedev/sandbox-runtime-rs`
- **Crate**: `skarn-sandbox`
  - **Version**: 1.0.1
  - **Crate updated**: 2026-06-24
  - **Downloads**: 4,485
  - **GitHub stars**: 5
  - **License**: MIT OR Apache-2.0
  - **Repo**: `Rani367/Skarn`
- **Crate**: `kernex-sandbox`
  - **Version**: 0.10.0
  - **Crate updated**: 2026-06-12
  - **Downloads**: 1,701
  - **GitHub stars**: 2
  - **License**: Apache-2.0 OR MIT
  - **Repo**: `kernex-dev/kernex`

### Building blocks

| Crate         | Version | Crate updated | Downloads  | License        |
| ------------- | ------- | ------------- | ---------- | -------------- |
| `landlock`    | 0.4.7   | 2026-07-27    | 19,028,345 | MIT/Apache-2.0 |
| `seccompiler` | 0.5.0   | 2025-03-07    | 25,451,348 | —              |
| `syd`         | 3.59.0  | 2026-09-01    | 3,715,817  | —              |
| `bashkit`     | 0.19.0  | 2026-10-15    | 96,053     | MIT            |
| `hakoniwa`    | 1.8.0   | 2026-09-25    | 42,974     | —              |
| `procjail`    | 0.3.2   | 2026-08-07    | 47,950     | MIT            |
| `rstrict`     | 0.1.14  | 2025-05-15    | 8,361      | —              |
| `birdcage`    | 0.8.1   | 2024-04-19    | 153,391    | GPL-3.0        |
| `syscallz`    | 0.17.0  | 2023-09-28    | 117,502    | —              |
| `extrasafe`   | 0.5.1   | 2024-04-16    | 37,501     | —              |
| `seatbelt-rs` | 0.1.0   | 2026-06-28    | 26         | —              |

### MicroVM / in-process tier

| Crate                | Version | Crate updated | Downloads | Repo                           |
| -------------------- | ------- | ------------- | --------- | ------------------------------ |
| `microsandbox`       | 0.7.8   | 2026-10-09    | 41,736    | `superradcompany/microsandbox` |
| `microsandbox-types` | 0.7.8   | 2026-10-09    | 20,967    | `superradcompany/microsandbox` |
| `a3s-box-runtime`    | 3.3.0   | 2026-09-23    | 1,825     | `A3S-Lab/Box`                  |

### Non-Rust reference points

| Project                      | Stars   | License      | Repo                         |
| ---------------------------- | ------- | ------------ | ---------------------------- |
| `anthropics/sandbox-runtime` | 5,626   | Apache-2.0   | `anthropics/sandbox-runtime` |
| `openai/codex`               | 130,350 | Apache-2.0   | `openai/codex`               |
| `google/gvisor`              | 19,742  | Apache-2.0   | `google/gvisor`              |
| `apple/container`            | 50,830  | Apple        | `apple/container`            |
| `netblue30/firejail`         | 7,709   | GPL-2.0      | `netblue30/firejail`         |
| `lima-vm/lima`               | 22,139  | Apache-2.0   | `lima-vm/lima`               |
| `google/nsjail`              | 4,165   | Apache-2.0   | `google/nsjail`              |
| `google/minijail`            | 393     | BSD-3-Clause | `google/minijail`            |

---

## Appendix B — Sources

**Primary — local checkout** (`~/src/nono` @ `766fe84b`): `crates/nono/src/sandbox/linux.rs`,
`crates/nono/src/sandbox/macos.rs`, `crates/nono/src/sandbox/linux_namespace.rs`,
`crates/nono/src/capability.rs`, `crates/nono/src/audit.rs`, `crates/nono/src/trust/`,
`crates/nono/src/undo/`, `crates/nono-cli/src/tool-sandbox/platform/macos.rs`,
`crates/nono-cli/src/cli.rs`, `crates/nono/Cargo.toml`, `crates/nono-proxy/Cargo.toml`,
`docs/cli/internals/security-model.mdx`, `docs/cli/internals/seatbelt.mdx`,
`docs/cli/internals/landlock.mdx`, `neps/0004-opt-in-linux-namespace-isolation.md`, `ROADMAP.md`,
`SECURITY.md`.

**Primary — crates.io API** (`https://crates.io/api/v1/crates/<name>`): `nono`, `zerobox`,
`mxc-sdk`, `cageforge`, `heel`, `ai-jail`, `sandbox-runtime`, `skarn-sandbox`, `kernex-sandbox`,
`landlock`, `seccompiler`, `syd`, `hakoniwa`, `hakoniwa-cli`, `procjail`, `rstrict`, `birdcage`,
`syscallz`, `extrasafe`, `seatbelt-rs`, `cageforge-macos`, `mxc-alpha-seatbelt-common`,
`microsandbox`, `microsandbox-types`, `a3s-box-runtime`, `bashkit`, `koda-sandbox`, `adk-sandbox`,
`sbe-core`, `zerobox-{sandboxing,linux-sandbox,network-proxy,process-hardening}`.

**Primary — repository files** (`raw.githubusercontent.com`): `README.md` and `Cargo.toml` for
`nolabs-ai/nono`, `afshinm/zerobox`, `microsoft/mxc`, `m62624/cageforge`, `lexoliu/heel`,
`akitaonrails/ai-jail`, `wangyedev/sandbox-runtime-rs`, `Rani367/Skarn`, `kernex-dev/kernex`,
`souk4711/hakoniwa`, `santhreal/procjail`, `phylum-dev/birdcage`, `landlock-lsm/rust-landlock`,
`superradcompany/microsandbox`, `everruns/bashkit`, `openai/codex` (`codex-rs/Cargo.toml`,
`codex-rs/sandboxing/src/`), `containers/bubblewrap`, `google/nsjail`, `netblue30/firejail`,
`apple/container`, `apple/containerization`; `LICENSE` files for `nolabs-ai/nono`,
`afshinm/zerobox`, `m62624/cageforge`, `santhreal/procjail`, `phylum-dev/birdcage`,
`wangyedev/sandbox-runtime-rs`.

**Primary — GitHub HTML payload** (`"stargazerCount"`, `"spdxId"`): all repos in the tables above.

**Primary — GitHub issue**: `apple/containerization#737` — "Clarify `sandbox-exec` deprecation
timeline and provide a replacement for non-App-Store process sandboxing", opened 2026-05-12, last
updated 2026-09-13.

**Documentation**: `landlock.io` (ABI history, `news/`),
`nono.sh/docs/cli/internals/{landlock,seatbelt}`, `code.claude.com/docs/en/sandboxing`,
`gvisor.dev/docs/`, `github.com/anthropics/sandbox-runtime` README.

**Secondary — used for context only, not for factual claims in the tables**:
`restyler/awesome-sandbox`, the `wincent/2752d8d97727577050c043e4ff9e386e` gist "List of coding
agent sandboxes 2026-05", `tanayshah.dev/blog/choosing-agent-sandbox-2026`,
`bigiron.cc/guides/bubblewrap-vs-firejail-vs-nsjail-application-sandboxing`,
`zylos.ai/research/2026-09-01-unprivileged-userns-restrictions-container-agent-sandboxes/`,
`sandboxicon.com/tech/seatbelt/`, `hacktricks.wiki/en/macos-hardening/.../macos-sandbox/`.
