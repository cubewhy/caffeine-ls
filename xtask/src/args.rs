use std::path::PathBuf;

#[derive(Debug, clap::Parser)]
pub enum Cli {
    Prepare {
        #[arg(value_enum)]
        target: Option<PrepareTarget>,
    },

    /// Parse source tree
    Parse {
        file: PathBuf,

        #[arg(long, short)]
        lang: Option<ParseLanguage>,
    },

    BatchParse {
        input: PathBuf,

        #[arg(long, short)]
        output: PathBuf,
    },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum ParseLanguage {
    Java,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum PrepareTarget {
    Cfr,
    Vineflower,
}
