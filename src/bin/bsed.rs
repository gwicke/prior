//! The `bsed` shim binary.
//!
//! prior shells out to `bsed preview` (inside a read-only sandbox) so the sed
//! engine is a *process* boundary as well as a policy one — see
//! `doc/bsed.md` §5.2. All the logic lives in the `bsed` crate, which is
//! independently installable; this file exists only so that
//! `cargo install --path .` puts the binary on `$PATH` alongside prior.

fn main() -> std::process::ExitCode {
    bsed::cli::main()
}
