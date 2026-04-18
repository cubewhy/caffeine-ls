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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TokenType {
    LeftParen,  // (
    RightParen, // )

    LeftBrace,  // {
    RightBrace, // }

    LeftBracket,  // [
    RightBracket, // ]

    StringLit, // ""
    StringTemplateBegin,
    StringTemplateMid,
    StringTemplateEnd,
    TextBlockTemplateBegin,
    TextBlockTemplateMid,
    TextBlockTemplateEnd,
    NullLit,          // null
    TrueLit,          // true
    FalseLit,         // false
    CharLit,          // ''
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
    PackageKw,
    ImportKw,
    ClassKw,
    PublicKw,
    PrivateKw,
    ProtectedKw,
    FinalKw,
    StaticKw,
    VoidKw,
    ByteKw,
    EnumKw,
    InterfaceKw,
    AbstractKw,
    ForKw,
    WhileKw,
    ContinueKw,
    BreakKw,
    InstanceofKw,
    ReturnKw,
    TransientKw,
    ExtendsKw,
    ImplementsKw,
    NewKw,
    AssertKw,
    SwitchKw,
    DefaultKw,
    SynchronizedKw,
    DoKw,
    IfKw,
    ElseKw,
    ThisKw,
    SuperKw,
    VolatileKw,
    NativeKw,
    ThrowKw,
    ThrowsKw,
    TryKw,
    CatchKw,
    FinallyKw,
    StrictfpKw,
    DoubleKw,
    IntKw,
    ShortKw,
    LongKw,
    FloatKw,
    CharKw,
    BooleanKw,
    GotoKw,
    ConstKw,

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
            "package" => TokenType::PackageKw,
            "import" => TokenType::ImportKw,
            "class" => TokenType::ClassKw,
            "enum" => TokenType::EnumKw,
            "interface" => TokenType::InterfaceKw,
            "public" => TokenType::PublicKw,
            "private" => TokenType::PrivateKw,
            "final" => TokenType::FinalKw,
            "static" => TokenType::StaticKw,
            "protected" => TokenType::ProtectedKw,
            "abstract" => TokenType::AbstractKw,
            "for" => TokenType::ForKw,
            "while" => TokenType::WhileKw,
            "continue" => TokenType::ContinueKw,
            "break" => TokenType::BreakKw,
            "instanceof" => TokenType::InstanceofKw,
            "return" => TokenType::ReturnKw,
            "transient" => TokenType::TransientKw,
            "extends" => TokenType::ExtendsKw,
            "implements" => TokenType::ImplementsKw,
            "new" => TokenType::NewKw,
            "assert" => TokenType::AssertKw,
            "switch" => TokenType::SwitchKw,
            "default" => TokenType::DefaultKw,
            "synchronized" => TokenType::SynchronizedKw,
            "do" => TokenType::DoKw,
            "if" => TokenType::IfKw,
            "else" => TokenType::ElseKw,
            "this" => TokenType::ThisKw,
            "super" => TokenType::SuperKw,
            "volatile" => TokenType::VolatileKw,
            "native" => TokenType::NativeKw,
            "throw" => TokenType::ThrowKw,
            "throws" => TokenType::ThrowsKw,
            "try" => TokenType::TryKw,
            "catch" => TokenType::CatchKw,
            "finally" => TokenType::FinallyKw,
            "strictfp" => TokenType::StrictfpKw,

            // primitive types
            "void" => TokenType::VoidKw,
            "double" => TokenType::DoubleKw,
            "int" => TokenType::IntKw,
            "short" => TokenType::ShortKw,
            "long" => TokenType::LongKw,
            "float" => TokenType::FloatKw,
            "char" => TokenType::CharKw,
            "boolean" => TokenType::BooleanKw,
            "byte" => TokenType::ByteKw,

            // Seems like keywords but they are actually literals
            "null" => TokenType::NullLit,
            "true" => TokenType::TrueLit,
            "false" => TokenType::FalseLit,

            // reserved keywords
            "goto" => TokenType::GotoKw,
            "const" => TokenType::ConstKw,

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
