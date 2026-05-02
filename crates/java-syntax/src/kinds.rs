use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, strum::Display)]
#[repr(u16)]
#[allow(non_camel_case_types)]
pub enum SyntaxKind {
    #[strum(to_string = "(")]
    L_PAREN, // (
    #[strum(to_string = ")")]
    R_PAREN, // )

    #[strum(to_string = "{")]
    L_BRACE, // {

    // https://github.com/Peternator7/strum/issues/363
    #[strum(disabled)]
    R_BRACE, // }

    #[strum(to_string = "[")]
    L_BRACKET, // [
    #[strum(to_string = "]")]
    R_BRACKET, // ]

    #[strum(to_string = "a string")]
    STRING_LITERAL, // ""
    #[strum(to_string = "a number")]
    NUMBER_LITERAL, // dec, hex, oct, bin
    STRING_TEMPLATE_BEGIN,
    STRING_TEMPLATE_MID,
    STRING_TEMPLATE_END,
    TEXT_BLOCK_TEMPLATE_BEGIN,
    TEXT_BLOCK_TEMPLATE_MID,
    TEXT_BLOCK_TEMPLATE_END,

    #[strum(to_string = "null")]
    NULL_LITERAL, // null
    #[strum(to_string = "true")]
    TRUE_LITERAL, // true
    #[strum(to_string = "false")]
    FALSE_LITERAL, // false
    CHAR_LITERAL, // ''
    #[strum(to_string = ";")]
    SEMICOLON, // ;
    #[strum(to_string = ".")]
    DOT, // .
    #[strum(to_string = "@")]
    AT, // @
    #[strum(to_string = "+")]
    PLUS, // +
    #[strum(to_string = "-")]
    MINUS, // -
    #[strum(to_string = "*")]
    STAR, // *
    #[strum(to_string = "/")]
    SLASH, // /
    #[strum(to_string = "<=")]
    LESS_EQUAL, // <=
    #[strum(to_string = "<")]
    LESS, // <
    #[strum(to_string = ">")]
    GREATER, // >
    #[strum(to_string = ">=")]
    GREATER_EQUAL, // >=
    #[strum(to_string = "==")]
    EQUAL_EQUAL, // ==
    #[strum(to_string = "=")]
    EQUAL, // =
    #[strum(to_string = "||")]
    OR, // ||
    #[strum(to_string = "|")]
    BIT_OR, // |
    #[strum(to_string = "|=")]
    OR_EQUAL, // |=
    #[strum(to_string = "&&")]
    AND, // &&
    #[strum(to_string = "&")]
    BIT_AND, // &
    #[strum(to_string = "&=")]
    AND_EQUAL, // &=
    #[strum(to_string = "!")]
    NOT, // !
    #[strum(to_string = "~")]
    TILDE, // ~
    #[strum(to_string = "%")]
    MODULO, // %
    #[strum(to_string = "^")]
    CARET, // ^
    #[strum(to_string = "/=")]
    DIVIDE_EQUAL, // /=
    #[strum(to_string = "!=")]
    NOT_EQUAL, // !=
    #[strum(to_string = "*=")]
    MULTIPLE_EQUAL, // *=
    #[strum(to_string = "+=")]
    PLUS_EQUAL, // +=
    #[strum(to_string = "++")]
    PLUS_PLUS, // ++
    #[strum(to_string = "-=")]
    MINUS_EQUAL, // -=
    #[strum(to_string = "--")]
    MINUS_MINUS, // --
    #[strum(to_string = "^=")]
    XOR_EQUAL, // ^=
    #[strum(to_string = "%=")]
    MODULO_EQUAL, // %=
    #[strum(to_string = "<<=")]
    LEFT_SHIFT_EQUAL, // <<=
    #[strum(to_string = ">>=")]
    RIGHT_SHIFT_EQUAL, // >>=
    #[strum(to_string = ">>>=")]
    UNSIGNED_RIGHT_SHIFT_EQUAL, // >>>=
    #[strum(to_string = "<<")]
    LEFT_SHIFT, // <<
    #[strum(to_string = ">>")]
    RIGHT_SHIFT, // >>
    #[strum(to_string = ">>>")]
    UNSIGNED_RIGHT_SHIFT, // >>>
    #[strum(to_string = ",")]
    COMMA, // ,
    #[strum(to_string = "?")]
    QUESTION, // ?
    #[strum(to_string = "->")]
    ARROW, // ->
    #[strum(to_string = "::")]
    COLON_COLON, // ::
    #[strum(to_string = ":")]
    COLON, // :
    #[strum(to_string = "...")]
    ELLIPSIS, // ...
    TEXT_BLOCK,   // """ """
    #[strum(to_string = "_")]
    UNDERSCORE, // _

    // Keywords
    #[strum(to_string = "package")]
    PACKAGE_KW, // package
    #[strum(to_string = "import")]
    IMPORT_KW, // import
    #[strum(to_string = "class")]
    CLASS_KW, // class
    #[strum(to_string = "public")]
    PUBLIC_KW, // public
    #[strum(to_string = "private")]
    PRIVATE_KW, // private
    #[strum(to_string = "protected")]
    PROTECTED_KW, // protected
    #[strum(to_string = "final")]
    FINAL_KW, // final
    #[strum(to_string = "static")]
    STATIC_KW, // static
    #[strum(to_string = "void")]
    VOID_KW, // void
    #[strum(to_string = "byte")]
    BYTE_KW, // byte
    #[strum(to_string = "enum")]
    ENUM_KW, // enum
    #[strum(to_string = "interface")]
    INTERFACE_KW, // interface
    #[strum(to_string = "abstract")]
    ABSTRACT_KW, // abstract
    #[strum(to_string = "for")]
    FOR_KW, // for
    #[strum(to_string = "while")]
    WHILE_KW, // while
    #[strum(to_string = "continue")]
    CONTINUE_KW, // continue
    #[strum(to_string = "break")]
    BREAK_KW, // break
    #[strum(to_string = "instanceof")]
    INSTANCEOF_KW, // instanceof
    #[strum(to_string = "return")]
    RETURN_KW, // return
    #[strum(to_string = "transient")]
    TRANSIENT_KW, // transient
    #[strum(to_string = "extends")]
    EXTENDS_KW, // extends
    #[strum(to_string = "implements")]
    IMPLEMENTS_KW, // implements
    #[strum(to_string = "new")]
    NEW_KW, // new
    #[strum(to_string = "assert")]
    ASSERT_KW, // assert
    #[strum(to_string = "switch")]
    SWITCH_KW, // switch
    #[strum(to_string = "case")]
    CASE_KW, // case
    #[strum(to_string = "default")]
    DEFAULT_KW, // default
    #[strum(to_string = "synchronized")]
    SYNCHRONIZED_KW, // synchronized
    #[strum(to_string = "do")]
    DO_KW, // do
    #[strum(to_string = "if")]
    IF_KW, // if
    #[strum(to_string = "else")]
    ELSE_KW, // else
    #[strum(to_string = "this")]
    THIS_KW, // this
    #[strum(to_string = "super")]
    SUPER_KW, // super
    #[strum(to_string = "volatile")]
    VOLATILE_KW, // volatile
    #[strum(to_string = "native")]
    NATIVE_KW, // native
    #[strum(to_string = "throw")]
    THROW_KW, // throw
    #[strum(to_string = "throws")]
    THROWS_KW, // throws
    #[strum(to_string = "try")]
    TRY_KW, // try
    #[strum(to_string = "catch")]
    CATCH_KW, // catch
    #[strum(to_string = "finally")]
    FINALLY_KW, // finally
    #[strum(to_string = "strictfp")]
    STRICTFP_KW, // strictfp
    #[strum(to_string = "double")]
    DOUBLE_KW, // double
    #[strum(to_string = "int")]
    INT_KW, // int
    #[strum(to_string = "short")]
    SHORT_KW, // short
    #[strum(to_string = "long")]
    LONG_KW, // long
    #[strum(to_string = "float")]
    FLOAT_KW, // float
    #[strum(to_string = "char")]
    CHAR_KW, // char
    #[strum(to_string = "boolean")]
    BOOLEAN_KW, // boolean

    // reserved keywords
    #[strum(to_string = "goto")]
    GOTO_KW, // goto
    #[strum(to_string = "const")]
    CONST_KW, // const

    // Trivia
    LINE_COMMENT,
    BLOCK_COMMENT,
    JAVADOC_LINE,
    JAVADOC,
    WHITESPACE,
    UNKNOWN,

    // Internal
    #[strum(to_string = "identifier")]
    IDENTIFIER,
    EOF,

    // Nodes
    #[strum(to_string = "missing code")]
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
    INSTANCEOF_EXPR,
    LAMBDA_EXPR,
    SWITCH_EXPR,
    COND_EXPR,
    ASSIGN_EXPR,
    PRIMITIVE_TYPE_EXPR,
    LITERAL,
    CLASS_LITERAL,
    TEMPLATE_EXPR,
    TEMPLATE_ARGUMENT,
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
    INFERRED_PARAMETERS,
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
    CATCH_TYPE,
    CATCH_CLAUSE,
    CATCH_FORMAL_PARAMETER,
    FINALLY_CLAUSE,

    // The root node
    // This should be the last variant.
    ROOT,
}

impl From<SyntaxKind> for String {
    fn from(val: SyntaxKind) -> Self {
        match val {
            SyntaxKind::R_BRACE => "}".to_string(),
            _ => val.to_string(),
        }
    }
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
