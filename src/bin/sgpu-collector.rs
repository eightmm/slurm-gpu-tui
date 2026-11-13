use clap::Parser;
fn main() {
    sgpu::logs::helper_dispatch();
    if let Err(error) = sgpu::collector::run(sgpu::collector::Args::parse()) {
        eprintln!("sgpu-collector: {error}");
        std::process::exit(1);
    }
}
