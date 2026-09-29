//! The standalone `bsed` binary. All the logic lives in the library so picocode
//! can call exactly the same code paths.

fn main() -> std::process::ExitCode {
    bsed::cli::main()
}
