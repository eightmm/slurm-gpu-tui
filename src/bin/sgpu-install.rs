use clap::Parser;
fn main() {
    match sgpu::packaging::run(sgpu::packaging::Args::parse()) {
        Ok(()) => (),
        Err(error) => {
            eprintln!("sgpu-install: {error}");
            std::process::exit(1);
        }
    }
}
