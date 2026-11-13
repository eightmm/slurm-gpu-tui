use clap::Parser;
#[derive(Parser)]
#[command(version=sgpu::VERSION,about="One-shot Slurm allocation matrix; no compute-node requests")]
struct Args {}
fn main() {
    Args::parse();
    if let Err(error) = sgpu::quickview::run() {
        eprintln!("chkgpu: {error}");
        std::process::exit(1);
    }
}
