#[derive(Debug)]
pub struct JavaToken<'source> {
    pub token_type: TokenType,
    pub lexeme: &'source str,
    pub offset: usize, // the start position of the token
}

impl<'s> JavaToken<'s> {
    pub fn new(token_type: TokenType, lexeme: &'s str, offset: usize) -> Self {
        Self {
            token_type,
            lexeme,
            offset,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum TokenType {
    LeftParen,  // (
    RightParen, // )

    LeftBrace,  // {
    RightBrace, // }

    LeftBracket,  // [
    RightBracket, // ]

    StringLiteral, // ""
    StringTemplateBegin,
    StringTemplateMid,
    StringTemplateEnd,
    TextBlockTemplateBegin,
    TextBlockTemplateMid,
    TextBlockTemplateEnd,
    CharLiteral,      // ''
    Semicolon,        // ;
    Dot,              // .
    At,               // @
    Plus,             // +
    Minus,            // -
    Star,             // *
    Slash,            // /
    LessEq,           // <=
    Less,             // <
    Greater,          // >
    GreaterEq,        // >=
    EqualEqual,       // ==
    Equal,            // =
    Shl,              // <<
    Shr,              // >>
    Or,               // ||
    BitOr,            // |
    BitOrEqual,       // |=
    OrEqual,          // |=
    And,              // &&
    BitAnd,           // &
    AndEqual,         // &=
    Not,              // !
    Modulo,           //
    Caret,            // ^
    DivideEqual,      // /=
    NotEqual,         // !=
    MultipleEqual,    // *=
    PlusEqual,        // +=
    PlusPlus,         // ++
    MinusEqual,       // -=
    MinusMinus,       // --
    XorEqual,         // ^=
    ModuloEqual,      // %=
    ShrEqual,         // >>=
    ShlEqual,         // <<=
    UnsignedShrEqual, // <<<=
    UnsignedShr,      // <<<
    Comma,            // ,
    Question,         // ?
    Arrow,            // ->
    ColonColon,       // ::
    Colon,            // :
    Ellipsis,         // ...
    TextBlock,        // """ """
    NumberLiteral,

    // Keywords
    Package,
    Import,
    Class,
    Public,
    Private,
    Protected,
    Final,
    Static,
    Void,
    Byte,
    Enum,
    Interface,
    Abstract,
    For,
    While,
    Continue,
    Break,
    Instanceof,
    Return,
    Transient,
    Extends,
    Implements,
    New,
    Assert,
    Switch,
    Default,
    Synchronized,
    Do,
    If,
    Else,
    This,
    Super,
    Volatile,
    Native,
    Throw,
    Throws,
    Try,
    Catch,
    Finally,
    Strictfp,
    Double,
    Int,
    Short,
    Long,
    Float,
    Char,
    Boolean,
    Null,
    True,
    False,
    Goto,
    Const,

    // Trivia
    LineComment,
    BlockComment,
    Javadoc,
    Whitespace,
    Unknown,

    // Internal
    Identifier,
    Eof,
}
impl TokenType {
    pub fn parse(text: &str) -> TokenType {
        match text {
            "package" => TokenType::Package,
            "import" => TokenType::Import,
            "class" => TokenType::Class,
            "enum" => TokenType::Enum,
            "interface" => TokenType::Interface,
            "public" => TokenType::Public,
            "private" => TokenType::Private,
            "final" => TokenType::Final,
            "static" => TokenType::Static,
            "protected" => TokenType::Protected,
            "abstract" => TokenType::Abstract,
            "for" => TokenType::For,
            "while" => TokenType::While,
            "continue" => TokenType::Continue,
            "break" => TokenType::Break,
            "instanceof" => TokenType::Instanceof,
            "return" => TokenType::Return,
            "transient" => TokenType::Transient,
            "extends" => TokenType::Extends,
            "implements" => TokenType::Implements,
            "new" => TokenType::New,
            "assert" => TokenType::Assert,
            "switch" => TokenType::Switch,
            "default" => TokenType::Default,
            "synchronized" => TokenType::Synchronized,
            "do" => TokenType::Do,
            "if" => TokenType::If,
            "else" => TokenType::Else,
            "this" => TokenType::This,
            "super" => TokenType::Super,
            "volatile" => TokenType::Volatile,
            "native" => TokenType::Native,
            "throw" => TokenType::Throw,
            "throws" => TokenType::Throws,
            "try" => TokenType::Try,
            "catch" => TokenType::Catch,
            "finally" => TokenType::Finally,
            "strictfp" => TokenType::Strictfp,

            // primitive types
            "void" => TokenType::Void,
            "double" => TokenType::Double,
            "int" => TokenType::Int,
            "short" => TokenType::Short,
            "long" => TokenType::Long,
            "float" => TokenType::Float,
            "char" => TokenType::Char,
            "boolean" => TokenType::Boolean,
            "byte" => TokenType::Byte,

            // Seems like keywords but they are actually literals
            "null" => TokenType::Null,
            "true" => TokenType::True,
            "false" => TokenType::False,

            // reserved keywords
            "goto" => TokenType::Goto,
            "const" => TokenType::Const,

            _ => TokenType::Identifier,
        }
    }

    pub fn is_trivia(&self) -> bool {
        matches!(
            self,
            TokenType::Whitespace | TokenType::LineComment | TokenType::BlockComment
        )
    }
}
