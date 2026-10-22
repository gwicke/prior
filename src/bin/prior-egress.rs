//! The `prior-egress` shim binary (R3b).
//!
//! Runs **inside** the sandbox's network namespace as bwrap's payload. It binds
//! a listener on the namespace's loopback, hands the listening fd back to prior
//! over the inherited bootstrap socket, points the proxy variables at it, and
//! `exec`s the real command. See `prior::egress` for the mechanism and
//! `doc/261021_sandbox_env_x11_audit.md` (R3b) for why it is shaped this way.
//!
//! It lives in its own binary rather than an `sh -c` fragment so that the fd
//! plumbing survives a payload with no shell, and so it can be resolved by
//! absolute path exactly like `bsed`.

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    std::process::ExitCode::from(prior::egress::shim_main(&argv) as u8)
}
