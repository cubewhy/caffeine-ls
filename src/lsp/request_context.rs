use std::sync::Arc;
use std::time::Instant;

use tower_lsp::lsp_types::{Position, Url};

use crate::index::{IndexScope, IndexView};
use crate::language::rope_utils::{rope_identifier_end_position, rope_line_col_to_offset};
use crate::language::{Language, LanguageRegistry, ParseEnv};
use crate::lsp::request_cancellation::{
    CancellationToken, Cancelled, RequestFamily, RequestResult,
};
use crate::request_metrics::RequestMetrics;
use crate::salsa_queries::conversion::RequestAnalysisState;
use crate::semantic::SemanticContext;
use crate::workspace::document::SemanticContextCacheKey;
use crate::workspace::{AnalysisContext, SourceFile, Workspace};

pub struct RequestContext {
    metrics: Arc<RequestMetrics>,
    cancel: CancellationToken,
    family: RequestFamily,
    generation: u64,
}

impl std::fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestContext")
            .field("request_id", &self.metrics.request_id())
            .field("request_kind", &self.metrics.request_kind())
            .field("uri", &self.metrics.uri())
            .field("family", &self.family)
            .field("generation", &self.generation)
            .finish()
    }
}

impl RequestContext {
    pub fn new(
        request_kind: &'static str,
        uri: &Url,
        family: RequestFamily,
        generation: u64,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            metrics: RequestMetrics::new(request_kind, uri),
            cancel,
            family,
            generation,
        })
    }

    pub fn metrics(&self) -> &Arc<RequestMetrics> {
        &self.metrics
    }

    pub fn token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn check_cancelled(&self, phase: &'static str) -> RequestResult<()> {
        if let Some(reason) = self.cancel.reason() {
            tracing::debug!(
                request_id = self.metrics.request_id(),
                request_kind = self.metrics.request_kind(),
                uri = %self.metrics.uri(),
                request_family = ?self.family,
                generation = self.generation,
                cancel_reason = %reason.as_str(),
                phase,
                "observed cancelled request phase"
            );
            return Err(reason);
        }
        Ok(())
    }

    pub fn cancellation_reason(&self) -> Option<Cancelled> {
        self.cancel.reason()
    }
}

pub struct PreparedRequest<'a> {
    workspace: Arc<Workspace>,
    uri: Url,
    lang: &'a dyn Language,
    file: Arc<SourceFile>,
    view: IndexView,
    overlay_class_count: usize,
    salsa_file: crate::salsa_db::SourceFile,
    inferred_package: Option<Arc<str>>,
    request_analysis: RequestAnalysisState,
    request: Arc<RequestContext>,
}

impl<'a> PreparedRequest<'a> {
    pub fn prepare(
        workspace: Arc<Workspace>,
        registry: &'a LanguageRegistry,
        uri: &Url,
        request: Arc<RequestContext>,
    ) -> RequestResult<Option<Self>> {
        let lang_id = workspace
            .document_snapshot(uri)
            .map(|file| file.language_id.to_string());
        let Some(lang_id) = lang_id else {
            return Ok(None);
        };
        let Some(lang) = registry.find(&lang_id) else {
            return Ok(None);
        };
        request.check_cancelled("request_setup.before_ensure_tree")?;
        let Some(file) = workspace.ensure_tree(uri, lang) else {
            return Ok(None);
        };
        request.check_cancelled("request_setup.after_ensure_tree")?;

        let (analysis, inferred_package, index_snapshot) =
            workspace.load_analysis_state_for_uri(uri);
        let scope = analysis.scope();

        let view = {
            request.check_cancelled("request_setup.before_index_view")?;
            let db = workspace.salsa_db.lock();
            request.metrics().record_index_view_acquisition(
                "request_setup",
                scope.module.0,
                analysis.classpath,
                analysis.source_root.map(|id| id.0),
                false,
            );
            crate::salsa_queries::get_index_view_for_context(
                &*db,
                scope.module,
                analysis.classpath,
                analysis.source_root,
            )
        };
        request.check_cancelled("request_setup.after_index_view")?;
        let salsa_file = workspace.get_or_update_salsa_file_for_snapshot(file.as_ref());
        {
            let db = workspace.salsa_db.lock();
            request.metrics().record_parse_snapshot(
                "request_setup.salsa_file",
                crate::salsa_queries::parse::cached_parse_tree_origin(&*db, salsa_file),
            );
        }

        let origin = crate::index::ClassOrigin::SourceFile(Arc::from(uri.as_str()));
        let overlay_started = Instant::now();
        let overlay_classes = {
            let db = workspace.salsa_db.lock();
            workspace.extract_salsa_classes_for_index_context(
                &*db,
                salsa_file,
                &origin,
                index_snapshot.as_ref(),
                analysis,
            )
        };
        request
            .metrics()
            .record_phase_duration("request_setup.overlay_classes", overlay_started.elapsed());
        request
            .metrics()
            .record_event("request_setup.overlay_class_extract");
        let overlay_class_count = overlay_classes.len();
        let view = view
            .with_overlay_classes(overlay_classes)
            .with_request_metrics(Arc::clone(request.metrics()));
        tracing::debug!(
            uri = %uri,
            module = analysis.module.0,
            classpath = ?analysis.classpath,
            source_root = ?analysis.source_root.map(|id| id.0),
            overlay_class_count,
            view_layers = view.layer_count(),
            "prepared request with current-document source overlay"
        );

        let request_analysis = RequestAnalysisState {
            analysis,
            view: view.clone(),
            workspace_version: index_snapshot.version(),
        };

        Ok(Some(Self {
            workspace,
            uri: uri.clone(),
            lang,
            file,
            view,
            overlay_class_count,
            salsa_file,
            inferred_package,
            request_analysis,
            request,
        }))
    }

    pub fn uri(&self) -> &Url {
        &self.uri
    }

    pub fn lang(&self) -> &'a dyn Language {
        self.lang
    }

    pub fn file(&self) -> &Arc<SourceFile> {
        &self.file
    }

    pub fn source_text(&self) -> &str {
        self.file.text()
    }

    pub fn analysis(&self) -> AnalysisContext {
        self.request_analysis.analysis
    }

    pub fn scope(&self) -> IndexScope {
        self.request_analysis.analysis.scope()
    }

    pub fn view(&self) -> &IndexView {
        &self.view
    }

    pub fn metrics(&self) -> &Arc<RequestMetrics> {
        self.request.metrics()
    }

    pub fn request(&self) -> &Arc<RequestContext> {
        &self.request
    }

    pub fn salsa_file(&self) -> crate::salsa_db::SourceFile {
        self.salsa_file
    }

    pub fn parse_env(&self) -> ParseEnv {
        ParseEnv {
            name_table: None,
            view: Some(self.view.clone()),
            workspace: Some(Arc::clone(&self.workspace)),
            file_uri: Some(Arc::from(self.uri.as_str())),
            request: Some(Arc::clone(&self.request)),
            semantic_request_scope: Some(crate::language::SemanticRequestScope {
                uri: self.uri.clone(),
                document_version: self.file.version,
                overlay_class_count: self.overlay_class_count,
                request_analysis: self.request_analysis.clone(),
            }),
        }
    }

    pub fn token_end_position(&self, position: Position) -> Position {
        rope_identifier_end_position(&self.file.rope, position.line, position.character)
            .unwrap_or(position)
    }

    pub fn semantic_context(
        &self,
        position: Position,
        trigger: Option<char>,
    ) -> RequestResult<Option<SemanticContext>> {
        let Some(offset) =
            rope_line_col_to_offset(&self.file.rope, position.line, position.character)
        else {
            return Ok(None);
        };
        let cache_key = SemanticContextCacheKey {
            document_version: self.file.version,
            workspace_version: self.request_analysis.workspace_version,
            module: self.request_analysis.analysis.module,
            classpath: self.request_analysis.analysis.classpath,
            source_root: self.request_analysis.analysis.source_root,
            overlay_class_count: self.overlay_class_count,
            offset,
            trigger,
        };
        if let Some(cached) = self
            .workspace
            .documents
            .with_doc(&self.uri, |doc| doc.cached_semantic_context(&cache_key))
            .flatten()
        {
            tracing::debug!(
                uri = %self.uri,
                offset,
                trigger = ?trigger,
                module = self.request_analysis.analysis.module.0,
                classpath = ?self.request_analysis.analysis.classpath,
                source_root = ?self.request_analysis.analysis.source_root.map(|id| id.0),
                "semantic context cache hit"
            );
            return Ok(Some((*cached).clone()));
        }

        tracing::debug!(
            uri = %self.uri,
            module = self.request_analysis.analysis.module.0,
            classpath = ?self.request_analysis.analysis.classpath,
            source_root = ?self.request_analysis.analysis.source_root.map(|id| id.0),
            path = "index_view",
            "building request semantic context without NameTable"
        );
        self.request
            .check_cancelled("semantic_context.before_extract")?;
        let extract_started = Instant::now();
        let Some(context_data) = ({
            let db = self.workspace.salsa_db.lock();
            self.request.metrics().record_parse_snapshot(
                "semantic_context.before_extract",
                crate::salsa_queries::parse::cached_parse_tree_origin(&*db, self.salsa_file),
            );
            self.lang.extract_completion_context_salsa_at_offset(
                &*db,
                self.salsa_file,
                offset,
                trigger,
            )
        }) else {
            return Ok(None);
        };
        self.request
            .metrics()
            .record_phase_duration("semantic_context.extract", extract_started.elapsed());
        self.request
            .check_cancelled("semantic_context.after_extract")?;

        let build_started = Instant::now();
        let db = self.workspace.salsa_db.lock();
        self.request.metrics().record_parse_snapshot(
            "semantic_context.before_build",
            crate::salsa_queries::parse::cached_parse_tree_origin(&*db, self.salsa_file),
        );
        self.request
            .check_cancelled("semantic_context.before_build")?;
        let mut ctx = self.lang.build_semantic_context_salsa(
            &*db,
            self.salsa_file,
            context_data.as_ref().clone(),
            Some(&*self.workspace),
            &self.request_analysis,
        );
        self.request
            .metrics()
            .record_phase_duration("semantic_context.build", build_started.elapsed());
        self.request
            .check_cancelled("semantic_context.after_build")?;

        if let Some(pkg) = self.inferred_package.as_ref() {
            ctx = ctx.with_inferred_package(Arc::clone(pkg));
        }

        if self
            .workspace
            .documents
            .with_doc(&self.uri, |doc| doc.version() == cache_key.document_version)
            .unwrap_or(false)
        {
            let cached = Arc::new(ctx.clone());
            self.workspace.documents.with_doc_mut(&self.uri, |doc| {
                if doc.version() == cache_key.document_version {
                    doc.cache_semantic_context(cache_key, Arc::clone(&cached));
                }
            });
        }

        self.request
            .metrics()
            .record_phase_duration("semantic_context.total", extract_started.elapsed());
        Ok(Some(ctx))
    }

    pub fn semantic_context_at_token_end(
        &self,
        position: Position,
        trigger: Option<char>,
    ) -> RequestResult<Option<SemanticContext>> {
        self.semantic_context(self.token_end_position(position), trigger)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::ClassOrigin;
    use crate::language::java::class_parser::parse_java_source_via_tree_for_test;
    use crate::lsp::request_cancellation::{CancellationToken, RequestFamily};
    use crate::semantic::LocalVar;
    use crate::semantic::context::CursorLocation;
    use crate::semantic::types::type_name::TypeName;
    use crate::workspace::document::Document;
    use ropey::Rope;

    #[test]
    fn java_prepared_request_materializes_var_receiver_type() {
        let workspace = Arc::new(Workspace::new());
        let registry = LanguageRegistry::new();
        let uri = Url::parse("file:///workspace/Main.java").expect("uri");
        let source = indoc::indoc! {r#"
            package org.example;

            class Main {
                void foo(String name, int age) {
                    var a = new User(name, age);
                    a.
                }
            }

            class User {
                User(String name, int age) {}

                void greet() {}
            }
        "#}
        .to_string();

        let lang = registry.find("java").expect("java language");
        let tree = lang.parse_tree(&source, None);
        let parsed = crate::language::java::class_parser::parse_java_source_via_tree_for_test(
            &source,
            ClassOrigin::SourceFile(Arc::from(uri.as_str())),
            None,
        );
        workspace.index.update(|index| index.add_classes(parsed));
        workspace.documents.open(Document::new(SourceFile::new(
            uri.clone(),
            "java",
            1,
            source.clone(),
            tree,
        )));

        let request = PreparedRequest::prepare(
            Arc::clone(&workspace),
            &registry,
            &uri,
            RequestContext::new(
                "test_completion",
                &uri,
                RequestFamily::Completion,
                1,
                CancellationToken::new(),
            ),
        )
        .expect("request result")
        .expect("prepared request");

        let byte_offset = source.find("a.").expect("member access") + 2;
        let rope = Rope::from_str(&source);
        let line = rope.byte_to_line(byte_offset) as u32;
        let character = (byte_offset - rope.line_to_byte(line as usize)) as u32;
        let ctx = request
            .semantic_context(Position::new(line, character), Some('.'))
            .expect("request result")
            .expect("semantic context");

        let local = ctx
            .local_variables
            .iter()
            .find(|local| local.name.as_ref() == "a")
            .expect("local a");
        assert_eq!(local.type_internal.erased_internal(), "org/example/User");

        match &ctx.location {
            CursorLocation::MemberAccess {
                receiver_expr,
                receiver_type,
                receiver_semantic_type,
                ..
            } => {
                assert_eq!(receiver_expr, "a");
                assert_eq!(receiver_type.as_deref(), Some("org/example/User"));
                assert_eq!(
                    receiver_semantic_type
                        .as_ref()
                        .map(|ty| ty.erased_internal()),
                    Some("org/example/User")
                );
            }
            other => panic!("expected MemberAccess, got {other:?}"),
        }
    }

    #[test]
    fn java_prepared_request_ignores_stale_non_overlay_cached_context() {
        let workspace = Arc::new(Workspace::new());
        let registry = LanguageRegistry::new();
        let uri = Url::parse("file:///workspace/Main.java").expect("uri");
        let source = indoc::indoc! {r#"
            package org.example;

            public class Main {
                public class Test {
                    private void foo() {
                        Test t = new Test();
                        t.new NestedNonStatic();
                    }

                    public class NestedNonStatic {}
                }
            }
        "#}
        .to_string();

        let lang = registry.find("java").expect("java language");
        let tree = lang.parse_tree(&source, None);
        workspace.documents.open(Document::new(SourceFile::new(
            uri.clone(),
            "java",
            1,
            source.clone(),
            tree,
        )));

        let offset =
            source.find("NestedNonStatic();").expect("constructor call") + "NestedNonStatic".len();
        let rope = Rope::from_str(&source);
        let line = rope.byte_to_line(offset) as u32;
        let character = (offset - rope.line_to_byte(line as usize)) as u32;

        let stale_ctx = SemanticContext::new(
            CursorLocation::ConstructorCall {
                class_prefix: "NestedNonStatic".to_string(),
                expected_type: Some("Test.NestedNonStatic".to_string()),
                qualifier_expr: Some("t".to_string()),
                qualifier_owner_internal: None,
            },
            "NestedNonStatic",
            vec![
                LocalVar {
                    name: Arc::from("nns"),
                    type_internal: TypeName::new("Test/NestedNonStatic"),
                    decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                    init_expr: None,
                },
                LocalVar {
                    name: Arc::from("t"),
                    type_internal: TypeName::new("Test"),
                    decl_kind: crate::semantic::LocalVarDeclKind::Explicit,
                    init_expr: None,
                },
            ],
            Some(Arc::from("Test")),
            Some(Arc::from("org/example/Main$Test")),
            Some(Arc::from("org/example")),
            vec![],
        );
        let stale_key = SemanticContextCacheKey {
            document_version: 1,
            workspace_version: workspace.index.load().version(),
            module: crate::index::ModuleId::ROOT,
            classpath: crate::index::ClasspathId::Main,
            source_root: None,
            overlay_class_count: 0,
            offset,
            trigger: None,
        };
        workspace.documents.with_doc_mut(&uri, |doc| {
            doc.cache_semantic_context(stale_key, Arc::new(stale_ctx));
        });

        let request = PreparedRequest::prepare(
            Arc::clone(&workspace),
            &registry,
            &uri,
            RequestContext::new(
                "test_completion",
                &uri,
                RequestFamily::Completion,
                1,
                CancellationToken::new(),
            ),
        )
        .expect("request result")
        .expect("prepared request");

        assert!(
            request.overlay_class_count > 0,
            "current document overlay should be active for java requests"
        );

        let ctx = request
            .semantic_context(Position::new(line, character), None)
            .expect("request result")
            .expect("semantic context");

        assert_eq!(
            ctx.location.constructor_qualifier_owner_internal(),
            Some("org/example/Main$Test")
        );
        let local_t = ctx
            .local_variables
            .iter()
            .find(|local| local.name.as_ref() == "t")
            .expect("local t");
        assert_eq!(
            local_t.type_internal.erased_internal(),
            "org/example/Main$Test"
        );
    }

    #[test]
    fn java_prepared_request_prefers_enclosing_type_name_over_same_package_top_level() {
        let workspace = Arc::new(Workspace::new());
        let registry = LanguageRegistry::new();
        let uri = Url::parse("file:///workspace/Main.java").expect("uri");
        let source = indoc::indoc! {r#"
            package org.example;

            public class Main {
                public class Test {
                    private void foo() {
                        Test t = new Test();
                        t.new NestedNonStatic();
                    }

                    public class NestedNonStatic {}
                }
            }
        "#}
        .to_string();

        let competing_uri = Url::parse("file:///workspace/Test.java").expect("uri");
        let competing_source = indoc::indoc! {r#"
            package org.example;

            public class Test {}
        "#};
        let competing_classes = parse_java_source_via_tree_for_test(
            competing_source,
            ClassOrigin::SourceFile(Arc::from(competing_uri.as_str())),
            None,
        );
        workspace
            .index
            .update(|index| index.add_classes(competing_classes));

        let lang = registry.find("java").expect("java language");
        let tree = lang.parse_tree(&source, None);
        workspace.documents.open(Document::new(SourceFile::new(
            uri.clone(),
            "java",
            1,
            source.clone(),
            tree,
        )));

        let offset =
            source.find("NestedNonStatic();").expect("constructor call") + "NestedNonStatic".len();
        let rope = Rope::from_str(&source);
        let line = rope.byte_to_line(offset) as u32;
        let character = (offset - rope.line_to_byte(line as usize)) as u32;

        let request = PreparedRequest::prepare(
            Arc::clone(&workspace),
            &registry,
            &uri,
            RequestContext::new(
                "test_completion",
                &uri,
                RequestFamily::Completion,
                1,
                CancellationToken::new(),
            ),
        )
        .expect("request result")
        .expect("prepared request");

        let ctx = request
            .semantic_context(Position::new(line, character), None)
            .expect("request result")
            .expect("semantic context");

        assert_eq!(
            ctx.location.constructor_qualifier_owner_internal(),
            Some("org/example/Main$Test"),
            "enclosing_internal={:?} location={:?} locals={:?}",
            ctx.enclosing_internal_name,
            ctx.location,
            ctx.local_variables
        );
        let local_t = ctx
            .local_variables
            .iter()
            .find(|local| local.name.as_ref() == "t")
            .expect("local t");
        assert_eq!(
            local_t.type_internal.erased_internal(),
            "org/example/Main$Test",
            "locals={:?}",
            ctx.local_variables
        );
    }
}
