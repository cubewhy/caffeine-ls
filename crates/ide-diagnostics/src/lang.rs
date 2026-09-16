//! The language registry of the diagnostics layer: one registration per
//! language for what a file reports (IntelliJ: the per-language inspections and
//! the highlight information they produce).
//!
//! The trait is crate-private because the sink a collector writes into is:
//! everything outside this crate reads the merged report
//! ([`crate::file_report`]), which is language-neutral. A kind no language
//! answers for reports nothing — a file with no source root yet, or a non-JVM
//! file — which is what the collectors answer for such a file anyway.

use hir_ty::TyDatabase;
use ide_db::base_db::LanguageKind;
use vfs::FileId;

use crate::{DiagnosticSink, lint};

/// The diagnostics of one language.
pub(crate) trait LanguageDiagnostics: Sync {
    /// The kinds this implementation answers for.
    fn kinds(&self) -> &'static [LanguageKind];

    /// Pushes the file's body/type diagnostics, each with its own severity.
    fn body_diagnostics(&self, sink: &mut DiagnosticSink, db: &dyn TyDatabase, file: FileId);

    /// Pushes the file's declaration-level diagnostics.
    fn declaration_diagnostics(
        &self,
        sink: &mut DiagnosticSink,
        db: &dyn TyDatabase,
        file: FileId,
    ) {
        let _ = (sink, db, file);
    }

    /// The `@SuppressWarnings` scopes in force in the file, in source order.
    fn suppression_scopes(&self, db: &dyn TyDatabase, file: FileId) -> Vec<lint::SuppressionScope> {
        let _ = (db, file);
        Vec::new()
    }
}

/// Every registered language, in lookup order.
static LANGUAGES: &[&dyn LanguageDiagnostics] =
    &[&crate::java::plugin::JAVA, &crate::kotlin::plugin::KOTLIN];

/// The diagnostics of a file of `kind`.
pub(crate) fn diagnostics(kind: LanguageKind) -> Option<&'static dyn LanguageDiagnostics> {
    LANGUAGES
        .iter()
        .copied()
        .find(|language| language.kinds().contains(&kind))
}

/// The diagnostics of the language declaring `file`.
pub(crate) fn for_file(
    db: &dyn TyDatabase,
    file: FileId,
) -> Option<&'static dyn LanguageDiagnostics> {
    diagnostics(hir::file_item_tree(db, file).language())
}

/// The kinds the registered languages answer for — the completeness check the
/// registration test asserts ([`crate::registered_kinds`]).
pub fn kinds() -> Vec<LanguageKind> {
    LANGUAGES
        .iter()
        .flat_map(|language| language.kinds().iter().copied())
        .collect()
}
