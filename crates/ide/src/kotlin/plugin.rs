//! Kotlin as the IDE layer's registry entry: every feature forwards to the
//! module that implements it (or is absent, where the Kotlin side of a feature
//! has not landed — see each default).

use ide_db::RootDatabase;
use ide_db::base_db::LanguageKind;
use ide_db::base_db::{file_language_kind, parse};
use rowan::{TextRange, TextSize};
use triomphe::Arc;
use vfs::FileId;

use crate::{
    highlight, inlay_hints,
    inlay_hints::{InlayHintDetail, InlayHintKind, InlayHintsConfig},
    lang::LanguageIde,
    nav, symbols,
};

pub(crate) struct Kotlin;

pub(crate) static KOTLIN: Kotlin = Kotlin;

impl LanguageIde for Kotlin {
    fn kinds(&self) -> &'static [LanguageKind] {
        &[LanguageKind::Kotlin, LanguageKind::KotlinScript]
    }

    fn definition(
        &self,
        db: &RootDatabase,
        file: FileId,
        offset: TextSize,
    ) -> Vec<nav::NavigationTarget> {
        nav::kotlin::definition(db, file, offset)
    }

    fn references(
        &self,
        db: &RootDatabase,
        file: FileId,
        offset: TextSize,
        include_declaration: bool,
    ) -> Vec<nav::ReferenceTarget> {
        nav::kotlin::references(db, file, offset, include_declaration)
    }

    fn pending_library_files(
        &self,
        db: &RootDatabase,
        file: FileId,
        offset: TextSize,
    ) -> Vec<nav::LibraryFileRef> {
        nav::kotlin::pending_library_files(db, file, offset)
    }

    fn hover(&self, db: &RootDatabase, file: FileId, offset: TextSize) -> Option<nav::HoverInfo> {
        nav::kotlin::hover(db, file, offset)
    }

    fn hover_docs(&self, db: &RootDatabase, file: FileId, item: symbols::ItemId) -> Option<String> {
        crate::docs::kdoc_of(db, file, item)
    }

    fn class_declaration(
        &self,
        db: &RootDatabase,
        file: FileId,
        fqn: &str,
    ) -> Option<nav::NavigationTarget> {
        nav::kotlin::class_declaration(db, file, fqn)
    }

    fn highlight(&self, db: &RootDatabase, file: FileId) -> Vec<highlight::Highlight> {
        let language = file_language_kind(db, file).unwrap_or(LanguageKind::Kotlin);
        let source = parse(db, file, language).syntax_node(language);
        highlight::kotlin::highlight(&source)
    }

    fn inlay_hints(
        &self,
        db: &RootDatabase,
        file: FileId,
        range: TextRange,
        config: &InlayHintsConfig,
    ) -> Vec<inlay_hints::InlayHint> {
        inlay_hints::kotlin::hints(db, file, range, config)
    }

    fn inlay_hint_resolve(
        &self,
        db: &RootDatabase,
        file: FileId,
        offset: TextSize,
        kind: InlayHintKind,
        config: &InlayHintsConfig,
    ) -> Option<InlayHintDetail> {
        inlay_hints::kotlin::resolve(db, file, offset, kind, config)
    }

    fn document_symbols(&self, db: &RootDatabase, file: FileId) -> Vec<symbols::DocumentSymbol> {
        symbols::kotlin_document_symbols(db, file)
    }

    fn package_symbol(&self, db: &RootDatabase, file: FileId) -> symbols::DocumentSymbol {
        symbols::kotlin_package_symbol(db, file)
    }

    fn source_symbol_range(
        &self,
        db: &RootDatabase,
        file: FileId,
        item: symbols::ItemId,
    ) -> Option<TextRange> {
        symbols::kotlin_source_symbol_range(db, file, item)
    }

    fn declaration_name_range(
        &self,
        db: &RootDatabase,
        file: FileId,
        item: symbols::ItemId,
    ) -> Option<TextRange> {
        nav::kotlin::kotlin_declaration_name_range(db, file, item)
    }

    fn item_ty(&self, db: &RootDatabase, file: FileId, item: symbols::ItemId) -> String {
        hir_ty::kotlin::db::item_ty(db, file, item)
            .display_simple(db)
            .to_string()
    }

    /// The parameter types of a Kotlin callable, rendered from its declaration
    /// — a Kotlin function's own parameter list, which the type layer's
    /// resolved-call table does not carry yet.
    fn method_params(
        &self,
        db: &RootDatabase,
        file: FileId,
        item: symbols::ItemId,
    ) -> Arc<[String]> {
        let Some(tree) = hir::hir_def::kotlin::plugin::tree(db, file) else {
            return Arc::from(Vec::new());
        };
        let params = match tree.data(item) {
            hir::hir_def::kotlin::item_tree::KotlinItemData::Function(data) => &data.params,
            hir::hir_def::kotlin::item_tree::KotlinItemData::Constructor(data) => &data.params,
            _ => return Arc::from(Vec::new()),
        };
        Arc::from(
            params
                .iter()
                .map(|parameter| {
                    hir::hir_def::kotlin::pretty::display_type(&parameter.param.ty).to_string()
                })
                .collect::<Vec<_>>(),
        )
    }
}
