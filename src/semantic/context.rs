use std::{any::Any, collections::HashMap, sync::Arc};

use rust_asm::constants::{ACC_PRIVATE, ACC_STATIC};

use crate::{
    index::{FieldSummary, MethodSummary},
    language::LanguageId,
    semantic::types::type_name::TypeName,
};

#[derive(Clone, Debug)]
pub enum CurrentClassMember {
    Method(Arc<MethodSummary>),
    Field(Arc<FieldSummary>),
}

impl CurrentClassMember {
    pub fn name(&self) -> Arc<str> {
        match self {
            Self::Method(m) => m.name.clone(),
            Self::Field(f) => f.name.clone(),
        }
    }

    pub fn descriptor(&self) -> Arc<str> {
        match self {
            Self::Method(m) => m.desc(),
            Self::Field(f) => f.descriptor.clone(),
        }
    }

    pub fn access_flags(&self) -> u16 {
        match self {
            Self::Method(m) => m.access_flags,
            Self::Field(f) => f.access_flags,
        }
    }

    pub fn is_static(&self) -> bool {
        (self.access_flags() & ACC_STATIC) != 0
    }

    pub fn is_private(&self) -> bool {
        (self.access_flags() & ACC_PRIVATE) != 0
    }

    pub fn is_method(&self) -> bool {
        matches!(self, Self::Method(_))
    }

    pub fn is_field(&self) -> bool {
        matches!(self, Self::Field(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorLocation {
    /// `import com.example.|`
    Import {
        prefix: String,
    },
    /// `import static java.lang.Math.|`
    ImportStatic {
        prefix: String,
    },
    /// `someObj.|` or `someObj.prefix|`
    MemberAccess {
        /// The inferred semantic type of the accessed object, preserving generics/arrays when known.
        receiver_semantic_type: Option<TypeName>,
        /// Legacy compatibility field for the erased receiver owner internal name
        /// (e.g. "java/util/List"). This does not preserve generic arguments.
        receiver_type: Option<Arc<str>>,
        /// Prefix of members entered before the cursor
        member_prefix: String,
        /// Receiver expression plaintext, used by TypeResolver
        receiver_expr: String,
        /// Raw arguments text if this is a method invocation, e.g., "(1)"
        arguments: Option<String>,
    },
    /// `ClassName.|` (static access)
    StaticAccess {
        class_internal_name: Arc<str>,
        member_prefix: String,
    },
    /// `new Foo|`
    ConstructorCall {
        class_prefix: String,
        expected_type: Option<String>,
    },
    /// Type annotation location: the type part of the variable declaration `Ma|in m;`
    // The class name should be completed, not the variable name.
    TypeAnnotation {
        prefix: String,
    },
    /// Java method reference location: `Type::method`, `expr::method`, `Type::new`.
    MethodReference {
        qualifier_expr: String,
        member_prefix: String,
        is_constructor: bool,
    },
    /// Method call parameter location: `foo(aV|)` → Complete local variable
    MethodArgument {
        prefix: String,
    },
    /// Location of a regular expression (which could be a local variable, static class name, or keyword)
    Expression {
        prefix: String,
    },
    /// Annotations, e.g @Override
    Annotation {
        prefix: String,
        /// ElementType constant name: "TYPE", "METHOD", "FIELD", "PARAMETER",
        /// "CONSTRUCTOR", "LOCAL_VARIABLE", "RECORD_COMPONENT", "MODULE", etc.
        /// None = position unknown, show everything.
        target_element_type: Option<Arc<str>>,
    },
    /// Variable name position: `String |name|` — suggest variable names based on type
    VariableName {
        type_name: String,
    },
    StringLiteral {
        prefix: String,
    },
    /// Unrecognized location
    Unknown,
}

impl CursorLocation {
    pub fn member_access_receiver_semantic_type(&self) -> Option<&TypeName> {
        match self {
            CursorLocation::MemberAccess {
                receiver_semantic_type,
                ..
            } => receiver_semantic_type.as_ref(),
            _ => None,
        }
    }

    pub fn member_access_receiver_owner_internal(&self) -> Option<&str> {
        match self {
            CursorLocation::MemberAccess {
                receiver_semantic_type,
                receiver_type,
                ..
            } => receiver_semantic_type
                .as_ref()
                .map(TypeName::erased_internal)
                .or_else(|| receiver_type.as_deref()),
            _ => None,
        }
    }

    pub fn member_access_prefix(&self) -> Option<&str> {
        match self {
            CursorLocation::MemberAccess { member_prefix, .. } => Some(member_prefix),
            _ => None,
        }
    }

    pub fn member_access_expr(&self) -> Option<&str> {
        match self {
            CursorLocation::MemberAccess { receiver_expr, .. } => Some(receiver_expr),
            _ => None,
        }
    }

    pub fn member_access_arguments(&self) -> Option<&str> {
        match self {
            CursorLocation::MemberAccess { arguments, .. } => arguments.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionalTargetHint {
    pub expected_type_source: Option<String>,
    pub method_call: Option<FunctionalMethodCallHint>,
    pub expr_shape: Option<FunctionalExprShape>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionalMethodCallHint {
    pub receiver_expr: String,
    pub method_name: String,
    pub arg_index: usize,
    pub arg_texts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamSignature {
    pub method_name: Arc<str>,
    pub param_types: Vec<TypeName>,
    pub return_type: Option<TypeName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodRefQualifierKind {
    Type,
    Expr,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FunctionalExprShape {
    MethodReference {
        qualifier_expr: String,
        member_name: String,
        is_constructor: bool,
        qualifier_kind: MethodRefQualifierKind,
    },
    Lambda {
        param_count: usize,
        expression_body: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionalCompatStatus {
    Exact,
    Partial,
    Incompatible,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionalCompat {
    pub status: FunctionalCompatStatus,
    pub resolved_owner: Option<TypeName>,
    pub resolved_return: Option<TypeName>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectedTypeSource {
    AssignmentRhs,
    MethodArgument { arg_index: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedTypeConfidence {
    Exact,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedType {
    pub ty: TypeName,
    pub source: ExpectedTypeSource,
    pub confidence: ExpectedTypeConfidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedExpressionContext {
    pub expected_type: Option<ExpectedType>,
    pub receiver_type: Option<TypeName>,
    pub functional_compat: Option<FunctionalCompat>,
}

#[derive(Debug, Clone)]
pub struct SemanticContext {
    pub location: CursorLocation,
    pub local_variables: Vec<LocalVar>,
    pub enclosing_class: Option<Arc<str>>,
    pub enclosing_internal_name: Option<Arc<str>>,
    pub enclosing_package: Option<Arc<str>>,
    /// Existing imports, contains wildcard imports
    pub existing_imports: Vec<Arc<str>>,
    pub static_imports: Vec<Arc<str>>,
    pub query: String,
    /// All members of the current class (parsed directly from the source file, without relying on indexes)
    pub current_class_members: HashMap<Arc<str>, CurrentClassMember>,
    /// The method/field member where the cursor is located (None indicates that it is in the field initializer or static block)
    pub enclosing_class_member: Option<CurrentClassMember>,
    pub char_after_cursor: Option<char>,
    pub file_uri: Option<Arc<str>>,
    pub inferred_package: Option<Arc<str>>,
    pub language_id: LanguageId,
    pub functional_target_hint: Option<FunctionalTargetHint>,
    pub typed_expr_ctx: Option<TypedExpressionContext>,
    pub expected_functional_interface: Option<TypeName>,
    pub expected_sam: Option<SamSignature>,
    pub ext: Option<Arc<dyn Any + Send + Sync>>,
}

#[derive(Debug, Clone)]
pub struct LocalVar {
    pub name: Arc<str>,
    /// internal class name, like "java/util/List"
    pub type_internal: TypeName,
    /// For `var` declarations: the raw initializer expression text,
    /// used by enrich_context to resolve the actual type via TypeResolver.
    pub init_expr: Option<String>,
}

impl SemanticContext {
    pub fn new(
        location: CursorLocation,
        query: impl Into<String>,
        local_variables: Vec<LocalVar>,
        enclosing_class: Option<Arc<str>>,
        enclosing_internal_name: Option<Arc<str>>,
        enclosing_package: Option<Arc<str>>,
        existing_imports: Vec<Arc<str>>,
    ) -> Self {
        Self {
            location,
            local_variables,
            enclosing_class,
            enclosing_internal_name,
            enclosing_package,
            existing_imports,
            static_imports: vec![],
            query: query.into(),
            current_class_members: HashMap::new(),
            enclosing_class_member: None,
            char_after_cursor: None,
            file_uri: None,
            inferred_package: None,
            language_id: LanguageId::new("unknown"),
            functional_target_hint: None,
            typed_expr_ctx: None,
            expected_functional_interface: None,
            expected_sam: None,
            ext: None,
        }
    }

    pub fn with_static_imports(mut self, imports: Vec<Arc<str>>) -> Self {
        self.static_imports = imports;
        self
    }

    pub fn with_file_uri(mut self, uri: Arc<str>) -> Self {
        self.file_uri = Some(uri);
        self
    }

    pub fn with_language_id(mut self, language_id: LanguageId) -> Self {
        self.language_id = language_id;
        self
    }

    pub fn with_functional_target_hint(mut self, hint: Option<FunctionalTargetHint>) -> Self {
        self.functional_target_hint = hint;
        self
    }

    pub fn with_typed_expression_context(mut self, typed: Option<TypedExpressionContext>) -> Self {
        self.typed_expr_ctx = typed;
        self
    }

    pub fn with_extension(mut self, ext: Arc<dyn Any + Send + Sync>) -> Self {
        self.ext = Some(ext);
        self
    }

    pub fn extension<T: Any>(&self) -> Option<&T> {
        self.ext.as_ref()?.downcast_ref::<T>()
    }

    pub fn extension_arc<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        let ext = self.ext.as_ref()?.clone();
        Arc::downcast::<T>(ext).ok()
    }

    pub fn with_inferred_package(mut self, pkg: Arc<str>) -> Self {
        self.inferred_package = Some(pkg);
        self
    }

    /// Returns valid package names: prioritizes AST resolution, then falls back to path inference.
    pub fn effective_package(&self) -> Option<&str> {
        self.enclosing_package
            .as_deref()
            .or(self.inferred_package.as_deref())
    }

    pub fn with_class_members(
        mut self,
        members: impl IntoIterator<Item = CurrentClassMember>,
    ) -> Self {
        self.current_class_members = members.into_iter().map(|m| (m.name(), m)).collect();
        self
    }

    pub fn with_enclosing_member(mut self, member: Option<CurrentClassMember>) -> Self {
        self.enclosing_class_member = member;
        self
    }

    /// Whether the current context is static (static method / static field initializer)
    pub fn is_in_static_context(&self) -> bool {
        self.enclosing_class_member
            .as_ref()
            .is_some_and(|m| m.is_static())
    }

    pub fn with_char_after_cursor(mut self, c: Option<char>) -> Self {
        self.char_after_cursor = c;
        self
    }

    /// The cursor is immediately followed by '(', and method completion does not require additional parentheses.
    pub fn has_paren_after_cursor(&self) -> bool {
        self.char_after_cursor == Some('(')
    }

    pub fn file_stem(&self) -> Option<&str> {
        let uri = self.file_uri.as_deref()?;
        let last = uri.rsplit('/').next()?;
        Some(last.split('.').next().unwrap_or(last))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_member_access_owner_derivation_prefers_semantic() {
        let loc = CursorLocation::MemberAccess {
            receiver_semantic_type: Some(TypeName::with_args(
                "java/util/List",
                vec![TypeName::new("java/lang/String")],
            )),
            receiver_type: Some(Arc::from("legacy/Wrong")),
            member_prefix: String::new(),
            receiver_expr: "x".to_string(),
            arguments: None,
        };

        assert_eq!(
            loc.member_access_receiver_owner_internal(),
            Some("java/util/List")
        );
    }

    #[test]
    fn test_member_access_owner_derivation_falls_back_to_legacy() {
        let loc = CursorLocation::MemberAccess {
            receiver_semantic_type: None,
            receiver_type: Some(Arc::from("java/util/List")),
            member_prefix: String::new(),
            receiver_expr: "x".to_string(),
            arguments: None,
        };

        assert_eq!(
            loc.member_access_receiver_owner_internal(),
            Some("java/util/List")
        );
    }
}
