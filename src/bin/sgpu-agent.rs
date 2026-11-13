use clap::Parser;
fn main() {
    if let Err(error) = sgpu::agent::run(sgpu::agent::Args::parse()) {
        eprintln!("sgpu-agent: {error}");
        std::process::exit(1);
    }
}
