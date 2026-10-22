//! The seccomp filter bwrap installs on the payload (R3d).
//!
//! The network namespace already makes the abstract X11 socket and host
//! `127.0.0.1` unreachable by construction, so this is **not** the X11 fix. It
//! closes the two remaining holes that route around `connect()` and therefore
//! around both the namespace and the SOCKS bridge:
//!
//! - **`socket(AF_VSOCK, …)`** — VM sockets are addressed by CID, not by the
//!   network namespace, so the netns does not fence them. `AF_VSOCK` is only
//!   meaningful inside a VM; a desktop sandbox never needs it.
//! - **`io_uring_setup` / `io_uring_enter` / `io_uring_register`** — io_uring
//!   constructs and connects sockets in kernel context (`IORING_OP_SOCKET`,
//!   `IORING_OP_CONNECT`), bypassing a `connect()`-based rule and the socket
//!   path's netns check. Nothing in a shell tool needs io_uring.
//!
//! Everything else is allowed. In particular `AF_UNIX` stays open: the netns
//! already removed the abstract X11 socket, and both the bridge's own plumbing
//! and `agent-browser`'s control socket are Unix sockets. `srt` denies
//! `AF_UNIX` wholesale because it has no bridge; prior does not need to.
//!
//! # Why a hand-written classic-BPF program
//!
//! `bwrap --seccomp FD` takes a raw `struct sock_filter[]`, not a libseccomp
//! program, and the filter is three rules and one argument test. Depending on
//! `libseccomp` (a C library plus a build-time pkg-config) would buy nothing
//! over a dozen instructions whose jump targets a test checks. The program is
//! assembled here and serialised in the layout the kernel's `seccomp(2)` expects,
//! which is also the layout `bwrap` reads.

use std::io;

/// `AUDIT_ARCH_*` values are not exposed by `libc`, so the supported ones are
/// named here. The filter kills the payload on any other architecture rather
/// than letting a mismatched program through.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_NATIVE: u32 = 0xc000_003e; // AUDIT_ARCH_X86_64
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH_NATIVE: u32 = 0xc000_00b7; // AUDIT_ARCH_AARCH64

/// Syscall numbers the filter names, pinned as `u32` because they are BPF
/// immediate fields. They are identical on x86_64 and aarch64, the two
/// architectures the build targets.
const SYS_SOCKET: u32 = 41;
const SYS_IO_URING_SETUP: u32 = 425;
const SYS_IO_URING_ENTER: u32 = 426;
const SYS_IO_URING_REGISTER: u32 = 427;

/// Errno the filter returns for a denied call: `EPERM`, matching what the kernel
/// reports for a socket family that is not compiled in — so a denied call looks
/// like an ordinary "not supported here", not like a sandbox probe.
const DENY_ERRNO: u32 = libc::EPERM as u32;

/// One classic-BPF instruction, in the kernel's `struct sock_filter` layout
/// (`u16 code; u8 jt; u8 jf; u32 k;`, 8 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

const BPF_LD_W_ABS: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
const BPF_JEQ_K: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
const BPF_RET_K: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

const fn ld_abs(offset: usize) -> Insn {
    Insn {
        code: BPF_LD_W_ABS,
        jt: 0,
        jf: 0,
        k: offset as u32,
    }
}

const fn jeq(k: u32, jt: u8, jf: u8) -> Insn {
    Insn {
        code: BPF_JEQ_K,
        jt,
        jf,
        k,
    }
}

const fn ret(k: u32) -> Insn {
    Insn {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k,
    }
}

/// The terminal indices the layout below refers to.
const DENY: usize = 10;
const ALLOW: usize = 9;
const KILL: usize = 11;

/// The program.
///
/// ```text
/// 0  load arch
/// 1  arch != native          -> kill
/// 2  load nr
/// 3  nr == io_uring_setup    -> deny
/// 4  nr == io_uring_enter    -> deny
/// 5  nr == io_uring_register -> deny
/// 6  nr != socket            -> allow
/// 7  load args[0]            (the socket domain)
/// 8  domain == AF_VSOCK      -> deny, otherwise -> allow
/// 9  allow
/// 10 deny (EPERM)
/// 11 kill (wrong architecture)
/// ```
///
/// `jt`/`jf` are offsets from the *next* instruction, so a target `to` from
/// instruction `from` is `to - (from + 1)`. All jumps go forward.
fn program() -> [Insn; 12] {
    // `rel(from, to)` is the forward distance from instruction `from` to `to`.
    const fn rel(from: usize, to: usize) -> u8 {
        assert!(to > from, "BPF only jumps forward");
        (to - (from + 1)) as u8
    }

    [
        // 0: A = arch
        ld_abs(std::mem::offset_of!(libc::seccomp_data, arch)),
        // 1: arch == native -> fall through (0); otherwise -> kill
        jeq(AUDIT_ARCH_NATIVE, 0, rel(1, KILL)),
        // 2: A = nr
        ld_abs(std::mem::offset_of!(libc::seccomp_data, nr)),
        // 3-5: the three io_uring calls all deny
        jeq(SYS_IO_URING_SETUP, rel(3, DENY), 0),
        jeq(SYS_IO_URING_ENTER, rel(4, DENY), 0),
        jeq(SYS_IO_URING_REGISTER, rel(5, DENY), 0),
        // 6: not socket -> allow; socket -> fall through to the domain test
        jeq(SYS_SOCKET, 0, rel(6, ALLOW)),
        // 7: A = args[0] (domain). Only the low 32 bits are loaded; a domain is
        //    a small positive constant, so the high half is always zero.
        ld_abs(std::mem::offset_of!(libc::seccomp_data, args)),
        // 8: AF_VSOCK -> deny; any other family -> allow
        jeq(libc::AF_VSOCK as u32, rel(8, DENY), rel(8, ALLOW)),
        // 9: allow
        ret(libc::SECCOMP_RET_ALLOW),
        // 10: deny with EPERM
        ret(libc::SECCOMP_RET_ERRNO | (DENY_ERRNO & libc::SECCOMP_RET_DATA)),
        // 11: kill the whole process on a foreign architecture
        ret(libc::SECCOMP_RET_KILL_PROCESS),
    ]
}

/// The filter as bytes in the kernel's `struct sock_filter` layout: little-endian
/// `u16 code`, `u8 jt`, `u8 jf`, `u32 k`, eight bytes each. This is exactly what
/// `bwrap --seccomp FD` reads.
pub fn filter_bytes() -> Vec<u8> {
    let prog = program();
    let mut out = Vec::with_capacity(prog.len() * 8);
    for insn in prog {
        out.extend_from_slice(&insn.code.to_le_bytes());
        out.push(insn.jt);
        out.push(insn.jf);
        out.extend_from_slice(&insn.k.to_le_bytes());
    }
    out
}

/// Create a fresh, inheritable memfd carrying [`filter_bytes`] and return its raw
/// fd, positioned at offset 0.
///
/// A **fresh** fd per spawn is required, not one shared instance: `bwrap` reads
/// the `--seccomp` fd sequentially to EOF, so a shared descriptor is at EOF on
/// the second spawn and `prctl(SECCOMP_SET_MODE_FILTER)` then fails with
/// `EINVAL` (measured: the first spawn succeeds, the second and every one after
/// fails). A memfd also keeps the rules out of the filesystem, so no path
/// binding can re-expose them.
///
/// The returned fd has `CLOEXEC` cleared so it survives `execve` into bwrap. The
/// caller passes it as a `keep` fd to the R8 seal and is responsible for closing
/// it once the child has inherited it.
pub fn rules_fd() -> io::Result<std::os::fd::RawFd> {
    let bytes = filter_bytes();
    // SAFETY: a static NUL-terminated name and no flags returns a fresh
    // anonymous file descriptor.
    let fd = unsafe { libc::memfd_create(c"prior-seccomp".as_ptr(), 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Write the whole program: a short write would truncate the filter.
    let mut written = 0;
    while written < bytes.len() {
        // SAFETY: `fd` is the memfd just created; the pointer and length are in
        // bounds of `bytes`.
        let n = unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if n < 0 {
            let e = io::Error::last_os_error();
            // SAFETY: we own `fd`; close it on the error path.
            unsafe { libc::close(fd) };
            return Err(e);
        }
        written += n as usize;
    }
    // Rewind so bwrap reads from the start.
    // SAFETY: `fd` is a valid memfd we own.
    if unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } < 0 {
        let e = io::Error::last_os_error();
        // SAFETY: as above.
        unsafe { libc::close(fd) };
        return Err(e);
    }
    // Clear CLOEXEC so the fd survives the exec into bwrap. R8's seal keeps it
    // explicitly via `keep`; this is the same contract as the egress fd.
    // SAFETY: `fd` is valid; F_SETFD/FD_CLOEXEC is the documented interface.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        let e = io::Error::last_os_error();
        // SAFETY: as above.
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode the serialised bytes back into instructions, mirroring how the
    /// kernel reads them, so the on-wire layout is asserted, not just the
    /// in-memory vector.
    fn decode(bytes: &[u8]) -> Vec<Insn> {
        assert_eq!(bytes.len() % 8, 0, "filter length must be a multiple of 8");
        bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| Insn {
                code: u16::from_le_bytes([c[0], c[1]]),
                jt: c[2],
                jf: c[3],
                k: u32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            })
            .collect()
    }

    /// Walk the program from instruction 0 for a given syscall and (for
    /// `socket`) domain, following `jt`/`jf`, and return the terminal `k` (the
    /// seccomp return value). The load instructions' values are supplied by the
    /// fixed sequence rather than a stack, which is enough for this program.
    fn verdict(nr: u32, domain: Option<u32>) -> u32 {
        let prog = program();
        let mut pc = 0usize;
        for _ in 0..prog.len() {
            let insn = prog[pc];
            if insn.code == BPF_LD_W_ABS {
                pc += 1;
                continue;
            }
            if insn.code == BPF_JEQ_K {
                // The compare at each index sees a fixed value: [1]=arch,
                // [3..=6]=nr, [8]=domain.
                let actual = match pc {
                    1 => AUDIT_ARCH_NATIVE,
                    3..=6 => nr,
                    8 => domain.unwrap_or(0),
                    other => panic!("unexpected JEQ at {other}"),
                };
                let delta = if insn.k == actual { insn.jt } else { insn.jf } as usize;
                pc += 1 + delta;
                continue;
            }
            assert_eq!(insn.code, BPF_RET_K, "unexpected opcode at {pc}");
            return insn.k;
        }
        panic!("filter did not terminate");
    }

    fn deny_value() -> u32 {
        libc::SECCOMP_RET_ERRNO | (DENY_ERRNO & libc::SECCOMP_RET_DATA)
    }

    #[test]
    fn serialised_layout_is_eight_bytes_per_instruction() {
        let prog = program();
        let bytes = filter_bytes();
        assert_eq!(bytes.len(), prog.len() * 8);
        assert_eq!(
            decode(&bytes),
            prog.to_vec(),
            "round-trip must preserve the program"
        );
    }

    #[test]
    fn io_uring_calls_are_denied_with_eperm() {
        for nr in [
            SYS_IO_URING_SETUP,
            SYS_IO_URING_ENTER,
            SYS_IO_URING_REGISTER,
        ] {
            assert_eq!(
                verdict(nr, None),
                deny_value(),
                "syscall {nr} must be denied"
            );
        }
    }

    #[test]
    fn vsock_socket_is_denied_but_other_families_are_allowed() {
        assert_eq!(
            verdict(SYS_SOCKET, Some(libc::AF_VSOCK as u32)),
            deny_value(),
            "socket(AF_VSOCK) must be denied"
        );
        // The families that must keep working: the bridge, agent-browser and
        // ordinary network clients all rely on these.
        for fam in [
            libc::AF_UNIX as u32,
            libc::AF_INET as u32,
            libc::AF_INET6 as u32,
        ] {
            assert_eq!(
                verdict(SYS_SOCKET, Some(fam)),
                libc::SECCOMP_RET_ALLOW,
                "socket(family {fam}) must be allowed"
            );
        }
    }

    #[test]
    fn unrelated_syscalls_are_allowed() {
        // read, write, close, openat — nothing the filter names.
        for nr in [0u32, 1, 3, 257] {
            assert_eq!(verdict(nr, None), libc::SECCOMP_RET_ALLOW);
        }
    }

    /// Every jump must land inside the program. The kernel would reject an
    /// out-of-range `jt`/`jf` at load time, but the assembler should never
    /// produce one.
    #[test]
    fn every_jump_target_is_in_bounds() {
        let prog = program();
        for (i, insn) in prog.iter().enumerate() {
            if insn.code != BPF_JEQ_K {
                continue;
            }
            for delta in [insn.jt, insn.jf] {
                let target = i + 1 + delta as usize;
                assert!(
                    target < prog.len(),
                    "instruction {i} jumps to {target}, past end {}",
                    prog.len()
                );
            }
        }
    }

    #[test]
    fn rules_fd_holds_a_readable_program_at_offset_zero() {
        use std::io::Read;
        use std::os::fd::FromRawFd;

        let fd = rules_fd().expect("memfd");
        // SAFETY: `rules_fd` returns a fresh owned descriptor; take ownership so
        // the File closes it exactly once.
        let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, filter_bytes(), "the memfd must hold the program");
    }
}
