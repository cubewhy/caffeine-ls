use std::process;

use clap::Parser;

use crate::{
    args::{Cli, ParseLanguage},
    tree::run_batch_parse,
};

mod args;
mod prepare;
mod tree;

fn main() {
    let args = Cli::parse();

    match args {
        Cli::Prepare { target } => {
            prepare::prepare(target);
        }
        Cli::Parse { lang, file } => {
            let Some(lang) = lang.or_else(|| {
                probe_lang_by_extension(
                    file.extension()
                        .map(|os_str| os_str.to_string_lossy())
                        .as_deref(),
                )
            }) else {
                eprintln!("Unknown file type");
                process::exit(1);
            };
            if let Err(e) = tree::render_tree(lang, file) {
                eprintln!("An error has occurred: {e:#}");
                process::exit(2);
            }
        }

        Cli::BatchParse { input, output } => {
            let config = tree::BatchConfig {
                input_dir: input,
                output_dir: output,
            };

            if let Err(e) = run_batch_parse(config) {
                eprintln!("An error has occurred: {e:#}");
                process::exit(2);
            }
        }
    }
}

fn probe_lang_by_extension(extension: Option<&str>) -> Option<ParseLanguage> {
    match extension {
        Some("java") => Some(ParseLanguage::Java),
        _ => None,
    }
}
