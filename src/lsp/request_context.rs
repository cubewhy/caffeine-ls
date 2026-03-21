use std::sync::Arc;

use tower_lsp::lsp_types::{Position, Url};

use crate::index::{IndexScope, IndexView};
use crate::language::{Language, LanguageRegistry, ParseEnv};
use crate::salsa_queries::conversion::{FromSalsaDataWithAnalysis, RequestAnalysisState};
use crate::semantic::SemanticContext;
use crate::workspace::{AnalysisContext, SourceFile, Workspace};

pub struct PreparedRequest<'a> {
    workspace: Arc<Workspace>,
    uri: Url,
    lang: &'a dyn Language,
    file: Arc<SourceFile>,
    view: IndexView,
    salsa_file: crate::salsa_db::SourceFile,
    inferred_package: Option<Arc<str>>,
    request_analysis: RequestAnalysisState,
}

impl<'a> PreparedRequest<'a> {
    pub fn prepare(
        workspace: Arc<Workspace>,
        registry: &'a LanguageRegistry,
        uri: &Url,
    ) -> Option<Self> {
        let lang_id = workspace
            .documents
            .with_doc(uri, |doc| doc.language_id().to_owned())?;
        let lang = registry.find(&lang_id)?;
        let file = ensure_tree(&workspace, uri, lang)?;

        let analysis = workspace.analysis_context_for_uri(uri);
        let scope = analysis.scope();
        let inferred_package = workspace.infer_java_package_for_uri(uri, analysis.source_root);

        let view = {
            let db = workspace.salsa_db.lock();
            crate::salsa_queries::get_index_view_for_context(
                &*db,
                scope.module,
                analysis.classpath,
                analysis.source_root,
            )
        };

        let salsa_file = workspace.get_or_update_salsa_file(uri)?;

        Some(Self {
            workspace,
            uri: uri.clone(),
            lang,
            file,
            view,
            salsa_file,
            inferred_package,
            request_analysis: RequestAnalysisState { analysis },
        })
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

    pub fn salsa_file(&self) -> crate::salsa_db::SourceFile {
        self.salsa_file
    }

    pub fn parse_env(&self) -> ParseEnv {
        ParseEnv {
            name_table: None,
            view: Some(self.view.clone()),
            workspace: Some(Arc::clone(&self.workspace)),
        }
    }

    pub fn token_end_position(&self, position: Position) -> Position {
        Position::new(
            position.line,
            token_end_character(self.source_text(), position.line, position.character),
        )
    }

    pub fn semantic_context(
        &self,
        position: Position,
        trigger: Option<char>,
    ) -> Option<SemanticContext> {
        tracing::debug!(
            uri = %self.uri,
            module = self.request_analysis.analysis.module.0,
            classpath = ?self.request_analysis.analysis.classpath,
            source_root = ?self.request_analysis.analysis.source_root.map(|id| id.0),
            path = "index_view",
            "building request semantic context without NameTable"
        );
        let context_data = {
            let db = self.workspace.salsa_db.lock();
            if self.lang.id() == "java" {
                crate::salsa_queries::java::extract_java_completion_context(
                    &*db,
                    self.salsa_file,
                    position.line,
                    position.character,
                    trigger,
                )
            } else {
                self.lang.extract_completion_context_salsa(
                    &*db,
                    self.salsa_file,
                    position.line,
                    position.character,
                    trigger,
                )?
            }
        };

        let db = self.workspace.salsa_db.lock();
        let mut ctx = SemanticContext::from_salsa_data_with_analysis(
            context_data.as_ref().clone(),
            &*db,
            self.salsa_file,
            Some(&*self.workspace),
            Some(&self.request_analysis),
        );

        if let Some(pkg) = self.inferred_package.as_ref() {
            ctx = ctx.with_inferred_package(Arc::clone(pkg));
        }

        self.lang
            .enrich_completion_context(&mut ctx, self.scope(), &self.view);

        Some(ctx)
    }

    pub fn semantic_context_at_token_end(
        &self,
        position: Position,
        trigger: Option<char>,
    ) -> Option<SemanticContext> {
        self.semantic_context(self.token_end_position(position), trigger)
    }
}

fn ensure_tree(workspace: &Workspace, uri: &Url, lang: &dyn Language) -> Option<Arc<SourceFile>> {
    let has_tree = workspace
        .documents
        .with_doc(uri, |doc| doc.source().tree.is_some())
        .unwrap_or(false);

    if !has_tree {
        workspace.documents.with_doc_mut(uri, |doc| {
            if doc.source().tree.is_some() {
                return;
            }
            let tree = lang.parse_tree(doc.source().text(), None);
            doc.set_tree(tree);
        });
    }

    workspace
        .documents
        .with_doc(uri, |doc| Arc::clone(doc.source()))
}

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
