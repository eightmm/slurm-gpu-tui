use clap::Parser;

#[derive(Parser)]
#[command(version=sgpu::VERSION, about="One-shot report of my failed/stopped Slurm allocations")]
struct Args {
    #[command(flatten)]
    options: sgpu::cli::FailureOptions,
}
fn main() {
    if let Err(error) = sgpu::cli::failures(Args::parse().options) {
        eprintln!("chkfail: {error}");
        std::process::exit(1);
    }
}
