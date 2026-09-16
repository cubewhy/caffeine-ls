//! Java as the IDE layer's registry entry: every feature forwards to the
//! module that implements it.

use ide_db::RootDatabase;
use ide_db::base_db::LanguageKind;
use ide_db::base_db::{file_language_kind, parse};
use rowan::{TextRange, TextSize};
use vfs::FileId;

use crate::{
    docs, highlight, inlay_hints,
    inlay_hints::{InlayHintDetail, InlayHintKind, InlayHintsConfig},
    lang::LanguageIde,
    nav, symbols,
};

pub(crate) struct Java;

pub(crate) static JAVA: Java = Java;

/// The Java parse of `file`, for the passes that read a syntax tree.
fn source(db: &RootDatabase, file: FileId) -> syntax::SourceFile {
    let language = file_language_kind(db, file).unwrap_or(LanguageKind::Java);
    parse(db, file, language).syntax_node(language)
}

impl LanguageIde for Java {
    fn kinds(&self) -> &'static [LanguageKind] {
        &[LanguageKind::Java]
    }

    fn definition(
        &self,
        db: &RootDatabase,
        file: FileId,
        offset: TextSize,
    ) -> Vec<nav::NavigationTarget> {
        nav::java::definition(db, file, offset)
    }

    fn references(
        &self,
        db: &RootDatabase,
        file: FileId,
        offset: TextSize,
        include_declaration: bool,
    ) -> Vec<nav::ReferenceTarget> {
        nav::java::references(db, file, offset, include_declaration)
    }

    fn pending_library_files(
        &self,
        db: &RootDatabase,
        file: FileId,
        offset: TextSize,
    ) -> Vec<nav::LibraryFileRef> {
        nav::java::pending_library_files(db, file, offset)
    }

    fn hover(&self, db: &RootDatabase, file: FileId, offset: TextSize) -> Option<nav::HoverInfo> {
        nav::java::hover(db, file, offset)
    }

    fn hover_docs(&self, db: &RootDatabase, file: FileId, item: symbols::ItemId) -> Option<String> {
        docs::javadoc_of(db, file, item)
    }

    fn class_declaration(
        &self,
        db: &RootDatabase,
        file: FileId,
        fqn: &str,
    ) -> Option<nav::NavigationTarget> {
        nav::java::class_declaration(db, file, fqn)
    }

    fn declared_parameter_names(
        &self,
        db: &RootDatabase,
        file: FileId,
        method: &hir_ty::MethodData,
        constructor: bool,
    ) -> Option<Vec<String>> {
        nav::java::declared_parameter_names(db, file, method, constructor)
    }

    fn pending_parameter_names(
        &self,
        db: &RootDatabase,
        file: FileId,
        method: &hir_ty::MethodData,
        constructor: bool,
    ) -> Option<nav::LibraryFileRef> {
        nav::java::pending_parameter_names(db, file, method, constructor)
    }

    fn highlight(&self, db: &RootDatabase, file: FileId) -> Vec<highlight::Highlight> {
        let source = source(db, file);
        highlight::java::highlight(db, file, &source)
    }

    fn inlay_hints(
        &self,
        db: &RootDatabase,
        file: FileId,
        range: TextRange,
        config: &InlayHintsConfig,
    ) -> Vec<inlay_hints::InlayHint> {
        inlay_hints::java::hints(db, file, range, config)
    }

    fn inlay_hint_pending_library_files(
        &self,
        db: &RootDatabase,
        file: FileId,
        range: TextRange,
        config: &InlayHintsConfig,
    ) -> Vec<nav::LibraryFileRef> {
        inlay_hints::java::pending_library_files(db, file, range, config)
    }

    fn inlay_hint_resolve(
        &self,
        db: &RootDatabase,
        file: FileId,
        offset: TextSize,
        kind: InlayHintKind,
        config: &InlayHintsConfig,
    ) -> Option<InlayHintDetail> {
        inlay_hints::java::resolve(db, file, offset, kind, config)
    }

    fn document_symbols(&self, db: &RootDatabase, file: FileId) -> Vec<symbols::DocumentSymbol> {
        symbols::java_document_symbols(db, file)
    }

    fn package_symbol(&self, db: &RootDatabase, file: FileId) -> symbols::DocumentSymbol {
        symbols::java_package_symbol(db, file)
    }

    fn source_symbol_range(
        &self,
        db: &RootDatabase,
        file: FileId,
        item: symbols::ItemId,
    ) -> Option<TextRange> {
        symbols::java_source_symbol_range(db, file, item)
    }

    fn declaration_name_range(
        &self,
        db: &RootDatabase,
        file: FileId,
        item: symbols::ItemId,
    ) -> Option<TextRange> {
        nav::java::declaration_name_range(db, file, item)
    }

    fn item_ty(&self, db: &RootDatabase, file: FileId, item: symbols::ItemId) -> String {
        hir_ty::item_ty(db, file, item)
            .display_simple(db)
            .to_string()
    }

    fn method_params(
        &self,
        db: &RootDatabase,
        file: FileId,
        item: symbols::ItemId,
    ) -> triomphe::Arc<[String]> {
        triomphe::Arc::from(
            hir_ty::method_params(db, file, item)
                .into_iter()
                .map(|ty| ty.display_simple(db).to_string())
                .collect::<Vec<_>>(),
        )
    }
}
