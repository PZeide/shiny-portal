use clap::Parser;

#[derive(Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// Enable verbose logging.
    #[arg(short, long, action = clap::ArgAction::SetTrue)]
    pub verbose: bool,
}
