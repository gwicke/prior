//! The `bsed` shim binary.
//!
//! picocode shells out to `bsed preview` (inside a read-only sandbox) so the sed
//! engine is a *process* boundary as well as a policy one — see
//! `doc/safe_sed.md` §5.2. All the logic lives in the `safe_sed` crate, which is
//! independently installable; this file exists only so that
//! `cargo install --path .` puts the binary on `$PATH` alongside picocode.

fn main() -> std::process::ExitCode {
    safe_sed::cli::main()
}
