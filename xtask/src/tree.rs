use std::{fs::read_to_string, path::PathBuf};

use java_syntax::{Parser, lex};

use crate::args::ParseLanguage;

pub fn render_tree(lang: ParseLanguage, file_path: PathBuf) -> anyhow::Result<()> {
    let content = match read_to_string(file_path) {
        Ok(content) => content,
        Err(e) => {
            anyhow::bail!("Failed to read file");
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
    let tokens = match lex(&content) {
        Ok(tokens) => tokens,
        Err((tokens, errors)) => {
            for err in errors {
                println!("Lexical error: {err:?}");
            }
            tokens
        }
    };

    let parse = Parser::new(tokens).parse();
    let res = parse.debug_dump();
    println!("{res}");

    if !parse.errors().is_empty() {
        Err(anyhow::anyhow!("parsing errors occurred"))
    } else {
        Ok(())
    }
}
