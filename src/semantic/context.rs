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

    pub fn is_constructor_like(&self) -> bool {
        matches!(self.name().as_ref(), "<init>" | "<clinit>")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessReceiverKind {
    Unknown,
    Expression,
    Type { class_internal_name: Arc<str> },
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
        /// Whether semantic enrichment proved the receiver is a value or a type.
        receiver_kind: AccessReceiverKind,
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
    /// `new Foo|`
    ConstructorCall {
        class_prefix: String,
        expected_type: Option<String>,
        /// Qualifying instance expression for inner-class construction, e.g. `outer` in `outer.new Inner()`.
        qualifier_expr: Option<String>,
        /// Resolved owner type for qualified inner-class construction.
        qualifier_owner_internal: Option<Arc<str>>,
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
    /// Annotation element key position inside `@Anno(name = value)`.
    AnnotationParam {
        prefix: String,
        annotation_name: Option<Arc<str>>,
        used_keys: Vec<Arc<str>>,
        fresh_slot: bool,
    },
    /// Variable name position: `String |name|` — suggest variable names based on type
    VariableName {
        type_name: String,
    },
    StringLiteral {
        prefix: String,
    },
    StatementLabel {
        kind: StatementLabelCompletionKind,
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

    pub fn member_access_receiver_kind(&self) -> Option<&AccessReceiverKind> {
        match self {
            CursorLocation::MemberAccess { receiver_kind, .. } => Some(receiver_kind),
            _ => None,
        }
    }

    pub fn member_access_receiver_class_internal(&self) -> Option<&str> {
        match self {
            CursorLocation::MemberAccess {
                receiver_kind:
                    AccessReceiverKind::Type {
                        class_internal_name,
                    },
                ..
            } => Some(class_internal_name.as_ref()),
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
                .or(receiver_type.as_deref()),
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

    pub fn constructor_prefix(&self) -> Option<&str> {
        match self {
            CursorLocation::ConstructorCall { class_prefix, .. } => Some(class_prefix),
            _ => None,
        }
    }

    pub fn constructor_qualifier_expr(&self) -> Option<&str> {
        match self {
            CursorLocation::ConstructorCall { qualifier_expr, .. } => qualifier_expr.as_deref(),
            _ => None,
        }
    }

    pub fn constructor_qualifier_owner_internal(&self) -> Option<&str> {
        match self {
            CursorLocation::ConstructorCall {
                qualifier_owner_internal,
                ..
            } => qualifier_owner_internal.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementLabelCompletionKind {
    Break,
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementLabelTargetKind {
    Block,
    While,
    DoWhile,
    For,
    EnhancedFor,
    Switch,
    Other,
}

impl StatementLabelTargetKind {
    pub fn is_break_target(self) -> bool {
        matches!(
            self,
            Self::Block
                | Self::While
                | Self::DoWhile
                | Self::For
                | Self::EnhancedFor
                | Self::Switch
        )
    }

    pub fn is_continue_target(self) -> bool {
        matches!(
            self,
            Self::While | Self::DoWhile | Self::For | Self::EnhancedFor
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementLabel {
    pub name: Arc<str>,
    pub target_kind: StatementLabelTargetKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionalTargetHint {
    pub expected_type_source: Option<String>,
    pub expected_type_context: Option<ExpectedTypeSource>,
    pub assignment_lhs_expr: Option<String>,
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
    VariableInitializer,
    AssignmentRhs,
    ReturnExpr,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedChainConfidence {
    Exact,
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedChainReceiverMode {
    Concrete,
    WildcardUpperBound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedChainReceiver {
    pub receiver_ty: TypeName,
    pub confidence: TypedChainConfidence,
    pub receiver_mode: TypedChainReceiverMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JavaIntrinsicAccessKind {
    ClassLiteral,
    ArrayLength,
    ObjectGetClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JavaAccessReceiverKind {
    Type,
    Expression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JavaModuleContextKind {
    DirectiveKeyword,
    RequiresModifier,
    RequiresModule,
    ExportsPackage,
    OpensPackage,
    TargetModule,
    UsesType,
    ProvidesService,
    ProvidesImplementation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaIntrinsicAccess {
    pub kind: JavaIntrinsicAccessKind,
    pub receiver_kind: JavaAccessReceiverKind,
}

#[derive(Debug, Clone)]
pub struct SemanticContext {
    pub location: CursorLocation,
    pub local_variables: Vec<LocalVar>,
    pub statement_labels: Vec<StatementLabel>,
    pub active_lambda_param_names: Vec<Arc<str>>,
    pub enclosing_class: Option<Arc<str>>,
    pub enclosing_internal_name: Option<Arc<str>>,
    /// Exact enclosing type chain from outermost to innermost source declaration.
    /// This preserves legal `$` characters inside identifiers and must be preferred
    /// over reverse-parsing `enclosing_internal_name`.
    pub enclosing_class_chain: Vec<Arc<str>>,
    pub enclosing_package: Option<Arc<str>>,
    /// Existing imports, contains wildcard imports
    pub existing_imports: Vec<Arc<str>>,
    pub static_imports: Vec<Arc<str>>,
    pub query: String,
    /// All members of the current class (parsed directly from the source file, without relying on indexes)
    pub current_class_members: HashMap<Arc<str>, CurrentClassMember>,
    /// The full current-class member list, preserving source overloads with the same name.
    pub current_class_member_list: Vec<CurrentClassMember>,
    /// The method/field member where the cursor is located (None indicates that it is in the field initializer or static block)
    pub enclosing_class_member: Option<CurrentClassMember>,
    pub char_after_cursor: Option<char>,
    pub file_uri: Option<Arc<str>>,
    pub inferred_package: Option<Arc<str>>,
    pub language_id: LanguageId,
    /// True when cursor is directly at type-member position inside a class/interface/enum body.
    /// False for executable/nested body contexts (method/constructor/lambda/initializer/local class, etc).
    pub is_class_member_position: bool,
    pub functional_target_hint: Option<FunctionalTargetHint>,
    pub typed_expr_ctx: Option<TypedExpressionContext>,
    pub typed_chain_receiver: Option<TypedChainReceiver>,
    pub java_intrinsic_access: Option<JavaIntrinsicAccess>,
    pub java_module_context: Option<JavaModuleContextKind>,
    pub current_java_module_name: Option<Arc<str>>,
    pub java_module_packages: Vec<Arc<str>>,
    pub java_module_names: Vec<Arc<str>>,
    pub expected_functional_interface: Option<TypeName>,
    pub expected_sam: Option<SamSignature>,
    /// Flow-sensitive local type overrides scoped to the current cursor region
    /// (e.g. `if (x instanceof T) { ... }` => `x -> T` inside the true branch).
    pub flow_type_overrides: HashMap<Arc<str>, TypeName>,
    pub ext: Option<Arc<dyn Any + Send + Sync>>,
}

#[derive(Debug, Clone)]
pub struct LocalVar {
    pub name: Arc<str>,
    /// internal class name, like "java/util/List"
    pub type_internal: TypeName,
    pub decl_kind: LocalVarDeclKind,
    /// For `var` declarations: the raw initializer expression text,
    /// used by enrich_context to resolve the actual type via TypeResolver.
    pub init_expr: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LocalVarDeclKind {
    #[default]
    Explicit,
    VarSyntax,
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
            statement_labels: vec![],
            active_lambda_param_names: vec![],
            enclosing_class,
            enclosing_internal_name,
            enclosing_class_chain: vec![],
            enclosing_package,
            existing_imports,
            static_imports: vec![],
            query: query.into(),
            current_class_members: HashMap::new(),
            current_class_member_list: Vec::new(),
            enclosing_class_member: None,
            char_after_cursor: None,
            file_uri: None,
            inferred_package: None,
            language_id: LanguageId::new("unknown"),
            is_class_member_position: false,
            functional_target_hint: None,
            typed_expr_ctx: None,
            typed_chain_receiver: None,
            java_intrinsic_access: None,
            java_module_context: None,
            current_java_module_name: None,
            java_module_packages: vec![],
            java_module_names: vec![],
            expected_functional_interface: None,
            expected_sam: None,
            flow_type_overrides: HashMap::new(),
            ext: None,
        }
    }

    pub fn with_static_imports(mut self, imports: Vec<Arc<str>>) -> Self {
        self.static_imports = imports;
        self
    }

    pub fn with_active_lambda_param_names(mut self, names: Vec<Arc<str>>) -> Self {
        self.active_lambda_param_names = names;
        self
    }

    pub fn with_statement_labels(mut self, labels: Vec<StatementLabel>) -> Self {
        self.statement_labels = labels;
        self
    }

    pub fn with_enclosing_class_chain(mut self, chain: Vec<Arc<str>>) -> Self {
        self.enclosing_class_chain = chain;
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

    pub fn with_class_member_position(mut self, is_class_member_position: bool) -> Self {
        self.is_class_member_position = is_class_member_position;
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

    pub fn with_java_module_context(mut self, kind: Option<JavaModuleContextKind>) -> Self {
        self.java_module_context = kind;
        self
    }

    pub fn with_current_java_module_name(mut self, name: Option<Arc<str>>) -> Self {
        self.current_java_module_name = name;
        self
    }

    pub fn with_java_module_packages(mut self, packages: Vec<Arc<str>>) -> Self {
        self.java_module_packages = packages;
        self
    }

    pub fn with_java_module_names(mut self, module_names: Vec<Arc<str>>) -> Self {
        self.java_module_names = module_names;
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
        let members: Vec<_> = members
            .into_iter()
            .filter(|member| !member.is_constructor_like())
            .collect();
        self.current_class_members = members
            .iter()
            .cloned()
            .map(|member| (member.name(), member))
            .collect();
        self.current_class_member_list = members;
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

    pub fn with_flow_type_overrides(mut self, overrides: HashMap<Arc<str>, TypeName>) -> Self {
        self.flow_type_overrides = overrides;
        self
    }

    pub fn flow_override_for_local(&self, name: &str) -> Option<&TypeName> {
        self.flow_type_overrides.get(name)
    }

    pub fn visible_statement_labels(&self) -> &[StatementLabel] {
        &self.statement_labels
    }

    /// The cursor is immediately followed by '(', and method completion does not require additional parentheses.
    pub fn is_followed_by_opener(&self) -> bool {
        matches!(
            self.char_after_cursor,
            Some('(') | Some('<') | Some('{') | Some('[')
        )
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
            receiver_kind: AccessReceiverKind::Expression,
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
            receiver_kind: AccessReceiverKind::Unknown,
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
