use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
#[allow(non_camel_case_types)]
pub enum SyntaxKind {
    L_PAREN, // (
    R_PAREN, // )

    L_BRACE, // {
    R_BRACE, // }

    L_BRACKET, // [
    R_BRACKET, // ]

    STRING_LITERAL, // ""
    NUMBER_LITERAL, // dec, hex, oct, bin
    STRING_TEMPLATE_BEGIN,
    STRING_TEMPLATE_MID,
    STRING_TEMPLATE_END,
    TEXT_BLOCK_TEMPLATE_BEGIN,
    TEXT_BLOCK_TEMPLATE_MID,
    TEXT_BLOCK_TEMPLATE_END,
    NULL_LITERAL,               // null
    TRUE_LITERAL,               // true
    FALSE_LITERAL,              // false
    CHAR_LITERAL,               // ''
    SEMICOLON,                  // ;
    DOT,                        // .
    AT,                         // @
    PLUS,                       // +
    MINUS,                      // -
    STAR,                       // *
    SLASH,                      // /
    LESS_EQUAL,                 // <=
    LESS,                       // <
    GREATER,                    // >
    GREATER_EQUAL,              // >=
    EQUAL_EQUAL,                // ==
    EQUAL,                      // =
    OR,                         // ||
    BIT_OR,                     // |
    BIT_OR_EQUAL,               // |=
    OR_EQUAL,                   // |=
    AND,                        // &&
    BIT_AND,                    // &
    AND_EQUAL,                  // &=
    NOT,                        // !
    TILDE,                      // ~
    MODULO,                     //
    CARET,                      // ^
    DIVIDE_EQUAL,               // /=
    NOT_EQUAL,                  // !=
    MULTIPLE_EQUAL,             // *=
    PLUS_EQUAL,                 // +=
    PLUS_PLUS,                  // ++
    MINUS_EQUAL,                // -=
    MINUS_MINUS,                // --
    XOR_EQUAL,                  // ^=
    MODULO_EQUAL,               // %=
    LEFT_SHIFT_EQUAL,           // <<=
    RIGHT_SHIFT_EQUAL,          // >>=
    UNSIGNED_RIGHT_SHIFT_EQUAL, // >>>=
    LEFT_SHIFT,                 // <<
    RIGHT_SHIFT,                // >>
    UNSIGNED_RIGHT_SHIFT,       // >>>
    COMMA,                      // ,
    QUESTION,                   // ?
    ARROW,                      // ->
    COLON_COLON,                // ::
    COLON,                      // :
    ELLIPSIS,                   // ...
    TEXT_BLOCK,                 // """ """
    UNDERSCORE,                 // _

    // Keywords
    PACKAGE_KW,      // package
    IMPORT_KW,       // import
    CLASS_KW,        // class
    PUBLIC_KW,       // public
    PRIVATE_KW,      // private
    PROTECTED_KW,    // protected
    FINAL_KW,        // final
    STATIC_KW,       // static
    VOID_KW,         // void
    BYTE_KW,         // byte
    ENUM_KW,         // enum
    INTERFACE_KW,    // interface
    ABSTRACT_KW,     // abstract
    FOR_KW,          // for
    WHILE_KW,        // while
    CONTINUE_KW,     // continue
    BREAK_KW,        // break
    INSTANCEOF_KW,   // instanceof
    RETURN_KW,       // return
    TRANSIENT_KW,    // transient
    EXTENDS_KW,      // extends
    IMPLEMENTS_KW,   // implements
    NEW_KW,          // new
    ASSERT_KW,       // assert
    SWITCH_KW,       // switch
    CASE_KW,         // case
    DEFAULT_KW,      // default
    SYNCHRONIZED_KW, // synchronized
    DO_KW,           // do
    IF_KW,           // if
    ELSE_KW,         // else
    THIS_KW,         // this
    SUPER_KW,        // super
    VOLATILE_KW,     // volatile
    NATIVE_KW,       // native
    THROW_KW,        // throw
    THROWS_KW,       // throws
    TRY_KW,          // try
    CATCH_KW,        // catch
    FINALLY_KW,      // finally
    STRICTFP_KW,     // strictfp
    DOUBLE_KW,       // double
    INT_KW,          // int
    SHORT_KW,        // short
    LONG_KW,         // long
    FLOAT_KW,        // float
    CHAR_KW,         // char
    BOOLEAN_KW,      // boolean

    // reserved keywords
    GOTO_KW,  // goto
    CONST_KW, // const

    // Trivia
    LINE_COMMENT,
    BLOCK_COMMENT,
    JAVADOC_LINE,
    JAVADOC,
    WHITESPACE,
    UNKNOWN,

    // Internal
    IDENTIFIER,
    EOF,

    // Nodes
    MISSING,
    ERROR,

    QUALIFIED_NAME,
    TYPE,
    NAME_REF,

    ASSIGNMENT_EXPR, // a = 1
    POSTFIX_EXPR,    // i++, i--
    PREFIX_EXPR,     // ++i, --i
    METHOD_CALL,     // method()
    NEW_EXPR,        // new Object()
    METHOD_REFERENCE,
    SUPER_EXPR,
    CAST_EXPR,
    SWITCH_EXPR,
    COND_EXPR,
    ASSIGN_EXPR,
    PRIMITIVE_TYPE_EXPR,
    LITERAL,
    CLASS_LITERAL,
    PAREN_EXPR,
    UNARY_EXPR,
    FIELD_ACCESS,
    ARRAY_ACCESS,
    BINARY_EXPR,

    TYPE_PARAMETERS,
    TYPE_PARAMETER,
    TYPE_BOUND,
    TYPE_ARGUMENTS,
    TYPE_ARGUMENT,
    WILDCARD_TYPE,
    WILDCARD_BOUNDS,

    VARIABLE_DECLARATOR_LIST,
    VARIABLE_DECLARATOR,

    LOCAL_VARIABLE_DECLARATION_STMT,
    LOCAL_VARIABLE_DECLARATION,
    EXPRESSION_STMT,
    EMPTY_STMT,
    YIELD_STMT,
    RETURN_STMT,
    THROW_STMT,
    BREAK_STMT,
    CONTINUE_STMT,
    ASSERT_STMT,
    IF_STMT,
    WHILE_STMT,
    SWITCH_STMT,
    SYNCHRONIZED_STMT,
    DO_STMT,
    TRY_STMT,
    FOR_STMT,
    ENHANCED_FOR_STMT,
    TRY_WITH_RESOURCES_STMT,
    LABELED_STMT,
    RESOURCE_SPECIFICATION,
    RESOURCE,
    VARIABLE_ACCESS,
    PARENTHESIZED_EXPR,

    DIMENSION,
    DIMENSIONS,
    ARRAY_TYPE,
    ARRAY_ACCESS_EXPR,
    ARRAY_INITIALIZER,

    MODIFIER_LIST,
    ARGUMENT_LIST,
    FORMAL_PARAMETERS,
    FORMAL_PARAMETER,
    SPREAD_PARAMETER,
    ANNOTATION,
    MARKER_ANNOTATION,
    ANNOTATION_ARGUMENT_LIST,
    ELEMENT_VALUE_PAIR,

    CLASS_DECL,
    PACKAGE_DECL,
    IMPORT_DECL,
    IMPORT_PATH,
    FIELD_DECL,
    METHOD_DECL,
    INTERFACE_DECL,
    ANNOTATION_TYPE_DECL,
    ANNOTATION_TYPE_ELEMENT_DECL,
    RECORD_DECL,
    ENUM_DECL,
    MODULE_DECL,

    MODULE_NAME,
    REQUIRES_DIRECTIVE,
    EXPORTS_DIRECTIVE,
    OPENS_DIRECTIVE,
    USES_DIRECTIVE,
    PROVIDES_DIRECTIVE,

    COMPACT_CONSTRUCTOR_DECL,
    CONSTRUCTOR_DECL,
    EMPTY_DECL,

    ENUM_CONSTANT,

    STATIC_INITIALIZER,
    INSTANCE_INITIALIZER,

    BLOCK, // { ... }

    SWITCH_BLOCK,
    SWITCH_RULE,
    SWITCH_BLOCK_STATEMENT_GROUP,
    SWITCH_LABEL,

    TYPE_PATTERN,
    RECORD_PATTERN,
    MATCH_ALL_PATTERN,

    CLASS_BODY,
    ENUM_BODY,
    INTERFACE_BODY,
    RECORD_BODY,
    ANNOTATION_TYPE_BODY,
    MODULE_BODY,

    EXTENDS_CLAUSE,           // extends <super>
    THROWS_CLAUSE,            // throws <exception a>, <exception b>
    INTERFACE_EXTENDS_CLAUSE, // interface <identifier> extends A, B
    IMPLEMENTS_CLAUSE,        // implements <interface 1>, <interface 2>
    CATCH_CLAUSE,
    CATCH_FORMAL_PARAMETER,
    FINALLY_CLAUSE,

    // The root node
    // This should be the last variant.
    ROOT,
}

impl SyntaxKind {
    pub fn is_trivia(&self) -> bool {
        matches!(
            self,
            Self::WHITESPACE
                | Self::LINE_COMMENT
                | Self::BLOCK_COMMENT
                | Self::JAVADOC
                | Self::JAVADOC_LINE
        )
    }
}

impl From<SyntaxKind> for rowan::SyntaxKind {
    fn from(kind: SyntaxKind) -> Self {
        Self(kind as u16)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum ContextualKeyword {
    Record,
    Sealed,
    NonSealed,
    Permits,
    Yield,
    Var,
    When,
    Module,
    Open,
    Requires,
    Opens,
    Exports,
    Uses,
    Provides,
    Transitive,
    To,
    With,
}

impl ContextualKeyword {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextualKeyword::Record => "record",
            ContextualKeyword::Sealed => "sealed",
            ContextualKeyword::NonSealed => "non-sealed",
            ContextualKeyword::Permits => "permits",
            ContextualKeyword::Yield => "yield",
            ContextualKeyword::Var => "var",
            ContextualKeyword::When => "when",
            ContextualKeyword::Module => "module",
            ContextualKeyword::Open => "open",
            ContextualKeyword::Requires => "requires",
            ContextualKeyword::Opens => "opens",
            ContextualKeyword::Exports => "exports",
            ContextualKeyword::Uses => "uses",
            ContextualKeyword::Provides => "provides",
            ContextualKeyword::Transitive => "transitive",
            ContextualKeyword::To => "to",
            ContextualKeyword::With => "with",
        }
    }
}

impl FromStr for ContextualKeyword {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sealed" => Ok(Self::Sealed),
            "non-sealed" => Ok(Self::NonSealed),
            "yield" => Ok(Self::Yield),
            "record" => Ok(Self::Record),
            "var" => Ok(Self::Var),
            "permits" => Ok(Self::Permits),
            "when" => Ok(Self::When),
            "module" => Ok(Self::Module),
            "open" => Ok(Self::Open),
            "requires" => Ok(Self::Requires),
            "opens" => Ok(Self::Opens),
            "exports" => Ok(Self::Exports),
            "uses" => Ok(Self::Uses),
            "provides" => Ok(Self::Provides),
            "transitive" => Ok(Self::Transitive),
            "to" => Ok(Self::To),
            "with" => Ok(Self::With),
            _ => Err(()),
        }
    }
}
