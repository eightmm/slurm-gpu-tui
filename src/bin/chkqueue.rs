use clap::Parser;

#[derive(Parser)]
#[command(version=sgpu::VERSION, about="One-shot pending Slurm jobs, numeric priority, wait and reasons")]
struct Args {
    #[command(flatten)]
    options: sgpu::cli::QueueOptions,
}
fn main() {
    if let Err(error) = sgpu::cli::queue(Args::parse().options) {
        eprintln!("chkqueue: {error}");
        std::process::exit(1);
    }
}
