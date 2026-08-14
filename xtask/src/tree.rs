use std::fs::File;
use std::io::Write;
use std::{
    fs::{self, read_to_string},
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use indicatif::{ProgressBar, ProgressStyle};
use java_syntax::{Parser, lex};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use walkdir::WalkDir;

use crate::args::ParseLanguage;

pub fn render_tree(lang: ParseLanguage, file_path: PathBuf) -> anyhow::Result<()> {
    let content = match read_to_string(file_path) {
        Ok(content) => content,
        Err(e) => {
            anyhow::bail!("Failed to read file: {e:#}");
        }
    };
    match lang {
        ParseLanguage::Java => {
            render_java_tree(content)?;
        }
    }

    Ok(())
}

pub fn render_java_tree(content: String) -> anyhow::Result<()> {
    let tokens = lex(&content).0;

    let parse = Parser::new(tokens).parse();
    let res = parse.debug_dump();
    println!("{res}");

    if !parse.errors().is_empty() {
        println!();
        println!("Parsing errors:");
        for err in parse.errors() {
            println!("\t{err:?}");
        }
        Err(anyhow::anyhow!("parsing errors occurred"))
    } else {
        Ok(())
    }
}

pub struct BatchConfig {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
}

pub fn run_batch_parse(config: BatchConfig) -> anyhow::Result<()> {
    let files: Vec<PathBuf> = WalkDir::new(&config.input_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "java"))
        .map(|e| e.path().to_path_buf())
        .collect();

    let total_files = files.len();
    if total_files == 0 {
        println!("No files found");
        return Ok(());
    }

    let pb = ProgressBar::new(total_files as u64);
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")?
        .progress_chars("#>-"));

    if !config.output_dir.exists() {
        fs::create_dir_all(&config.output_dir)?;
    }

    files.par_iter().for_each(|file_path| {
        pb.set_message(format!(
            "Parsing: {}",
            file_path.file_name().unwrap().to_string_lossy()
        ));

        if let Err(e) = process_with_timeout(file_path.clone(), config.output_dir.clone())
            && e.to_string() == "Parsing timeout"
        {
            pb.println(format!("Timeout: {:?}", file_path));
        }

        pb.inc(1);
    });

    pb.finish_with_message("Done!");
    Ok(())
}

fn process_with_timeout(input_path: PathBuf, output_root: PathBuf) -> anyhow::Result<()> {
    let (sender, receiver) = mpsc::channel();

    let t_path = input_path.clone();
    let t_output = output_root.clone();

    std::thread::spawn(move || {
        let result = process_single_file(&t_path, &t_output);
        let _ = sender.send(result);
    });

    match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(res) => res,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let log_path = output_root.join("timeouts.log");
            let _ = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
                .map(|mut f| writeln!(f, "Timeout (2s): {:?}", input_path));
            Err(anyhow::anyhow!("Parsing timeout"))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!("Worker thread panicked")),
    }
}

fn process_single_file(input_path: &Path, output_root: &Path) -> anyhow::Result<()> {
    let content = fs::read_to_string(input_path)?;

    let tokens = lex(&content).0;

    let parse = Parser::new(tokens).parse();
    let errors = parse.errors();

    if !errors.is_empty() {
        let res = parse.debug_dump();

        let relative_path = input_path
            .to_string_lossy()
            .replace(|c: char| !c.is_alphanumeric(), "_");
        let mut output_path = output_root.to_path_buf();
        output_path.push(format!("{}.txt", relative_path));

        let mut f = File::open(&output_path)?;
        writeln!(f, "File: {input_path:?}\n\nSyntax tree:")?;
        write!(f, "{res}")?;
    }

    Ok(())
}
