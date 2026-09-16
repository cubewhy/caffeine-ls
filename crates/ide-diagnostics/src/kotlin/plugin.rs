//! Kotlin as the diagnostics layer's registry entry: the Kotlin type layer's
//! body findings.
//!
//! The declaration-level checks and the source-level (lint) checks are the Java
//! type layer's, so a Kotlin file reports none of them — the trait's default —
//! and the `@SuppressWarnings` scopes are a Java-source question, so nothing is
//! suppressed in a Kotlin file either.

use hir_ty::TyDatabase;
use ide_db::base_db::LanguageKind;
use syntax::DiagnosticCode;
use vfs::FileId;

use crate::Severity;
use crate::lang::LanguageDiagnostics;
use crate::{DiagnosticSink, make_diagnostic};

pub(crate) struct Kotlin;

pub(crate) static KOTLIN: Kotlin = Kotlin;

impl LanguageDiagnostics for Kotlin {
    fn kinds(&self) -> &'static [LanguageKind] {
        &[LanguageKind::Kotlin, LanguageKind::KotlinScript]
    }

    fn body_diagnostics(&self, sink: &mut DiagnosticSink, db: &dyn TyDatabase, file_id: FileId) {
        let Some(tree) = hir::hir_def::kotlin::plugin::tree(db, file_id) else {
            return;
        };
        for (id, _) in tree.items.iter() {
            let types = hir_ty::kotlin_body_types(db, file_id, hir_expand::ids::ItemId(id));
            for diagnostic in &types.diagnostics {
                let Some(range) = diagnostic.range() else {
                    continue;
                };
                sink.push(
                    file_id,
                    make_diagnostic(
                        file_id,
                        &diagnostic.message(db),
                        range,
                        Some(DiagnosticCode::Kotlin(diagnostic.code())),
                        Severity::Error,
                    ),
                );
            }
        }
    }
}
