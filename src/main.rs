use clap::Parser;
fn main() {
    sgpu::logs::helper_dispatch();
    let args = sgpu::cli::Args::parse_from(sgpu::cli::normalize(std::env::args().collect()));
    match sgpu::cli::run(args) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("sgpu: {error}");
            std::process::exit(1);
        }
    }
}
