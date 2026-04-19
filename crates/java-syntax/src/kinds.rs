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

    STRING_LIT, // ""
    NUMBER_LIT, // dec, hex, oct, bin
    STRING_TEMPLATE_BEGIN,
    STRING_TEMPLATE_MID,
    STRING_TEMPLATE_END,
    TEXT_BLOCK_TEMPLATE_BEGIN,
    TEXT_BLOCK_TEMPLATE_MID,
    TEXT_BLOCK_TEMPLATE_END,
    NULL_LIT,       // null
    TRUE_LIT,       // true
    FALSE_LIT,      // false
    CHAR_LIT,       // ''
    SEMICOLON,      // ;
    DOT,            // .
    AT,             // @
    PLUS,           // +
    MINUS,          // -
    STAR,           // *
    SLASH,          // /
    LESS_EQUAL,     // <=
    LESS,           // <
    GREATER,        // >
    GREATER_EQUAL,  // >=
    EQUAL_EQUAL,    // ==
    EQUAL,          // =
    OR,             // ||
    BIT_OR,         // |
    BIT_OR_EQUAL,   // |=
    OR_EQUAL,       // |=
    AND,            // &&
    BIT_AND,        // &
    AND_EQUAL,      // &=
    NOT,            // !
    MODULO,         //
    CARET,          // ^
    DIVIDE_EQUAL,   // /=
    NOT_EQUAL,      // !=
    MULTIPLE_EQUAL, // *=
    PLUS_EQUAL,     // +=
    PLUS_PLUS,      // ++
    MINUS_EQUAL,    // -=
    MINUS_MINUS,    // --
    XOR_EQUAL,      // ^=
    MODULO_EQUAL,   // %=
    COMMA,          // ,
    QUESTION,       // ?
    ARROW,          // ->
    COLON_COLON,    // ::
    COLON,          // :
    ELLIPSIS,       // ...
    TEXT_BLOCK,     // """ """

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
    JAVADOC,
    WHITESPACE,
    UNKNOWN,

    // Internal
    IDENTIFIER,
    EOF,

    // Nodes
    ROOT,
    MISSING,
    ERROR,

    QUALIFIED_NAME,
    TYPE,
    NAME_REF,

    TYPE_PARAMETERS,
    TYPE_PARAMETER,
    TYPE_BOUND,

    VARIABLE_DECLARATOR_LIST,
    VARIABLE_DECLARATOR,

    DIMENSIONS,
    ARRAY_TYPE,
    ARRAY_ACCESS_EXPR,
    ARRAY_INITIALIZER,

    MODIFIER_LIST,
    ARGUMENT_LIST,
    FORMAL_PARAMETERS,
    FORMAL_PARAMETER,
    ANNOTATION,
    MARKER_ANNOTATION,
    ANNOTATION_ARGUMENT_LIST,
    ELEMENT_VALUE_PAIR,

    CLASS_DECL,
    PACKAGE_DECL,
    IMPORT_DECL,
    FIELD_DECL,
    METHOD_DECL,
    INTERFACE_DECL,
    ANNOTATION_TYPE_DECL,
    RECORD_DECL,
    ENUM_DECL,
    MODULE_DECL,

    COMPACT_CONSTRUCTOR_DECL,
    CONSTRUCTOR_DECL,
    EMPTY_DECL,

    ENUM_CONSTANT,

    STATIC_INITIALIZER,
    INSTANCE_INITIALIZER,

    BLOCK, // { ... }

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
}

impl SyntaxKind {
    pub fn is_trivia(&self) -> bool {
        matches!(
            self,
            SyntaxKind::WHITESPACE | SyntaxKind::LINE_COMMENT | SyntaxKind::BLOCK_COMMENT
        )
    }
}

impl From<SyntaxKind> for rowan::SyntaxKind {
    fn from(kind: SyntaxKind) -> Self {
        Self(kind as u16)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextualKeyword {
    Record,
    Sealed,
    Permits,
    Yield,
    Var,
    When,
}

impl ContextualKeyword {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextualKeyword::Record => "record",
            ContextualKeyword::Sealed => "sealed",
            ContextualKeyword::Permits => "permits",
            ContextualKeyword::Yield => "yield",
            ContextualKeyword::Var => "var",
            ContextualKeyword::When => "when",
        }
    }
}
