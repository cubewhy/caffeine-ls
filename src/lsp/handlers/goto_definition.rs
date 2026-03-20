use std::sync::Arc;

use crate::index::{ClassOrigin, IndexView};
use crate::language::java::class_parser::find_symbol_range;
use crate::lsp::server::Backend;
use crate::salsa_queries::conversion::{FromSalsaDataWithAnalysis, RequestAnalysisState};
use crate::semantic::context::CursorLocation;
use crate::semantic::types::symbol_resolver::{ResolvedSymbol, SymbolResolver};
use tower_lsp::lsp_types::*;
use tracing::instrument;

#[instrument(skip(backend, params), fields(uri = %params.text_document_position_params.text_document.uri))]
pub async fn handle_goto_definition(
    backend: &Backend,
    params: GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let uri = &params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    let (lang_id, full_end) = backend.workspace.documents.with_doc(uri, |doc| {
        let full_end = token_end_character(doc.source().text(), pos.line, pos.character);
        Some((doc.language_id().to_owned(), full_end))
    })??;

    let lang = backend.registry.find(&lang_id)?;

    backend.workspace.documents.with_doc_mut(uri, |doc| {
        if doc.source().tree.is_none() {
            let tree = lang.parse_tree(doc.source().text(), None);
            doc.set_tree(tree);
        }
    })?;

    let analysis = backend.workspace.analysis_context_for_uri(uri);
    let scope = analysis.scope();

    let request_analysis_t0 = std::time::Instant::now();

    // Use cached IndexView and NameTable via Salsa for better performance
    let (view, name_table) = {
        let db = backend.workspace.salsa_db.lock();

        // Get cached IndexView (memoized)
        let view = crate::salsa_queries::get_index_view_for_context(
            &*db,
            scope.module,
            analysis.classpath,
            analysis.source_root,
        );

        // Get cached NameTable (memoized)
        let name_table = crate::salsa_queries::get_name_table_for_context(
            &*db,
            scope.module,
            analysis.classpath,
            analysis.source_root,
        );

        (view, name_table)
    };

    let request_analysis = RequestAnalysisState {
        analysis,
        name_table: Arc::clone(&name_table),
    };

    tracing::debug!(
        uri = %uri,
        module = scope.module.0,
        classpath = ?analysis.classpath,
        source_root = ?analysis.source_root.map(|id| id.0),
        view_layers = view.layer_count(),
        name_table_len = name_table.len(),
        analysis_bundle_ms = request_analysis_t0.elapsed().as_secs_f64() * 1000.0,
        "goto: request analysis prepared"
    );

    let salsa_file = backend.workspace.get_or_update_salsa_file(uri)?;

    let mut ctx = {
        let context_data = {
            let db = backend.workspace.salsa_db.lock();
            if lang.id() == "java" {
                crate::salsa_queries::java::extract_java_completion_context_with_name_table(
                    &*db,
                    salsa_file,
                    pos.line,
                    full_end,
                    None,
                    Some(Arc::clone(&request_analysis.name_table)),
                )
            } else {
                lang.extract_completion_context_salsa(&*db, salsa_file, pos.line, full_end, None)?
            }
        };

        let db = backend.workspace.salsa_db.lock();
        crate::semantic::SemanticContext::from_salsa_data_with_analysis(
            context_data.as_ref().clone(),
            &*db,
            salsa_file,
            Some(&*backend.workspace),
            Some(&request_analysis),
        )
    };

    // enrich context
    lang.enrich_completion_context(&mut ctx, scope, &view);

    tracing::debug!(
        module = scope.module.0,
        classpath = ?analysis.classpath,
        source_root = ?analysis.source_root.map(|id| id.0),
        location = ?ctx.location,
        enclosing_class = ?ctx.enclosing_class,
        enclosing_internal = ?ctx.enclosing_internal_name,
        locals = ?ctx.local_variables,
        "goto: parsed context"
    );

    // ── 局部变量 / 参数跳转（在符号解析之前处理）─────────────────────────────
    let local_token: Option<&str> = match &ctx.location {
        CursorLocation::Expression { prefix } if !prefix.is_empty() => Some(prefix.as_str()),
        CursorLocation::MethodArgument { prefix } if !prefix.is_empty() => Some(prefix.as_str()),
        _ => None,
    };

    if let Some(token) = local_token
        && let Some(lv) = ctx
            .local_variables
            .iter()
            .find(|v| v.name.as_ref() == token)
    {
        tracing::debug!(token = %token, "goto: local variable jump");

        let range = backend.workspace.documents.with_doc(uri, |doc| {
            find_local_var_decl(doc.source().text(), lv.name.as_ref())
        });

        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: range.flatten().unwrap_or_default(),
        }));
    }

    if let CursorLocation::Import { prefix } = &ctx.location {
        let raw = prefix.trim().trim_end_matches(".*").trim();
        let internal = raw.replace('.', "/");
        if view.get_class(&internal).is_some() {
            return goto_resolved_symbol(
                backend,
                &view,
                ResolvedSymbol::Class(Arc::from(internal)),
            )
            .await;
        }
        return None;
    }

    // Index 符号解析
    let resolver = SymbolResolver::new(&view);
    let symbol = match resolver.resolve(&ctx) {
        Some(s) => s,
        None => {
            tracing::debug!(location = ?ctx.location, "goto: resolver returned None");
            return None;
        }
    };
    tracing::debug!(symbol = ?symbol, "goto: resolved symbol");

    goto_resolved_symbol(backend, &view, symbol).await
}

async fn goto_resolved_symbol(
    backend: &Backend,
    view: &IndexView,
    symbol: ResolvedSymbol,
) -> Option<GotoDefinitionResponse> {
    let (target_internal, member_name, descriptor, decl_kind) = match &symbol {
        ResolvedSymbol::Class(name) => {
            let simple_name = name.rsplit('/').next().unwrap_or(name.as_ref());
            (
                Arc::clone(name),
                Some(Arc::from(simple_name)),
                None,
                DeclKind::Type,
            )
        }
        ResolvedSymbol::Method { owner, summary } => (
            Arc::clone(owner),
            Some(Arc::clone(&summary.name)),
            Some(summary.desc()),
            DeclKind::Method,
        ),
        ResolvedSymbol::Field { owner, summary } => (
            Arc::clone(owner),
            Some(Arc::clone(&summary.name)),
            None,
            DeclKind::Field,
        ),
    };

    let meta = view.get_class(&target_internal)?;
    match &meta.origin {
        ClassOrigin::SourceFile(uri_str) => {
            let target_uri = Url::parse(uri_str).ok()?;

            let range = member_name.as_ref().and_then(|name| {
                let content = backend
                    .workspace
                    .documents
                    .with_doc(&target_uri, |d| d.source().text().to_owned())
                    .or_else(|| {
                        target_uri
                            .to_file_path()
                            .ok()
                            .and_then(|p| std::fs::read_to_string(p).ok())
                    })?;

                find_symbol_range(
                    &content,
                    &target_internal,
                    Some(name),
                    descriptor.as_deref(),
                    view,
                )
                .or_else(|| find_declaration_range(&content, name, decl_kind))
            });

            Some(GotoDefinitionResponse::Scalar(Location {
                uri: target_uri,
                range: range.unwrap_or_default(),
            }))
        }

        ClassOrigin::Jar(jar_path) => {
            let bytes = extract_class_bytes(jar_path, &target_internal).ok()?;
            let cache_path = backend.decompiler_cache.resolve(&target_internal, &bytes);

            if !cache_path.exists() {
                tracing::info!(class = %target_internal, "goto: cache miss, decompiling");
                let config = backend.config.read().await;
                let decompiler_jar = config.decompiler_path.clone()?;
                let java_bin = config.get_java_bin();
                let decompiler = config.decompiler_backend.get_decompiler();
                drop(config);

                if let Err(e) = decompiler
                    .decompile(&java_bin, &decompiler_jar, &bytes, &cache_path)
                    .await
                {
                    tracing::error!(error = %e, class = %target_internal, "goto: decompile failed");
                    return None;
                }
                backend
                    .decompiler_cache
                    .cleanup_stale(&target_internal, &cache_path);
            }

            let range = member_name.as_ref().and_then(|name| {
                let content = std::fs::read_to_string(&cache_path).ok()?;
                find_symbol_range(
                    &content,
                    &target_internal,
                    Some(name),
                    descriptor.as_deref(),
                    view,
                )
                .or_else(|| find_declaration_range(&content, name, decl_kind))
            });

            let target_uri = Url::from_file_path(&cache_path).ok()?;
            Some(GotoDefinitionResponse::Scalar(Location {
                uri: target_uri,
                range: range.unwrap_or_default(),
            }))
        }

        ClassOrigin::ZipSource {
            zip_path,
            entry_name,
        } => {
            let base_cache = std::env::temp_dir().join("java_analyzer_sources");
            let cache_path = base_cache.join(entry_name.as_ref());

            if !cache_path.exists() {
                tracing::info!(entry = %entry_name, "goto: extracting zip source to cache");
                if let Some(parent) = cache_path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                if let Ok(file) = std::fs::File::open(zip_path.as_ref())
                    && let Ok(mut archive) = zip::ZipArchive::new(file)
                    && let Ok(mut entry) = archive.by_name(entry_name.as_ref())
                    && let Ok(mut out) = std::fs::File::create(&cache_path)
                {
                    std::io::copy(&mut entry, &mut out).ok();
                }
            }

            let range = member_name.as_ref().and_then(|name| {
                let content = std::fs::read_to_string(&cache_path).ok()?;
                find_symbol_range(
                    &content,
                    &target_internal,
                    Some(name),
                    descriptor.as_deref(),
                    view,
                )
                .or_else(|| find_declaration_range(&content, name, decl_kind))
            });

            let target_uri = Url::from_file_path(&cache_path).ok()?;
            Some(GotoDefinitionResponse::Scalar(Location {
                uri: target_uri,
                range: range.unwrap_or_default(),
            }))
        }

        ClassOrigin::Unknown => {
            tracing::debug!(class = %target_internal, "goto: unknown origin");
            None
        }
    }
}

// ── 声明类型 ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum DeclKind {
    Type,
    Method,
    Field,
}

// ── 声明位置查找 ───────────────────────────────────────────────────────────────

fn find_declaration_range(content: &str, name: &str, kind: DeclKind) -> Option<Range> {
    for (line_idx, line) in content.lines().enumerate() {
        let col = match kind {
            DeclKind::Type => find_type_decl(line, name),
            DeclKind::Method => find_method_decl(line, name),
            DeclKind::Field => find_field_decl(line, name),
        };
        if let Some(col) = col {
            return Some(Range {
                start: Position {
                    line: line_idx as u32,
                    character: col as u32,
                },
                end: Position {
                    line: line_idx as u32,
                    character: (col + name.len()) as u32,
                },
            });
        }
    }
    None
}

fn find_type_decl(line: &str, name: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let has_kw = ["class ", "interface ", "enum ", "@interface "]
        .iter()
        .any(|kw| trimmed.contains(kw));
    if !has_kw {
        return None;
    }
    let col = find_word_boundary(line, name)?;
    let before = line[..col].trim_end();
    ["class", "interface", "enum"]
        .iter()
        .any(|kw| before.ends_with(kw))
        .then_some(col)
}

fn find_method_decl(line: &str, name: &str) -> Option<usize> {
    if !line.contains(name) {
        return None;
    }
    let trimmed = line.trim_start();
    const HINTS: &[&str] = &[
        "public ",
        "private ",
        "protected ",
        "static ",
        "final ",
        "abstract ",
        "synchronized ",
        "native ",
        "void ",
        "int ",
        "long ",
        "double ",
        "float ",
        "boolean ",
        "byte ",
        "short ",
        "char ",
    ];
    if !HINTS.iter().any(|h| trimmed.contains(h)) {
        return None;
    }
    let lb = line.as_bytes();
    let wb = name.as_bytes();
    let mut start = 0;
    loop {
        let rel = line[start..].find(name)?;
        let abs = start + rel;
        let before_ok = abs == 0 || !is_ident_byte(lb[abs - 1]);
        let after_pos = abs + wb.len();
        if before_ok && line[after_pos..].trim_start().starts_with('(') {
            return Some(abs);
        }
        start = abs + 1;
    }
}

fn find_field_decl(line: &str, name: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    const HINTS: &[&str] = &[
        "public ",
        "private ",
        "protected ",
        "static ",
        "final ",
        "int ",
        "long ",
        "double ",
        "float ",
        "boolean ",
        "byte ",
        "short ",
        "char ",
        "String ",
        "Object ",
    ];
    if !HINTS.iter().any(|h| trimmed.contains(h)) {
        return None;
    }
    let col = find_word_boundary(line, name)?;
    let after = line[col + name.len()..].trim_start();
    if after.starts_with('(') {
        return None;
    }
    Some(col)
}

fn find_local_var_decl(content: &str, var_name: &str) -> Option<Range> {
    for (line_idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
            || trimmed.starts_with("import ")
            || trimmed.starts_with("package ")
            || trimmed.starts_with('@')
        {
            continue;
        }
        if let Some(col) = find_var_decl_col(line, var_name) {
            return Some(Range {
                start: Position {
                    line: line_idx as u32,
                    character: col as u32,
                },
                end: Position {
                    line: line_idx as u32,
                    character: (col + var_name.len()) as u32,
                },
            });
        }
    }
    None
}

fn find_var_decl_col(line: &str, var_name: &str) -> Option<usize> {
    let lb = line.as_bytes();
    let wb = var_name.as_bytes();
    let mut start = 0;
    loop {
        let rel = line[start..].find(var_name)?;
        let abs = start + rel;
        let before_ok = abs == 0 || !is_ident_byte(lb[abs - 1]);
        let after_pos = abs + wb.len();
        let after_ok = after_pos >= lb.len() || !is_ident_byte(lb[after_pos]);

        if before_ok && after_ok {
            if line[after_pos..].trim_start().starts_with('(') {
                start = abs + 1;
                continue;
            }
            let before = line[..abs].trim_end();
            if before.ends_with("...") {
                return Some(abs);
            }
            if let Some(&last) = before.as_bytes().last()
                && (last.is_ascii_alphanumeric() || last == b'>' || last == b']' || last == b'_')
            {
                return Some(abs);
            }
        }
        start = abs + 1;
    }
}

// ── 工具函数 ──────────────────────────────────────────────────────────────────

fn token_end_character(content: &str, line: u32, character: u32) -> u32 {
    let Some(line_str) = content.lines().nth(line as usize) else {
        return character;
    };
    let mut byte_offset = 0usize;
    let mut utf16_col = 0u32;
    for ch in line_str.chars() {
        if utf16_col >= character {
            break;
        }
        utf16_col += ch.len_utf16() as u32;
        byte_offset += ch.len_utf8();
    }
    let rest = &line_str[byte_offset..];
    if !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
        return character;
    }
    let mut end_utf16 = character;
    for ch in rest.chars() {
        if !(ch.is_alphanumeric() || ch == '_') {
            break;
        }
        end_utf16 += ch.len_utf16() as u32;
    }
    end_utf16
}

fn find_word_boundary(line: &str, word: &str) -> Option<usize> {
    let lb = line.as_bytes();
    let wb = word.as_bytes();
    let mut start = 0usize;
    loop {
        let rel = line[start..].find(word)?;
        let abs = start + rel;
        let before_ok = abs == 0 || !is_ident_byte(lb[abs - 1]);
        let after_ok = abs + wb.len() >= lb.len() || !is_ident_byte(lb[abs + wb.len()]);
        if before_ok && after_ok {
            return Some(abs);
        }
        start = abs + 1;
    }
}

#[inline]
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn extract_class_bytes(jar: &str, internal: &str) -> anyhow::Result<Vec<u8>> {
    let file = std::fs::File::open(jar)?;
    let mut zip = zip::ZipArchive::new(file)?;
    let entry_name = format!("{}.class", internal);
    let mut entry = zip.by_name(&entry_name)?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_var_decl_col_supports_varargs_parameter() {
        let line = "    public static void printNumbers(int... numbers) {";
        let col = find_var_decl_col(line, "numbers").expect("varargs parameter declaration");
        assert_eq!(col, line.find("numbers").unwrap());
    }

    #[test]
    fn test_find_var_decl_col_ignores_member_access_usage() {
        let line = "        System.out.println(numbers.length);";
        assert!(find_var_decl_col(line, "length").is_none());
    }
}
