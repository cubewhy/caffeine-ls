//! The `caffeine-ls parse` subcommand: parses a single source file, a folder
//! of source files or stdin and reports, per file, the detected language, the
//! syntax-tree node count, the full syntax-tree dump and any syntax errors.
//!
//! Language is detected by file extension (`.java` → Java, `.kt`/`.kts` →
//! Kotlin); for stdin — which has no extension — `--language` is required.
//!
//! Output formats:
//! - `text` (default): a human-readable report with the tree dump.
//! - `json`: one pretty-printed document with a `files` array.
//! - `jsonl`: one compact JSON object per parsed file on its own line, so
//!   tools can stream and aggregate results line by line (e.g. via `jq`).

use std::{
    fs::File,
    io::{self, BufWriter, IsTerminal, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::Serialize;
use syntax::{LanguageKind, SourceFile, SyntaxError};

use crate::{
    cli::{EXIT_CLEAN, EXIT_FINDINGS, report},
    flags::{ParseArgs, ParseLanguage, ParseOutputFormat},
};

pub fn run(args: &ParseArgs) -> anyhow::Result<i32> {
    let stdin = std::io::stdin();
    if args.path.is_none() && stdin.is_terminal() {
        anyhow::bail!(
            "no input path given and stdin is a terminal; pass a source file/folder \
             or pipe code in (with --language <java|kotlin> for stdin)"
        );
    }

    let mut reader = stdin.lock();
    let mut writer: Box<dyn Write> = match &args.output {
        Some(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            let file = File::create(path)
                .with_context(|| format!("failed to write report to {}", path.display()))?;
            Box::new(BufWriter::new(file))
        }
        None => Box::new(std::io::stdout().lock()),
    };

    run_impl(args, &mut reader, &mut *writer)
}

/// The read/write-agnostic implementation, so tests can drive it with any
/// reader/writer pair.
fn run_impl(
    args: &ParseArgs,
    reader: &mut dyn Read,
    writer: &mut dyn Write,
) -> anyhow::Result<i32> {
    let inputs = resolve_inputs(args)?;

    let mut files_parsed = 0usize;
    let mut syntax_errors = 0usize;
    let mut entries = Vec::new();

    for input in &inputs {
        let text = match &input.path {
            Some(path) => std::fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?,
            None => {
                let mut buf = String::new();
                reader
                    .read_to_string(&mut buf)
                    .context("failed to read stdin")?;
                buf
            }
        };

        let entry = parse_one(&input.display, input.language, &text);
        syntax_errors += entry.syntax_errors.len();
        files_parsed += 1;
        entries.push(entry);
    }

    match args.format {
        ParseOutputFormat::Text => render_text(&entries, files_parsed, syntax_errors, writer)?,
        ParseOutputFormat::Json => {
            let report = JsonReport {
                files_parsed,
                syntax_errors,
                files: entries,
            };
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| anyhow::format_err!("failed to serialize report: {e}"))?;
            writeln!(writer, "{json}")?;
        }
        ParseOutputFormat::Jsonl => {
            for entry in &entries {
                let json = serde_json::to_string(&entry)
                    .map_err(|e| anyhow::format_err!("failed to serialize entry: {e}"))?;
                writeln!(writer, "{json}")?;
                writer.flush()?;
            }
        }
    }
    writer.flush()?;

    Ok(if syntax_errors == 0 {
        EXIT_CLEAN
    } else {
        EXIT_FINDINGS
    })
}

/// A single unit of input to parse.
struct FileInput {
    path: Option<PathBuf>,
    display: String,
    language: LanguageKind,
}

/// Resolves the CLI inputs to a concrete list of source files and their
/// languages. Unknown-extension files pass the filter: single files bail,
/// folder walks skip them.
fn resolve_inputs(args: &ParseArgs) -> anyhow::Result<Vec<FileInput>> {
    let override_lang = args.language.map(|language| match language {
        ParseLanguage::Java => LanguageKind::Java,
        ParseLanguage::Kotlin => LanguageKind::Kotlin,
    });

    let stdin_input = || -> anyhow::Result<Vec<FileInput>> {
        let Some(lang) = override_lang else {
            anyhow::bail!(
                "stdin has no file extension to detect a language; \
                 re-run with --language <java|kotlin>"
            );
        };
        Ok(vec![FileInput {
            path: None,
            display: "stdin".to_string(),
            language: lang,
        }])
    };

    let Some(raw_path) = &args.path else {
        return stdin_input();
    };
    if raw_path.as_os_str() == "-" {
        return stdin_input();
    }

    let metadata = std::fs::metadata(raw_path)
        .with_context(|| format!("path {} does not exist", raw_path.display()))?;

    if metadata.is_file() {
        let display = raw_path.display().to_string();
        let Some(language) = detect_language(override_lang, &display) else {
            anyhow::bail!(
                "cannot determine the language of {} (expected a .java/.kt/.kts file); \
                 pass --language <java|kotlin>",
                raw_path.display()
            );
        };
        return Ok(vec![FileInput {
            path: Some(raw_path.clone()),
            display,
            language,
        }]);
    }

    anyhow::ensure!(
        metadata.is_dir(),
        "{} is neither a file nor a directory",
        raw_path.display()
    );

    let files = report::discover_files(raw_path);
    anyhow::ensure!(
        !files.is_empty(),
        "no supported source files (.java, .kt, .kts) found under {}",
        raw_path.display()
    );

    let inputs = files
        .into_iter()
        .filter_map(|file| {
            let display = file.display().to_string();
            detect_language(override_lang, &display).map(|language| FileInput {
                path: Some(file),
                display: report::display_path(raw_path, Path::new(&display)),
                language,
            })
        })
        .collect();

    Ok(inputs)
}

/// Applies the `--language` override if set, otherwise detects the language
/// from the path's extension. `None` for paths with a supported-language
/// extension under an override that cannot match, and for unknown extensions.
fn detect_language(override_lang: Option<LanguageKind>, path: &str) -> Option<LanguageKind> {
    let language = override_lang.unwrap_or_else(|| LanguageKind::from_path(path));
    (language != LanguageKind::Unknown).then_some(language)
}

/// One serializable parse result for a single source file.
#[derive(Debug, Serialize)]
struct ParseEntry {
    file: String,
    language: &'static str,
    /// Total number of syntax nodes *and* tokens in the tree.
    node_count: usize,
    tree: TreeNode,
    syntax_errors: Vec<SyntaxErrorEntry>,
}

#[derive(Debug, Serialize)]
struct JsonReport {
    files_parsed: usize,
    syntax_errors: usize,
    files: Vec<ParseEntry>,
}

/// A node (or token for `text: Some(..)`) of the syntax tree.
#[derive(Debug, Serialize)]
struct TreeNode {
    kind: String,
    /// Byte offsets `[start, end)` of the node in the source text.
    range: [u32; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<TreeNode>,
}

#[derive(Debug, Serialize)]
struct SyntaxErrorEntry {
    /// 1-based line of the error start.
    line: u32,
    /// 1-based column (in characters) of the error start.
    column: u32,
    /// Byte offsets `[start, end)` of the error in the source text.
    range: [u32; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    message: String,
}

fn parse_one(file: &str, language: LanguageKind, text: &str) -> ParseEntry {
    let parse = SourceFile::parse(language, text);
    let source = parse.syntax_node(language);

    let tree = match &source {
        SourceFile::Java(sf) => node_to_tree(&sf.syntax_node),
        SourceFile::Kotlin(sf) => node_to_tree(&sf.syntax_node),
    };

    let node_count = count_tree(&tree);
    let syntax_errors = parse
        .errors()
        .iter()
        .map(|err| syntax_error_entry(text, err))
        .collect();

    ParseEntry {
        file: file.to_string(),
        language: language_name(language),
        node_count,
        tree,
        syntax_errors,
    }
}

fn language_name(language: LanguageKind) -> &'static str {
    match language {
        LanguageKind::Java => "java",
        LanguageKind::Kotlin => "kotlin",
        LanguageKind::Unknown => "unknown",
    }
}

/// Replicates the language-agnostic shape of a `rowan` syntax-tree dump
/// (`{kind}@{start}..{end}`, leaking a short repr of token text), so the text
/// output matches what `xtask parse` prints.
fn tree_dump(tree: &TreeNode) -> String {
    let mut out = String::new();
    write_node(tree, 0, &mut out);
    out
}

fn write_node(node: &TreeNode, level: usize, out: &mut String) {
    for _ in 0..level {
        out.push_str("  ");
    }
    out.push_str(&node.kind);
    out.push('@');
    out.push_str(&node.range[0].to_string());
    out.push_str("..");
    out.push_str(&node.range[1].to_string());
    if let Some(text) = &node.text
        && text.len() < 25
    {
        out.push_str(&format!(" {text:?}"));
    }
    out.push('\n');
    for child in &node.children {
        write_node(child, level + 1, out);
    }
}

fn node_to_tree<L: rowan::Language>(node: &rowan::SyntaxNode<L>) -> TreeNode {
    let children = node
        .children_with_tokens()
        .map(|element| match element {
            rowan::NodeOrToken::Node(child) => node_to_tree(&child),
            rowan::NodeOrToken::Token(token) => TreeNode {
                kind: format!("{:?}", token.kind()),
                range: range_of(token.text_range()),
                text: Some(token.text().to_string()),
                children: Vec::new(),
            },
        })
        .collect();

    TreeNode {
        kind: format!("{:?}", node.kind()),
        range: range_of(node.text_range()),
        text: None,
        children,
    }
}

fn count_tree(tree: &TreeNode) -> usize {
    1 + tree.children.iter().map(count_tree).sum::<usize>()
}

fn range_of(range: rowan::TextRange) -> [u32; 2] {
    [u32::from(range.start()), u32::from(range.end())]
}

fn syntax_error_entry(text: &str, err: &SyntaxError) -> SyntaxErrorEntry {
    let start = u32::from(err.range.start());
    let (line, column) = line_column(text, start);
    SyntaxErrorEntry {
        line,
        column,
        range: [start, u32::from(err.range.end())],
        code: err.code.map(|code| code.to_string()),
        message: err.message.clone(),
    }
}

/// Converts a byte offset into a 1-based `(line, column)` pair, counting
/// columns in characters.
fn line_column(text: &str, byte_offset: u32) -> (u32, u32) {
    let text = &text[..(byte_offset as usize).min(text.len())];
    let lines = text.split('\n').count() as u32;
    let column = text
        .split('\n')
        .next_back()
        .map(|line| line.chars().count() as u32)
        .unwrap_or(0)
        + 1;
    (lines, column)
}

fn render_text(
    entries: &[ParseEntry],
    files_parsed: usize,
    syntax_errors: usize,
    writer: &mut dyn Write,
) -> io::Result<()> {
    for entry in entries {
        writeln!(writer, "=== {} ({}) ===", entry.file, entry.language)?;
        writeln!(writer, "node_count: {}", entry.node_count)?;
        write!(writer, "{}", tree_dump(&entry.tree))?;
        if entry.syntax_errors.is_empty() {
            writeln!(writer, "no syntax errors")?;
        } else {
            for err in &entry.syntax_errors {
                let code = err
                    .code
                    .as_deref()
                    .map(|c| format!("[{c}]"))
                    .unwrap_or_default();
                writeln!(
                    writer,
                    "{}:{}:{}: error{}: {}",
                    entry.file, err.line, err.column, code, err.message
                )?;
            }
        }
        writeln!(writer)?;
    }
    writeln!(
        writer,
        "summary: {files_parsed} file(s) parsed, {syntax_errors} syntax error(s)"
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::Value;

    use super::*;

    fn flags(
        path: Option<PathBuf>,
        language: Option<ParseLanguage>,
        format: ParseOutputFormat,
    ) -> ParseArgs {
        ParseArgs {
            path,
            language,
            format,
            output: None,
        }
    }

    fn run(args: &ParseArgs, input: &str) -> (i32, String) {
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut writer = Vec::new();
        let code = run_impl(args, &mut reader, &mut writer).expect("parse run failed");
        (code, String::from_utf8(writer).expect("non-UTF-8 output"))
    }

    #[test]
    fn parses_a_clean_java_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Hello.java");
        std::fs::write(&file, "class Hello {}\n").unwrap();

        let (code, out) = run(&flags(Some(file), None, ParseOutputFormat::Json), "");

        assert_eq!(code, EXIT_CLEAN);
        let report: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["files_parsed"], 1);
        assert!(
            report["files"][0]["file"]
                .as_str()
                .unwrap()
                .ends_with("Hello.java")
        );
        assert_eq!(report["files"][0]["language"], "java");
        assert!(report["files"][0]["node_count"].as_u64().unwrap() > 0);
        assert_eq!(
            report["files"][0]["syntax_errors"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn reports_syntax_errors_with_positions() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Broken.java");
        // An unterminated string is a guaranteed lexical error.
        std::fs::write(&file, "class Broken { void m() { String s = \"\n }\n").unwrap();

        let (code, out) = run(&flags(Some(file), None, ParseOutputFormat::Json), "");

        assert_eq!(code, EXIT_FINDINGS);
        let report: Value = serde_json::from_str(&out).unwrap();
        let errors = &report["files"][0]["syntax_errors"];
        let errors = errors.as_array().unwrap();
        assert!(!errors.is_empty());
        let first = &errors[0];
        assert!(first["line"].as_u64().unwrap() >= 1);
        assert!(first["column"].as_u64().unwrap() >= 1);
        assert_eq!(first["code"], "unterminated-string");
        assert!(!first["message"].as_str().unwrap().is_empty());
        assert_eq!(first["range"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn parses_kotlin_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.kt");
        std::fs::write(&file, "fun main() = println(\"hi\")\n").unwrap();

        let (code, out) = run(&flags(Some(file), None, ParseOutputFormat::Json), "");

        assert_eq!(code, EXIT_CLEAN, "{out}");
        let report: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["files"][0]["language"], "kotlin");
    }

    #[test]
    fn parses_stdin_with_explicit_language_as_jsonl() {
        let args = flags(None, Some(ParseLanguage::Kotlin), ParseOutputFormat::Jsonl);
        let (code, out) = run(&args, "fun main() = println(\"hi\")\n");

        assert!(code == EXIT_CLEAN || code == EXIT_FINDINGS, "{out}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1, "{out}");
        let entry: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry["file"], "stdin");
        assert_eq!(entry["language"], "kotlin");
    }

    #[test]
    fn parses_a_folder_skipping_unsupported_extensions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Hello.java"), "class Hello {}\n").unwrap();
        std::fs::write(dir.path().join("main.kt"), "fun main() {}\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not source code\n").unwrap();

        let (code, out) = run(
            &flags(
                Some(dir.path().to_path_buf()),
                None,
                ParseOutputFormat::Json,
            ),
            "",
        );

        assert_eq!(code, EXIT_CLEAN, "{out}");
        let report: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["files_parsed"], 2);
        let languages: Vec<&str> = report["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["language"].as_str().unwrap())
            .collect();
        assert!(languages.contains(&"java"));
        assert!(languages.contains(&"kotlin"));
    }

    #[test]
    fn stdin_without_a_language_fails() {
        let mut reader = Cursor::new(b"class A {}\n".to_vec());
        let mut writer = Vec::new();
        let err = run_impl(
            &flags(None, None, ParseOutputFormat::Text),
            &mut reader,
            &mut writer,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--language"));
    }

    #[test]
    fn single_file_with_language_override() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fragment.txt");
        std::fs::write(&file, "class A {}\n").unwrap();

        let (code, out) = run(
            &flags(
                Some(file),
                Some(ParseLanguage::Java),
                ParseOutputFormat::Json,
            ),
            "",
        );

        assert_eq!(code, EXIT_CLEAN, "{out}");
        let report: Value = serde_json::from_str(&out).unwrap();
        assert!(
            report["files"][0]["file"]
                .as_str()
                .unwrap()
                .ends_with("fragment.txt")
        );
        assert_eq!(report["files"][0]["language"], "java");
    }
}
