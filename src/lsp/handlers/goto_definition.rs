use std::sync::Arc;

use crate::completion::context::CursorLocation;
use crate::completion::engine::ContextEnricher;
use crate::completion::type_resolver::symbol_resolver::{ResolvedSymbol, SymbolResolver};
use crate::index::{ClassOrigin, GlobalIndex};
use crate::language::java::class_parser::find_symbol_range;
use crate::lsp::server::{Backend, language_id_from_uri};
use tower_lsp::lsp_types::*;
use tracing::instrument;

#[instrument(skip(backend, params), fields(uri = %params.text_document_position_params.text_document.uri))]
pub async fn handle_goto_definition(
    backend: &Backend,
    params: GotoDefinitionParams,
) -> Option<GotoDefinitionResponse> {
    let uri = &params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    let doc = backend.workspace.documents.get(uri)?;
    let lang = backend.registry.find(language_id_from_uri(uri))?;
    let full_end = token_end_character(&doc.content, pos.line, pos.character);

    let index_guard = backend.workspace.index.read().await;
    let mut ctx = lang.parse_completion_context(&doc.content, pos.line, full_end, None)?;

    // enrich context
    ContextEnricher::new(&index_guard).enrich(&mut ctx);

    tracing::debug!(
        location = ?ctx.location,
        enclosing_class = ?ctx.enclosing_class,
        enclosing_internal = ?ctx.enclosing_internal_name,
        locals = ?ctx.local_variables,
        "goto: parsed context"
    );

    // ── 局部变量 / 参数跳转（在符号解析之前处理）─────────────────────────────
    // Expression 或 MethodArgument 中，如果 token 与某个局部变量名匹配，
    // 跳到当前文件中的声明处，不走 index。
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
        let range = find_local_var_decl(&doc.content, lv.name.as_ref());
        return Some(GotoDefinitionResponse::Scalar(Location {
            uri: uri.clone(),
            range: range.unwrap_or_default(),
        }));
    }

    if let CursorLocation::Import { prefix } = &ctx.location {
        let raw = prefix.trim().trim_end_matches(".*").trim();
        let internal = raw.replace('.', "/");
        if index_guard.get_class(&internal).is_some() {
            return goto_resolved_symbol(
                backend,
                &index_guard,
                ResolvedSymbol::Class(Arc::from(internal)),
            )
            .await;
        }
        return None;
    }

    // Index 符号解析
    let resolver = SymbolResolver::new(&index_guard);
    let symbol = match resolver.resolve(&ctx) {
        Some(s) => s,
        None => {
            tracing::debug!(location = ?ctx.location, "goto: resolver returned None");
            return None;
        }
    };
    tracing::debug!(symbol = ?symbol, "goto: resolved symbol");

    return goto_resolved_symbol(backend, &index_guard, symbol).await;
}

async fn goto_resolved_symbol(
    backend: &Backend,
    index_guard: &GlobalIndex,
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
            Some(Arc::clone(&summary.descriptor)),
            DeclKind::Method,
        ),
        ResolvedSymbol::Field { owner, summary } => (
            Arc::clone(owner),
            Some(Arc::clone(&summary.name)),
            None,
            DeclKind::Field,
        ),
    };

    let meta = match index_guard.get_class(&target_internal) {
        Some(m) => m,
        None => {
            tracing::debug!(internal = %target_internal, "goto: class not in index");
            return None;
        }
    };

    match &meta.origin {
        ClassOrigin::SourceFile(uri_str) => {
            let target_uri = match Url::parse(uri_str) {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!(uri = %uri_str, error = %e, "goto: invalid source URI");
                    return None;
                }
            };
            let range = member_name.as_ref().and_then(|name| {
                let content = backend
                    .workspace
                    .documents
                    .get(&target_uri)
                    .map(|d| d.content.to_string())
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
                    index_guard,
                )
                .or_else(|| find_declaration_range(&content, name, decl_kind))
            });
            Some(GotoDefinitionResponse::Scalar(Location {
                uri: target_uri,
                range: range.unwrap_or_default(),
            }))
        }

        ClassOrigin::Jar(jar_path) => {
            let bytes = match extract_class_bytes(jar_path, &target_internal) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, class = %target_internal, "goto: failed to read class bytes");
                    return None;
                }
            };
            let cache_path = backend.decompiler_cache.resolve(&target_internal, &bytes);

            if !cache_path.exists() {
                tracing::info!(class = %target_internal, "goto: cache miss, decompiling");
                let config = backend.config.read().await;
                let decompiler_jar = match &config.decompiler_path {
                    Some(p) => p.clone(),
                    None => {
                        tracing::warn!(
                            class = %target_internal,
                            "goto: decompiler_path not configured"
                        );
                        return None;
                    }
                };
                let java_bin = config.get_java_bin();
                let decompiler = config.decompiler_type.get_decompiler();
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
                    index_guard,
                )
                .or_else(|| find_declaration_range(&content, name, decl_kind))
            });
            let target_uri = match Url::from_file_path(&cache_path) {
                Ok(u) => u,
                Err(_) => {
                    tracing::warn!(path = ?cache_path, "goto: invalid cache path");
                    return None;
                }
            };
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
                    index_guard,
                )
                .or_else(|| find_declaration_range(&content, name, decl_kind))
            });

            let target_uri = match Url::from_file_path(&cache_path) {
                Ok(u) => u,
                Err(_) => return None,
            };

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

/// `class NAME` / `interface NAME` / `enum NAME`
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

/// 方法声明：行含修饰符/返回类型，且 `name(` 作为整词出现。
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
        // name 后（跳过空格）必须是 '('
        if before_ok && line[after_pos..].trim_start().starts_with('(') {
            return Some(abs);
        }
        start = abs + 1;
    }
}

/// 字段声明：行含修饰符/类型，且 `name` 不跟 '('。
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

/// 局部变量 / 参数声明：`Type name` 模式，name 不跟 '('，name 前是类型 token。
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

/// 单行内检测 `Type varName` 模式。
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
            // 跟 '(' → 是方法调用，跳过
            if line[after_pos..].trim_start().starts_with('(') {
                start = abs + 1;
                continue;
            }
            // 前面最后一个非空字符必须是类型 token 的末尾
            let before = line[..abs].trim_end();
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

/// 光标位置向右扩展到当前 token 末尾（UTF-16）。
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
