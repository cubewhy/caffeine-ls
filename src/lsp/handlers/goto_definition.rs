use std::sync::Arc;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use crate::index::{IndexView, NavigationDeclKind, NavigationSymbol, NavigationTarget, TypeRef};
use crate::language::LanguageRegistry;
use crate::language::java::class_parser::find_symbol_range;
use crate::language::java::module_info::{
    module_declaration_name_node, render_module_descriptor_source,
};
use crate::lsp::request_context::{PreparedRequest, RequestContext};
use crate::lsp::server::Backend;
use crate::semantic::context::CursorLocation;
use crate::semantic::types::symbol_resolver::{ResolvedSymbol, SymbolResolver};
use crate::workspace::{JavaModuleTarget, Workspace};
use tower_lsp::jsonrpc;
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tracing::instrument;

#[instrument(skip(backend, params), fields(uri = %params.text_document_position_params.text_document.uri))]
pub async fn handle_goto_definition(
    backend: &Backend,
    params: GotoDefinitionParams,
    request: Arc<RequestContext>,
) -> LspResult<Option<GotoDefinitionResponse>> {
    let task = tokio::task::spawn_blocking({
        let workspace = Arc::clone(&backend.workspace);
        let registry = Arc::clone(&backend.registry);
        let request = Arc::clone(&request);
        move || handle_goto_definition_blocking(workspace, registry, params, request)
    });

    let prepared = match task.await {
        Ok(result) => result.map_err(|cancelled| cancelled.into_lsp_error())?,
        Err(error) => {
            tracing::error!(%error, "goto definition worker panicked");
            return Err(jsonrpc::Error::internal_error());
        }
    };

    finish_goto_definition(backend, prepared, &request)
        .await
        .map_err(|cancelled| cancelled.into_lsp_error())
}

fn handle_goto_definition_blocking(
    workspace: Arc<Workspace>,
    registry: Arc<LanguageRegistry>,
    params: GotoDefinitionParams,
    request: Arc<RequestContext>,
) -> crate::lsp::request_cancellation::RequestResult<GotoPrepared> {
    let started = std::time::Instant::now();
    let uri = &params.text_document_position_params.text_document.uri;
    let pos = params.text_document_position_params.position;

    let Some(prepared) = PreparedRequest::prepare(
        Arc::clone(&workspace),
        registry.as_ref(),
        uri,
        Arc::clone(&request),
    )?
    else {
        return Ok(GotoPrepared::Ready(None));
    };
    let lookup_pos = prepared.token_end_position(pos);
    let analysis = prepared.analysis();
    let scope = prepared.scope();
    let view = prepared.view().clone();
    let log_summary = || {
        prepared.metrics().log_summary(
            analysis.module.0,
            analysis.classpath,
            analysis.source_root.map(|id| id.0),
            started.elapsed().as_secs_f64() * 1000.0,
        );
    };

    let request_analysis_t0 = std::time::Instant::now();

    tracing::debug!(
        uri = %uri,
        module = scope.module.0,
        classpath = ?analysis.classpath,
        source_root = ?analysis.source_root.map(|id| id.0),
        view_layers = view.layer_count(),
        analysis_bundle_ms = request_analysis_t0.elapsed().as_secs_f64() * 1000.0,
        "goto: request analysis prepared"
    );

    let Some(ctx) = prepared.semantic_context(lookup_pos, None)? else {
        log_summary();
        return Ok(GotoPrepared::Ready(None));
    };

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

        let range = workspace.documents.with_doc(uri, |doc| {
            find_local_var_decl(doc.source().text(), lv.name.as_ref())
        });

        let result = Ok(GotoPrepared::Ready(Some(GotoDefinitionResponse::Scalar(
            Location {
                uri: uri.clone(),
                range: range.flatten().unwrap_or_default(),
            },
        ))));
        log_summary();
        return result;
    }

    if let CursorLocation::Import { prefix } = &ctx.location {
        let raw = prefix.trim().trim_end_matches(".*").trim();
        let internal = raw.replace('.', "/");
        if view.get_class(&internal).is_some() {
            let result = goto_resolved_symbol_blocking(
                Arc::clone(&workspace),
                &view,
                ResolvedSymbol::Class(TypeRef::source(internal)),
                &request,
            );
            log_summary();
            return result;
        }
        log_summary();
        return Ok(GotoPrepared::Ready(None));
    }

    if matches!(
        ctx.java_module_context,
        Some(
            crate::semantic::context::JavaModuleContextKind::RequiresModule
                | crate::semantic::context::JavaModuleContextKind::TargetModule
        )
    ) && !ctx.query.is_empty()
        && let Some(target) = workspace.resolve_java_module_target(analysis, ctx.query.as_str())
    {
        let Some(location) = (match target {
            JavaModuleTarget::Source { uri } => {
                Some(module_source_location(workspace.as_ref(), &uri))
            }
            JavaModuleTarget::Bytecode { module } => module_bytecode_location(module.as_ref()),
        }) else {
            log_summary();
            return Ok(GotoPrepared::Ready(None));
        };
        log_summary();
        return Ok(GotoPrepared::Ready(Some(GotoDefinitionResponse::Scalar(
            location,
        ))));
    }

    let resolver = SymbolResolver::new(&view);
    let symbol = match resolver.resolve(&ctx) {
        Some(s) => s,
        None => {
            tracing::debug!(location = ?ctx.location, "goto: resolver returned None");
            log_summary();
            return Ok(GotoPrepared::Ready(None));
        }
    };
    tracing::debug!(symbol = ?symbol, "goto: resolved symbol");
    let result = goto_resolved_symbol_blocking(workspace, &view, symbol, &request);
    log_summary();
    result
}

async fn finish_goto_definition(
    backend: &Backend,
    prepared: GotoPrepared,
    request: &RequestContext,
) -> crate::lsp::request_cancellation::RequestResult<Option<GotoDefinitionResponse>> {
    match prepared {
        GotoPrepared::Ready(response) => Ok(response),
        GotoPrepared::Decompile(plan) => goto_decompile(plan, backend, request).await,
    }
}

fn goto_resolved_symbol_blocking(
    workspace: Arc<Workspace>,
    view: &IndexView,
    symbol: ResolvedSymbol,
    request: &RequestContext,
) -> crate::lsp::request_cancellation::RequestResult<GotoPrepared> {
    let projected = match &symbol {
        ResolvedSymbol::Class(class_ref) => view.project_type_navigation_target(class_ref),
        ResolvedSymbol::Method(method_ref) => view.project_method_navigation_target(method_ref),
        ResolvedSymbol::Field(field_ref) => view.project_field_navigation_target(field_ref),
    };
    let Some(projected) = projected else {
        return Ok(GotoPrepared::Ready(None));
    };

    request.check_cancelled("goto.before_origin_lookup")?;
    match projected {
        NavigationTarget::SourceFile { uri, symbol } => {
            let Some(target_uri) = Url::parse(uri.as_ref()).ok() else {
                return Ok(GotoPrepared::Ready(None));
            };

            let content = workspace
                .documents
                .with_doc(&target_uri, |d| d.source().text().to_owned())
                .or_else(|| {
                    target_uri
                        .to_file_path()
                        .ok()
                        .and_then(|p| std::fs::read_to_string(p).ok())
                });
            let range = content
                .as_deref()
                .and_then(|content| find_resolved_symbol_range(content, &symbol, view));

            Ok(GotoPrepared::Ready(Some(GotoDefinitionResponse::Scalar(
                Location {
                    uri: target_uri,
                    range: range.unwrap_or_default(),
                },
            ))))
        }

        NavigationTarget::Bytecode { jar_path, symbol } => {
            request.check_cancelled("goto.before_decompile_extract")?;
            Ok(GotoPrepared::Decompile(DecompilePlan {
                jar_path,
                symbol,
                view: view.clone(),
            }))
        }

        NavigationTarget::ZipSource {
            zip_path,
            entry_name,
            symbol,
        } => {
            request.check_cancelled("goto.before_zip_extract")?;
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

            request.check_cancelled("goto.after_zip_extract")?;
            let content = std::fs::read_to_string(&cache_path).ok();
            let range = content
                .as_deref()
                .and_then(|content| find_resolved_symbol_range(content, &symbol, view));

            let Some(target_uri) = Url::from_file_path(&cache_path).ok() else {
                return Ok(GotoPrepared::Ready(None));
            };
            Ok(GotoPrepared::Ready(Some(GotoDefinitionResponse::Scalar(
                Location {
                    uri: target_uri,
                    range: range.unwrap_or_default(),
                },
            ))))
        }
    }
}

async fn goto_decompile(
    plan: DecompilePlan,
    backend: &Backend,
    request: &RequestContext,
) -> crate::lsp::request_cancellation::RequestResult<Option<GotoDefinitionResponse>> {
    request.check_cancelled("goto.before_decompile_extract")?;
    let Some(bytes) = extract_class_bytes(
        plan.jar_path.as_ref(),
        plan.symbol.target_internal_name.as_ref(),
    )
    .ok() else {
        return Ok(None);
    };
    let cache_path = backend
        .decompiler_cache
        .resolve(plan.symbol.target_internal_name.as_ref(), &bytes);

    if !cache_path.exists() {
        tracing::info!(class = %plan.symbol.target_internal_name, "goto: cache miss, decompiling");
        let config = backend.config.read().await;
        let Some(decompiler_jar) = config.decompiler_path.clone() else {
            return Ok(None);
        };
        let java_bin = config.get_java_bin();
        let decompiler = config.decompiler_backend.get_decompiler();
        drop(config);

        if let Err(e) = decompiler
            .decompile(
                &java_bin,
                &decompiler_jar,
                &bytes,
                &cache_path,
                request.token(),
            )
            .await
        {
            if let Some(reason) = request.cancellation_reason() {
                return Err(reason);
            }
            tracing::error!(
                error = %e,
                class = %plan.symbol.target_internal_name,
                "goto: decompile failed"
            );
            return Ok(None);
        }
        backend
            .decompiler_cache
            .cleanup_stale(plan.symbol.target_internal_name.as_ref(), &cache_path);
    }

    request.check_cancelled("goto.after_decompile")?;
    let content = std::fs::read_to_string(&cache_path).ok();
    let range = content
        .as_deref()
        .and_then(|content| find_resolved_symbol_range(content, &plan.symbol, &plan.view));

    let Some(target_uri) = Url::from_file_path(&cache_path).ok() else {
        return Ok(None);
    };
    Ok(Some(GotoDefinitionResponse::Scalar(Location {
        uri: target_uri,
        range: range.unwrap_or_default(),
    })))
}

enum GotoPrepared {
    Ready(Option<GotoDefinitionResponse>),
    Decompile(DecompilePlan),
}

struct DecompilePlan {
    jar_path: Arc<str>,
    symbol: NavigationSymbol,
    view: IndexView,
}

fn find_resolved_symbol_range(
    content: &str,
    symbol: &NavigationSymbol,
    view: &IndexView,
) -> Option<Range> {
    find_symbol_range(
        content,
        symbol.target_internal_name.as_ref(),
        symbol.member_name.as_deref(),
        symbol.descriptor.as_deref(),
        view,
    )
    .or_else(|| {
        symbol
            .fallback_name
            .as_deref()
            .and_then(|name| find_declaration_range(content, name, symbol.decl_kind))
    })
}

// ── 声明位置查找 ───────────────────────────────────────────────────────────────

fn find_declaration_range(content: &str, name: &str, kind: NavigationDeclKind) -> Option<Range> {
    for (line_idx, line) in content.lines().enumerate() {
        let col = match kind {
            NavigationDeclKind::Type => find_type_decl(line, name),
            NavigationDeclKind::Method => find_method_decl(line, name),
            NavigationDeclKind::Field => find_field_decl(line, name),
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

fn module_source_location(workspace: &Workspace, target_uri: &Url) -> Location {
    let range = workspace
        .documents
        .with_doc(target_uri, |doc| doc.source().text().to_owned())
        .or_else(|| {
            target_uri
                .to_file_path()
                .ok()
                .and_then(|path| std::fs::read_to_string(path).ok())
        })
        .and_then(|content| module_declaration_name_range(&content))
        .unwrap_or_default();

    Location {
        uri: target_uri.clone(),
        range,
    }
}

fn module_bytecode_location(module: &crate::index::IndexedJavaModule) -> Option<Location> {
    let source = render_module_descriptor_source(&module.descriptor);
    let cache_path = bytecode_module_cache_path(module);
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::write(&cache_path, source.as_bytes()).ok()?;
    let uri = Url::from_file_path(&cache_path).ok()?;
    let range = module_declaration_name_range(&source).unwrap_or_default();
    Some(Location { uri, range })
}

fn module_declaration_name_range(content: &str) -> Option<Range> {
    let mut parser = crate::language::java::make_java_parser();
    let tree = parser.parse(content, None)?;
    let rope = ropey::Rope::from_str(content);
    let name_node = module_declaration_name_node(tree.root_node())?;
    Some(crate::lsp::converters::ts_node_to_range(&name_node, &rope))
}

fn bytecode_module_cache_path(module: &crate::index::IndexedJavaModule) -> std::path::PathBuf {
    let mut hasher = DefaultHasher::new();
    module.name().hash(&mut hasher);
    module.origin.hash(&mut hasher);
    let cache_key = format!("{:016x}", hasher.finish());
    std::env::temp_dir()
        .join("java_analyzer_modules")
        .join(cache_key)
        .join("module-info.java")
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
    use crate::index::{ClassOrigin, IndexedArchiveData, IndexedJavaModule};
    use crate::language::LanguageRegistry;
    use crate::language::java::class_parser::parse_java_source_via_tree_for_test;
    use crate::language::java::module_info::JavaModuleDescriptor;
    use crate::lsp::request_cancellation::{CancellationToken, RequestFamily};
    use crate::lsp::request_context::RequestContext;
    use crate::workspace::document::Document;
    use crate::workspace::{SourceFile, Workspace};
    use std::sync::Arc;
    use tower_lsp::lsp_types::{
        GotoDefinitionParams, PartialResultParams, TextDocumentIdentifier,
        TextDocumentPositionParams, Url, WorkDoneProgressParams,
    };

    fn strip_cursor_marker(marked_source: &str) -> (String, Position) {
        let marker = marked_source.find('|').expect("cursor marker");
        let source = marked_source.replacen('|', "", 1);
        let rope = ropey::Rope::from_str(&source);
        let line = rope.byte_to_line(marker) as u32;
        let character = (marker - rope.line_to_byte(line as usize)) as u32;
        (source, Position::new(line, character))
    }

    fn open_java_document(workspace: &Workspace, uri: &Url, source: &str) {
        workspace.documents.open(Document::new(SourceFile::new(
            uri.clone(),
            "java",
            1,
            source.to_string(),
            None,
        )));
        let salsa_file = workspace
            .get_or_update_salsa_file(uri)
            .expect("salsa file for document");
        let db = workspace.salsa_db.lock();
        workspace.refresh_java_module_descriptor_for_salsa_file(&db, salsa_file);
    }

    fn make_bytecode_module(name: &str) -> IndexedJavaModule {
        IndexedJavaModule {
            descriptor: Arc::new(JavaModuleDescriptor {
                name: Arc::from(name),
                is_open: false,
                requires: vec![],
                exports: vec![],
                opens: vec![],
                uses: vec![],
                provides: vec![],
            }),
            origin: ClassOrigin::Jar(Arc::from("/tmp/modules.jar")),
        }
    }

    fn goto_location_from_marked_source(
        workspace: Arc<Workspace>,
        uri: Url,
        marked_source: &str,
    ) -> Location {
        let (source, position) = strip_cursor_marker(marked_source);
        open_java_document(workspace.as_ref(), &uri, &source);

        let prepared = handle_goto_definition_blocking(
            Arc::clone(&workspace),
            Arc::new(LanguageRegistry::new()),
            GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri: uri.clone() },
                    position,
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            },
            RequestContext::new(
                "test_goto",
                &uri,
                RequestFamily::GotoDefinition,
                1,
                CancellationToken::new(),
            ),
        )
        .expect("goto result");

        match prepared {
            GotoPrepared::Ready(Some(GotoDefinitionResponse::Scalar(location))) => location,
            _ => panic!("expected scalar goto location"),
        }
    }

    fn assert_range_matches_name(source: &str, range: Range, expected_name: &str) {
        let line = source
            .lines()
            .nth(range.start.line as usize)
            .expect("target line");
        let start = range.start.character as usize;
        let end = range.end.character as usize;
        assert_eq!(&line[start..end], expected_name);
    }

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

    #[test]
    fn test_goto_resolved_nested_class_uses_type_range_not_default_location() {
        let workspace = Arc::new(Workspace::new());
        let uri = Url::parse("file:///workspace/Outer.java").expect("uri");
        let source = indoc::indoc! {r#"
            package com.example;

            class Outer {
                class Inner {
                    class Leaf {}
                }
            }
        "#}
        .to_string();

        let classes = parse_java_source_via_tree_for_test(
            &source,
            ClassOrigin::SourceFile(Arc::from(uri.as_str())),
            None,
        );
        workspace.index.update(|index| index.add_classes(classes));
        workspace.documents.open(Document::new(SourceFile::new(
            uri.clone(),
            "java",
            1,
            source.clone(),
            None,
        )));

        let view = workspace.index.load().view(crate::index::IndexScope {
            module: crate::index::ModuleId::ROOT,
        });
        let request = RequestContext::new(
            "test_goto",
            &uri,
            RequestFamily::GotoDefinition,
            1,
            CancellationToken::new(),
        );

        let prepared = goto_resolved_symbol_blocking(
            Arc::clone(&workspace),
            &view,
            ResolvedSymbol::Class(TypeRef::source("com/example/Outer$Inner$Leaf")),
            &request,
        )
        .expect("goto result");

        match prepared {
            GotoPrepared::Ready(Some(GotoDefinitionResponse::Scalar(location))) => {
                assert_eq!(location.uri, uri);
                let expected_line = source
                    .lines()
                    .position(|line| line.contains("Leaf"))
                    .expect("Leaf declaration line") as u32;
                assert_eq!(
                    location.range.start.line, expected_line,
                    "{:?}",
                    location.range
                );
                let line = source
                    .lines()
                    .nth(location.range.start.line as usize)
                    .expect("target line");
                let start = location.range.start.character as usize;
                let end = location.range.end.character as usize;
                assert_eq!(&line[start..end], "Leaf");
            }
            _ => panic!("expected source goto location"),
        }
    }

    #[test]
    fn test_goto_module_requires_jumps_to_module_declaration_name() {
        let workspace = Arc::new(Workspace::new());
        let target_uri = Url::parse("file:///workspace/shared/module-info.java").expect("uri");
        let target_source = "module com.example.shared { }";
        open_java_document(workspace.as_ref(), &target_uri, target_source);

        let app_uri = Url::parse("file:///workspace/app/module-info.java").expect("uri");
        let location = goto_location_from_marked_source(
            workspace,
            app_uri,
            "module com.example.app { requires com.example.sh|ared; }",
        );

        assert_eq!(location.uri, target_uri);
        assert_range_matches_name(target_source, location.range, "com.example.shared");
    }

    #[test]
    fn test_goto_module_target_jumps_to_module_declaration_name() {
        let workspace = Arc::new(Workspace::new());
        let target_uri = Url::parse("file:///workspace/shared/module-info.java").expect("uri");
        let target_source = "module com.example.shared { }";
        open_java_document(workspace.as_ref(), &target_uri, target_source);

        let app_uri = Url::parse("file:///workspace/app/module-info.java").expect("uri");
        let location = goto_location_from_marked_source(
            workspace,
            app_uri,
            "module com.example.app { exports com.example.api to com.example.sh|ared; }",
        );

        assert_eq!(location.uri, target_uri);
        assert_range_matches_name(target_source, location.range, "com.example.shared");
    }

    #[test]
    fn test_goto_module_requires_bytecode_target_renders_synthetic_source() {
        let workspace = Arc::new(Workspace::new());
        workspace.index.update(|index| {
            index.add_jdk_archive(IndexedArchiveData {
                classes: vec![],
                modules: vec![make_bytecode_module("com.example.bytecode")],
            });
        });

        let app_uri = Url::parse("file:///workspace/app/module-info.java").expect("uri");
        let location = goto_location_from_marked_source(
            workspace,
            app_uri,
            "module com.example.app { requires com.example.byt|ecode; }",
        );

        let path = location.uri.to_file_path().expect("synthetic module file");
        assert!(path.ends_with("module-info.java"), "{path:?}");
        assert!(
            path.to_string_lossy().contains("java_analyzer_modules"),
            "{path:?}"
        );

        let source = std::fs::read_to_string(&path).expect("synthetic module source");
        assert!(
            source.contains("module com.example.bytecode"),
            "unexpected synthetic module source: {source}"
        );
        assert_range_matches_name(&source, location.range, "com.example.bytecode");
    }
}
