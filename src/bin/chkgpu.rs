use clap::Parser;
#[derive(Parser)]
#[command(version=sgpu::VERSION,about="One-shot Slurm allocation matrix; no compute-node requests")]
struct Args {
    #[arg(
        long,
        help = "Compact node capacity and pending-reason report instead of allocation matrices"
    )]
    summary: bool,
    #[arg(
        long,
        value_enum,
        default_value = "auto",
        help = "Color output; auto respects NO_COLOR and TERM=dumb"
    )]
    color: sgpu::quickview::ColorMode,
}
fn main() {
    let args = Args::parse();
    if let Err(error) = sgpu::quickview::run(args.color, args.summary) {
        eprintln!("chkgpu: {error}");
        std::process::exit(1);
    }
}
