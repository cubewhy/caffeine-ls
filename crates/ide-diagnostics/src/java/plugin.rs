//! Java as the diagnostics layer's registry entry: the declaration-level
//! checks and the `@SuppressWarnings` scopes a Java file answers with. The
//! body/type diagnostics are the Java type layer's (the item walk
//! [`crate::item_diagnostics_impl`]).

use hir_ty::TyDatabase;
use ide_db::base_db::LanguageKind;
use vfs::FileId;

use crate::Severity;
use crate::lang::LanguageDiagnostics;
use crate::{
    DiagnosticSink, all_items, decl_code, decl_message, find_method, item_diagnostics_impl, lint,
    make_diagnostic,
};

pub(crate) struct Java;

pub(crate) static JAVA: Java = Java;

impl LanguageDiagnostics for Java {
    fn kinds(&self) -> &'static [LanguageKind] {
        // `Unknown` is answered here because these are the paths a file no
        // language lowered runs through: it declares no item at all, so every
        // collector finds nothing.
        &[LanguageKind::Java, LanguageKind::Unknown]
    }

    fn body_diagnostics(&self, sink: &mut DiagnosticSink, db: &dyn TyDatabase, file_id: FileId) {
        let tree = hir::hir_def::java::plugin::tree(db, file_id);
        for (item_id, _) in all_items(&tree) {
            for diagnostic in item_diagnostics_impl(db, file_id, item_id) {
                sink.push(file_id, diagnostic);
            }
        }
    }

    fn declaration_diagnostics(
        &self,
        sink: &mut DiagnosticSink,
        db: &dyn TyDatabase,
        file_id: FileId,
    ) {
        for diagnostic in hir_ty::class_diagnostics(db, file_id) {
            // §9.6.4.5: a warning named by an enclosing `@SuppressWarnings` is not
            // reported at all.
            if !lint::keeps_decl_diagnostic(db, file_id, &diagnostic) {
                continue;
            }
            let Some(range) = diagnostic.range().or_else(|| {
                // The hierarchy checks (incompatible override, conflicting
                // defaults, missing `@Override`) are keyed to the declaring method
                // name; point at the whole declaration when no reference range is
                // recorded.
                let tree = hir::hir_def::java::plugin::tree(db, file_id);
                let method_name = diagnostic.method_name();
                let item = tree
                    .top
                    .iter()
                    .copied()
                    .find_map(|top| find_method(&tree, top, method_name));
                item.and_then(|item| {
                    // The item tree carries no offsets; resolve the declaration
                    // range from the file's parse.
                    let language = tree.language;
                    if language == LanguageKind::Unknown {
                        return None;
                    }
                    let source =
                        ide_db::base_db::parse(db, file_id, language).syntax_node(language);
                    let map = hir::hir_def::db::ast_id_map(db, file_id, language);
                    hir::hir_def::java::ranges::item_range(map, &source, &tree, item)
                })
            }) else {
                continue;
            };
            sink.push(
                file_id,
                make_diagnostic(
                    file_id,
                    &decl_message(db, &diagnostic),
                    range,
                    Some(decl_code(&diagnostic)),
                    // A raw-type or deprecation declaration report is a warning
                    // ([JLS §4.12.2], [§9.6.4.6]): a legal program, flagged for
                    // its unsoundness or its use of a deprecated API.
                    lint::severity_of_decl(&diagnostic),
                ),
            );
        }
        // §7.7: the module-directive checks (`requires` of an unknown module,
        // `exports`/`opens` of an empty package, a `provides` implementation not
        // a subtype of its service) of a `module-info.java`. Every module
        // diagnostic carries its own range.
        for diagnostic in hir_ty::module_diagnostics(db, file_id) {
            if !lint::keeps_decl_diagnostic(db, file_id, &diagnostic) {
                continue;
            }
            let Some(range) = diagnostic.range() else {
                continue;
            };
            sink.push(
                file_id,
                make_diagnostic(
                    file_id,
                    &decl_message(db, &diagnostic),
                    range,
                    Some(decl_code(&diagnostic)),
                    Severity::Error,
                ),
            );
        }
        // A construct newer than the file's project source level (e.g. a record in
        // a `-source 11` module). Always an error: javac rejects it too.
        for diagnostic in hir_ty::level_diagnostics(db, file_id) {
            if !lint::keeps_decl_diagnostic(db, file_id, &diagnostic) {
                continue;
            }
            let Some(range) = diagnostic.range() else {
                continue;
            };
            sink.push(
                file_id,
                make_diagnostic(
                    file_id,
                    &decl_message(db, &diagnostic),
                    range,
                    Some(decl_code(&diagnostic)),
                    Severity::Error,
                ),
            );
        }
    }

    fn suppression_scopes(
        &self,
        db: &dyn TyDatabase,
        file_id: FileId,
    ) -> Vec<lint::SuppressionScope> {
        lint::suppression_scopes(db, file_id)
    }
}
