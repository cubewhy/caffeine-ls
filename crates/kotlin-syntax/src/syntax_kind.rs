/// An enum represent tokens and nodes in the Kotlin programming language
///
/// See https://kotlinlang.org/docs/keyword-reference.html
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
#[allow(non_camel_case_types)]
pub enum SyntaxKind {
    L_PAREN, // (
    R_PAREN, // )

    L_BRACE, // {
    R_BRACE, // }

    DOT,   // .
    COMMA, // ,

    TEMPLATE_SHORT_START,
    TEMPLATE_EXPR_START,
    ESCAPE_SEQUENCE,
    OPEN_QUOTE,
    CLOSE_QUOTE,
    OPEN_RAW_QUOTE,
    CLOSE_RAW_QUOTE,
    STRING_CONTENT,

    STRING_LITERAL,
    TEXT_BLOCK,
    INTEGER_LITERAL,
    FLOAT_LITERAL,
    CHAR_LITERAL,

    // TODO: string interpolation

    // Operators and special symbols
    // https://kotlinlang.org/docs/keyword-reference.html#operators-and-special-symbols
    PLUS,   // +
    MINUS,  // -
    STAR,   // *
    SLASH,  // /
    MODULO, // %

    EQUAL,        // =
    PLUS_EQUAL,   // +=
    MINUS_EQUAL,  // -=
    MUL_EQUAL,    // *=
    DIV_EQUAL,    // /=
    MODULO_EQUAL, // %=
    PLUS_PLUS,    // ++
    MINUS_MINUS,  // --

    AND, // &&
    OR,  // ||
    NOT, // !

    EQUAL_EQUAL, // ==
    NOT_EQUAL,   // !=
    SHEQ,        // ===
    SHNE,        // !==

    LESS,          // <
    GREATER,       // >
    LESS_EQUAL,    // <=
    GREATER_EQUAL, // >=

    L_BRACKET, // [
    R_BRACKET, // ]

    NOT_NULL_ASSERT, // !!
    SAFE_ACCESS,     // ?.
    ELVIS,           // ?:
    COLON_COLON,     // ::

    RANGE,       // ..
    RANGE_UNTIL, // ..<

    COLON,    // :
    QUESTION, // ?
    ARROW,    // ->

    AT, // @

    SEMICOLON, // ;

    DOLLAR,     // $
    UNDERSCORE, // _

    // Keywords
    AS_KW,
    AS_SAFE, // as?
    BREAK_KW,
    CLASS_KW,
    CONTINUE_KW,
    DO_KW,
    IF_KW,
    ELSE_KW,
    FALSE_KW,
    FOR_KW,
    FUN_KW,
    IN_KW,
    NOT_IN,
    INTERFACE_KW,
    IS_KW,
    NOT_IS,
    NULL_KW,
    OBJECT_KW,
    PACKAGE_KW,
    RETURN_KW,
    SUPER_KW,
    THIS_KW,
    THROW_KW,
    TRUE_KW,
    TRY_KW,
    TYPEALIAS_KW,
    TYPEOF_KW,
    VAL_KW,
    VAR_KW,
    WHEN_KW,
    WHILE_KW,

    SHEBANG_LINE,
    IDENTIFIER,
    WHITESPACE,
    KDOC,
    LINE_COMMENT,
    BLOCK_COMMENT,
    NEWLINE,

    EOF,

    MISSING,
    ERROR,

    ROOT,
}

impl SyntaxKind {
    pub fn is_trivia(&self) -> bool {
        matches!(
            self,
            Self::WHITESPACE | Self::LINE_COMMENT | Self::BLOCK_COMMENT | Self::KDOC
        )
    }

    pub fn to_quoted_string(self) -> String {
        let text = format!("{self:?}");

        if text.chars().any(|c| c.is_ascii_alphanumeric()) {
            text
        } else {
            format!("'{text}'")
        }
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

// https://kotlinlang.org/docs/keyword-reference.html#soft-keywords
define_contextual_keywords! {
    // Soft keywords
    By => "by",
    Catch => "catch",
    Constructor => "constructor",
    Delegate => "delegate",
    Dynamic => "dynamic",
    Field => "field",
    File => "file",
    Finally => "finally",
    Get => "get",
    Import => "import",
    Init => "init",
    Param => "param",
    Property => "property",
    Receiver => "receiver",
    Set => "set",
    SetParam => "setparam",
    Value => "value",
    Where => "where",

    // Modifier keywords
    Abstract => "abstract",
    Actual => "actual",
    Annotation => "annotation",
    Companion => "companion",
    Const => "const",
    CrossInline => "crossinline",
    Data => "data",
    Enum => "enum",
    Expect => "expect",
    External => "external",
    Final => "final",
    Infix => "infix",
    Inline => "inline",
    Internal => "internal",
    LateInit => "lateinit",
    NoInline => "noinline",
    Open => "open",
    Operator => "operator",
    Out => "out",
    Override => "override",
    Private => "private",
    Protected => "protected",
    Public => "public",
    Reified => "reified",
    Sealed => "sealed",
    Suspend => "suspend",
    Tailrec => "tailrec",
    Vararg => "vararg",
}
