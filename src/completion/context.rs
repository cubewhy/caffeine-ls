use std::{collections::HashMap, sync::Arc};

use rust_asm::constants::{ACC_PRIVATE, ACC_STATIC};

use crate::{
    completion::type_resolver::type_name::TypeName,
    index::{FieldSummary, MethodSummary},
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
            Self::Method(m) => m.descriptor.clone(),
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
        /// The inferred type of the accessed object (internal name, such as "java/lang/String")
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

#[derive(Debug, Clone)]
pub struct CompletionContext {
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

impl CompletionContext {
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
