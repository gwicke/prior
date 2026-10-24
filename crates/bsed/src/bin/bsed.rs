//! The `bsed` binary. It is the sole declaration of the `bsed` target in the
//! workspace — all the logic lives in the library so the tests can call exactly
//! the same code paths.

fn main() -> std::process::ExitCode {
    bsed::cli::main()
}
