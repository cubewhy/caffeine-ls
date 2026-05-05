use derive_more::Display;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Display)]
#[repr(u16)]
#[allow(non_camel_case_types)]
pub enum SyntaxKind {
    #[display("(")]
    L_PAREN, // (
    #[display(")")]
    R_PAREN, // )

    #[display("{{")]
    L_BRACE, // {

    #[display("}}")]
    R_BRACE, // }

    #[display("[")]
    L_BRACKET, // [
    #[display("]")]
    R_BRACKET, // ]

    #[display("a string")]
    STRING_LITERAL, // ""
    #[display("a number")]
    NUMBER_LITERAL, // dec, hex, oct, bin
    STRING_TEMPLATE_BEGIN,
    STRING_TEMPLATE_MID,
    STRING_TEMPLATE_END,
    TEXT_BLOCK_TEMPLATE_BEGIN,
    TEXT_BLOCK_TEMPLATE_MID,
    TEXT_BLOCK_TEMPLATE_END,

    #[display("null")]
    NULL_LITERAL, // null
    #[display("true")]
    TRUE_LITERAL, // true
    #[display("false")]
    FALSE_LITERAL, // false
    CHAR_LITERAL, // ''
    #[display(";")]
    SEMICOLON, // ;
    #[display(".")]
    DOT, // .
    #[display("@")]
    AT, // @
    #[display("+")]
    PLUS, // +
    #[display("-")]
    MINUS, // -
    #[display("*")]
    STAR, // *
    #[display("/")]
    SLASH, // /
    #[display("<=")]
    LESS_EQUAL, // <=
    #[display("<")]
    LESS, // <
    #[display(">")]
    GREATER, // >
    #[display(">=")]
    GREATER_EQUAL, // >=
    #[display("==")]
    EQUAL_EQUAL, // ==
    #[display("=")]
    EQUAL, // =
    #[display("||")]
    OR, // ||
    #[display("|")]
    BIT_OR, // |
    #[display("|=")]
    OR_EQUAL, // |=
    #[display("&&")]
    AND, // &&
    #[display("&")]
    BIT_AND, // &
    #[display("&=")]
    AND_EQUAL, // &=
    #[display("!")]
    NOT, // !
    #[display("~")]
    TILDE, // ~
    #[display("%")]
    MODULO, // %
    #[display("^")]
    CARET, // ^
    #[display("/=")]
    DIVIDE_EQUAL, // /=
    #[display("!=")]
    NOT_EQUAL, // !=
    #[display("*=")]
    MULTIPLE_EQUAL, // *=
    #[display("+=")]
    PLUS_EQUAL, // +=
    #[display("++")]
    PLUS_PLUS, // ++
    #[display("-=")]
    MINUS_EQUAL, // -=
    #[display("--")]
    MINUS_MINUS, // --
    #[display("^=")]
    XOR_EQUAL, // ^=
    #[display("%=")]
    MODULO_EQUAL, // %=
    #[display("<<=")]
    LEFT_SHIFT_EQUAL, // <<=
    #[display(">>=")]
    RIGHT_SHIFT_EQUAL, // >>=
    #[display(">>>=")]
    UNSIGNED_RIGHT_SHIFT_EQUAL, // >>>=
    #[display("<<")]
    LEFT_SHIFT, // <<
    #[display(">>")]
    RIGHT_SHIFT, // >>
    #[display(">>>")]
    UNSIGNED_RIGHT_SHIFT, // >>>
    #[display(",")]
    COMMA, // ,
    #[display("?")]
    QUESTION, // ?
    #[display("->")]
    ARROW, // ->
    #[display("::")]
    COLON_COLON, // ::
    #[display(":")]
    COLON, // :
    #[display("...")]
    ELLIPSIS, // ...
    TEXT_BLOCK,   // """ """
    #[display("_")]
    UNDERSCORE, // _

    // Keywords
    #[display("package")]
    PACKAGE_KW, // package
    #[display("import")]
    IMPORT_KW, // import
    #[display("class")]
    CLASS_KW, // class
    #[display("public")]
    PUBLIC_KW, // public
    #[display("private")]
    PRIVATE_KW, // private
    #[display("protected")]
    PROTECTED_KW, // protected
    #[display("final")]
    FINAL_KW, // final
    #[display("static")]
    STATIC_KW, // static
    #[display("void")]
    VOID_KW, // void
    #[display("byte")]
    BYTE_KW, // byte
    #[display("enum")]
    ENUM_KW, // enum
    #[display("interface")]
    INTERFACE_KW, // interface
    #[display("abstract")]
    ABSTRACT_KW, // abstract
    #[display("for")]
    FOR_KW, // for
    #[display("while")]
    WHILE_KW, // while
    #[display("continue")]
    CONTINUE_KW, // continue
    #[display("break")]
    BREAK_KW, // break
    #[display("instanceof")]
    INSTANCEOF_KW, // instanceof
    #[display("return")]
    RETURN_KW, // return
    #[display("transient")]
    TRANSIENT_KW, // transient
    #[display("extends")]
    EXTENDS_KW, // extends
    #[display("implements")]
    IMPLEMENTS_KW, // implements
    #[display("new")]
    NEW_KW, // new
    #[display("assert")]
    ASSERT_KW, // assert
    #[display("switch")]
    SWITCH_KW, // switch
    #[display("case")]
    CASE_KW, // case
    #[display("default")]
    DEFAULT_KW, // default
    #[display("synchronized")]
    SYNCHRONIZED_KW, // synchronized
    #[display("do")]
    DO_KW, // do
    #[display("if")]
    IF_KW, // if
    #[display("else")]
    ELSE_KW, // else
    #[display("this")]
    THIS_KW, // this
    #[display("super")]
    SUPER_KW, // super
    #[display("volatile")]
    VOLATILE_KW, // volatile
    #[display("native")]
    NATIVE_KW, // native
    #[display("throw")]
    THROW_KW, // throw
    #[display("throws")]
    THROWS_KW, // throws
    #[display("try")]
    TRY_KW, // try
    #[display("catch")]
    CATCH_KW, // catch
    #[display("finally")]
    FINALLY_KW, // finally
    #[display("strictfp")]
    STRICTFP_KW, // strictfp
    #[display("double")]
    DOUBLE_KW, // double
    #[display("int")]
    INT_KW, // int
    #[display("short")]
    SHORT_KW, // short
    #[display("long")]
    LONG_KW, // long
    #[display("float")]
    FLOAT_KW, // float
    #[display("char")]
    CHAR_KW, // char
    #[display("boolean")]
    BOOLEAN_KW, // boolean

    // reserved keywords
    #[display("goto")]
    GOTO_KW, // goto
    #[display("const")]
    CONST_KW, // const

    // Trivia
    LINE_COMMENT,
    BLOCK_COMMENT,
    JAVADOC_LINE,
    JAVADOC,
    WHITESPACE,
    UNKNOWN,

    // Internal
    #[display("identifier")]
    IDENTIFIER,
    EOF,

    // Nodes
    #[display("missing code")]
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
    THIS_EXPR,
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

macro_rules! define_contextual_keywords {
    ($($variant:ident => $string:expr),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(u16)]
        pub enum ContextualKeyword {
            $($variant),*
        }

        impl ContextualKeyword {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $string),*
                }
            }
        }

        impl std::str::FromStr for ContextualKeyword {
            type Err = ();
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($string => Ok(Self::$variant)),*,
                    _ => Err(()),
                }
            }
        }
    };
}

define_contextual_keywords! {
    Record => "record",
    Sealed => "sealed",
    NonSealed => "non-sealed",
    Permits => "permits",
    Yield => "yield",
    Var => "var",
    When => "when",
    Module => "module",
    Open => "open",
    Requires => "requires",
    Opens => "opens",
    Exports => "exports",
    Uses => "uses",
    Provides => "provides",
    Transitive => "transitive",
    To => "to",
    With => "with",
}
