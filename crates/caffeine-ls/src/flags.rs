use std::path::PathBuf;

#[derive(clap::Parser)]
pub struct Flags {
    #[arg(long, default_value = "false")]
    pub wait_dbg: bool,

    #[arg(long)]
    pub log_file: Option<PathBuf>,
}
