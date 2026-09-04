//! Entry point of the `kernos` binary.

fn main() {
    std::process::exit(kernos::cli::run(std::env::args_os()));
}
