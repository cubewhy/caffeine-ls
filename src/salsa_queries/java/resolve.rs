use super::common::root_index_view;
use super::completion::{extract_java_completion_context, extract_java_semantic_context_at_offset};
use crate::salsa_db::SourceFile;
use crate::salsa_queries::Db;
use crate::salsa_queries::context::{CursorLocationData, line_col_to_offset};
use crate::salsa_queries::symbols::{ResolvedSymbolData, SymbolKind};
use crate::semantic::CursorLocation;
use crate::semantic::types::symbol_resolver::{ResolvedSymbol, SymbolResolver};
use std::sync::Arc;

#[salsa::tracked]
pub fn resolve_java_symbol(
    db: &dyn Db,
    file: SourceFile,
    line: u32,
    character: u32,
) -> Option<Arc<ResolvedSymbolData>> {
    let content = file.content(db);
    let offset = line_col_to_offset(content, line, character)?;
    let context = extract_java_completion_context(db, file, line, character, None);

    match &context.location {
        CursorLocationData::Expression { prefix } => {
            resolve_java_expression_symbol(db, file, Arc::clone(prefix), offset)
        }
        CursorLocationData::MemberAccess {
            receiver_expr,
            member_prefix,
            arguments,
            ..
        } => resolve_java_member_symbol(
            db,
            file,
            Arc::clone(receiver_expr),
            Arc::clone(member_prefix),
            arguments.clone(),
            offset,
        ),
        CursorLocationData::StaticAccess {
            class_internal_name,
            member_prefix,
        } => Some(Arc::new(ResolvedSymbolData {
            kind: SymbolKind::Class,
            target_internal_name: Arc::clone(class_internal_name),
            member_name: Some(Arc::clone(member_prefix)),
            descriptor: None,
        })),
        CursorLocationData::Import { prefix } => {
            let internal = prefix.replace('.', "/");
            Some(Arc::new(ResolvedSymbolData {
                kind: SymbolKind::Class,
                target_internal_name: Arc::from(internal),
                member_name: None,
                descriptor: None,
            }))
        }
        _ => None,
    }
}

#[salsa::tracked]
pub fn is_java_local_variable(
    db: &dyn Db,
    file: SourceFile,
    symbol_name: Arc<str>,
    offset: usize,
) -> bool {
    crate::salsa_queries::symbols::find_local_variable_declaration(db, file, symbol_name, offset)
        .is_some()
}

#[salsa::tracked]
fn resolve_java_expression_symbol(
    db: &dyn Db,
    file: SourceFile,
    symbol_name: Arc<str>,
    offset: usize,
) -> Option<Arc<ResolvedSymbolData>> {
    if is_java_local_variable(db, file, Arc::clone(&symbol_name), offset) {
        return Some(Arc::new(ResolvedSymbolData {
            kind: SymbolKind::LocalVariable,
            target_internal_name: Arc::from(""),
            member_name: Some(symbol_name),
            descriptor: None,
        }));
    }

    resolve_java_symbol_with_resolver(
        db,
        file,
        offset,
        Some(CursorLocation::Expression {
            prefix: symbol_name.to_string(),
        }),
    )
}

#[salsa::tracked]
fn resolve_java_member_symbol(
    db: &dyn Db,
    file: SourceFile,
    receiver_expr: Arc<str>,
    member_name: Arc<str>,
    arguments: Option<Arc<str>>,
    offset: usize,
) -> Option<Arc<ResolvedSymbolData>> {
    resolve_java_symbol_with_resolver(
        db,
        file,
        offset,
        Some(CursorLocation::MemberAccess {
            receiver_semantic_type: None,
            receiver_type: None,
            member_prefix: member_name.to_string(),
            receiver_expr: receiver_expr.to_string(),
            arguments: arguments.map(|args| args.to_string()),
        }),
    )
}

fn resolve_java_symbol_with_resolver(
    db: &dyn Db,
    file: SourceFile,
    offset: usize,
    location_override: Option<CursorLocation>,
) -> Option<Arc<ResolvedSymbolData>> {
    let view = root_index_view(db);
    let mut ctx = extract_java_semantic_context_at_offset(db, file, offset, view.clone(), None)?;
    if let Some(location) = location_override {
        ctx.location = location;
    }

    let resolved = SymbolResolver::new(&view).resolve(&ctx)?;
    Some(Arc::new(convert_resolved_symbol(resolved)))
}

fn convert_resolved_symbol(symbol: ResolvedSymbol) -> ResolvedSymbolData {
    match symbol {
        ResolvedSymbol::Class(internal_name) => ResolvedSymbolData {
            kind: SymbolKind::Class,
            target_internal_name: internal_name,
            member_name: None,
            descriptor: None,
        },
        ResolvedSymbol::Method { owner, summary } => ResolvedSymbolData {
            kind: SymbolKind::Method,
            target_internal_name: owner,
            member_name: Some(Arc::clone(&summary.name)),
            descriptor: Some(summary.desc()),
        },
        ResolvedSymbol::Field { owner, summary } => ResolvedSymbolData {
            kind: SymbolKind::Field,
            target_internal_name: owner,
            member_name: Some(Arc::clone(&summary.name)),
            descriptor: Some(Arc::clone(&summary.descriptor)),
        },
    }
}
