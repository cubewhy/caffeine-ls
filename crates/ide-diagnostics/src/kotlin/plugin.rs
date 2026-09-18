//! Kotlin as the diagnostics layer's registry entry: the Kotlin type layer's
//! body findings and its declaration-level ones.
//!
//! The *source-level* (lint) checks are the Java type layer's, so a Kotlin file
//! reports none of them — the trait's default — and the `@SuppressWarnings`
//! scopes are a Java-source question, so nothing is suppressed in a Kotlin file
//! either.

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
            // A declaration's *initializer* expressions are inferred too — a
            // property writes one in place of a body — so both are walked
            // ([`hir_ty::kotlin_declaration_types`]).
            let types = hir_ty::kotlin_declaration_types(db, file_id, hir_expand::ids::ItemId(id));
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

    /// The declaration-level findings of the file
    /// ([`hir_ty::kotlin_class_diagnostics`]): the override rules, the abstract
    /// obligations, the supertype initialization, duplicate signatures, the
    /// applicability of a modifier and `lateinit`. Every one is an error in
    /// kotlinc, so none is suppressible and no `@Suppress` scope is consulted.
    fn declaration_diagnostics(
        &self,
        sink: &mut DiagnosticSink,
        db: &dyn TyDatabase,
        file_id: FileId,
    ) {
        for diagnostic in hir_ty::kotlin_class_diagnostics(db, file_id) {
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
