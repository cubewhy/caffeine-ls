use std::str::FromStr;

use rowan::{GreenNode, NodeCache};

use crate::{
    kinds::{
        ContextualKeyword,
        SyntaxKind::{self, *},
    },
    lex,
    lexer::token::Token,
    parser::{checkpoint::Checkpoint, marker::Marker, reader::TokenSource, sink::Sink},
};

mod checkpoint;
pub mod grammar;
mod marker;
mod reader;
mod sink;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lang {}

impl rowan::Language for Lang {
    type Kind = SyntaxKind;
    fn kind_from_raw(raw: rowan::SyntaxKind) -> Self::Kind {
        assert!(raw.0 <= ROOT as u16);
        unsafe { std::mem::transmute::<u16, SyntaxKind>(raw.0) }
    }
    fn kind_to_raw(kind: Self::Kind) -> rowan::SyntaxKind {
        kind.into()
    }
}

pub struct Parse {
    green_node: GreenNode,
    #[allow(unused)]
    errors: Vec<ParseError>,
}

impl Parse {
    pub fn syntax_node(&self) -> rowan::SyntaxNode<Lang> {
        rowan::SyntaxNode::new_root(self.green_node.clone())
    }

    pub fn into_syntax_node(self) -> rowan::SyntaxNode<Lang> {
        rowan::SyntaxNode::new_root(self.green_node)
    }

    pub fn into_green_node(self) -> rowan::GreenNode {
        self.green_node
    }

    pub fn errors(&self) -> &[ParseError] {
        &self.errors
    }

    pub fn debug_dump(&self) -> String {
        format!("{:#?}", self.syntax_node())
    }
}

pub enum Event<'a> {
    Tombstone,
    AddToken,
    AddVirtualToken {
        kind: SyntaxKind,
        lexeme: &'a str,
    },
    AdvanceSource,
    Error(ParseError),
    StartNode {
        kind: SyntaxKind,
        forward_parent: Option<usize>,
    },
    FinishNode,
}

pub struct Parser<'a> {
    source: TokenSource<'a>,
    pub events: Vec<Event<'a>>,
    pub errors: Vec<ParseError>,
    override_token: Option<Token<'a>>,
}

impl<'a> Parser<'a> {
    pub fn new(tokens: Vec<Token<'a>>) -> Self {
        Self {
            source: TokenSource::new(tokens),
            errors: Vec::new(),
            events: Vec::new(),
            override_token: None,
        }
    }

    pub fn parse_with_cache(mut self, cache: Option<&'a mut NodeCache>) -> Parse {
        grammar::root(&mut self);

        let green_node = Sink::new(self.source.into_inner(), self.events, cache).finish();

        Parse {
            green_node,
            errors: self.errors,
        }
    }

    pub fn parse(self) -> Parse {
        self.parse_with_cache(None)
    }

    pub(crate) fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            source_pos: self.source.pos(),
            events_len: self.events.len(),
            errors_len: self.errors.len(),
        }
    }

    pub(crate) fn rewind(&mut self, cp: Checkpoint) {
        self.source.set_pos(cp.source_pos);
        self.events.truncate(cp.events_len);
        self.errors.truncate(cp.errors_len);
    }

    pub(crate) fn start(&mut self) -> Marker {
        let pos = self.events.len();
        self.events.push(Event::Tombstone);
        Marker::new(pos)
    }

    pub(crate) fn pos(&self) -> usize {
        self.source.pos()
    }

    pub(crate) fn current(&self) -> Option<SyntaxKind> {
        self.override_token
            .as_ref()
            .map(|it| it.kind)
            .or_else(|| self.source.current())
    }

    pub(crate) fn current_lexeme(&'a self) -> Option<&'a str> {
        self.source.current_lexeme()
    }

    pub(crate) fn nth(&self, n: usize) -> Option<SyntaxKind> {
        if let Some(over) = &self.override_token {
            if n == 0 {
                return Some(over.kind);
            }
            return self.source.nth(n - 1).map(|t| t.kind);
        }
        self.source.nth(n).map(|t| t.kind)
    }

    pub(crate) fn bump(&mut self) {
        if let Some(over) = self.override_token.take() {
            self.events.push(Event::AddVirtualToken {
                kind: over.kind,
                lexeme: over.lexeme,
            });

            self.events.push(Event::AdvanceSource);
            self.source.bump();
        } else if !self.source.is_at_end() {
            self.events.push(Event::AddToken);
            self.source.bump();
        }
    }

    pub(crate) fn at_contextual_kw(&self, kw: ContextualKeyword) -> bool {
        self.current() == Some(IDENTIFIER) && self.current_lexeme() == Some(kw.as_str())
    }

    pub(crate) fn nth_at_contextual_kw(&self, n: usize, kw: ContextualKeyword) -> bool {
        let Some(token) = self.source.nth(n) else {
            return false;
        };

        token.kind == IDENTIFIER && token.lexeme == kw.as_str()
    }

    pub(crate) fn eat_contextual_kw(&mut self, kw: ContextualKeyword) -> bool {
        if self.at_contextual_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn expect_contextual_kw(&mut self, kw: ContextualKeyword) {
        if !self.eat_contextual_kw(kw) {
            self.error_message("expected contextual keyword");
        }
    }

    pub(crate) fn error_message(&mut self, msg: &'static str) {
        self.error(ParseErrorKind::Message(msg));
    }

    pub(crate) fn error(&mut self, error_kind: ParseErrorKind) {
        let error = ParseError::new(error_kind, self.pos());

        self.errors.push(error.clone());
        self.events.push(Event::Error(error));
    }

    pub(crate) fn expect(&mut self, kind: SyntaxKind) -> bool {
        if !self.eat(kind) {
            self.error_expected(&[kind]);
            false
        } else {
            true
        }
    }

    pub(crate) fn error_expected(&mut self, expected: &[SyntaxKind]) {
        self.error(ParseErrorKind::ExpectedToken {
            expected: expected.to_vec(),
            found: self.current(),
        });

        // insert MISSING node
        let m = self.start();
        m.complete(self, SyntaxKind::MISSING);
    }

    pub(crate) fn error_expected_construct(&mut self, construct: ExpectedConstruct) {
        self.error(ParseErrorKind::ExpectedConstruct(construct));
    }

    pub(crate) fn is_at_end(&self) -> bool {
        self.source.is_at_end()
    }

    pub(crate) fn at(&self, kind: SyntaxKind) -> bool {
        self.current() == Some(kind)
    }

    pub(crate) fn at_set(&self, set: TokenSet) -> bool {
        self.current().is_some_and(|kind| set.contains(kind as u16))
    }

    pub fn at_contextual_kw_set(&self, set: TokenSet) -> bool {
        if self.at(IDENTIFIER) {
            let Some(text) = self.current_lexeme() else {
                return false;
            };
            if let Ok(kw) = ContextualKeyword::from_str(text) {
                return set.contains(kw as u16);
            }
        }
        false
    }

    pub(crate) fn eat(&mut self, kind: SyntaxKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    pub(crate) fn matches(&self, kinds: &[SyntaxKind]) -> bool {
        kinds
            .iter()
            .enumerate()
            .all(|(i, &kind)| self.nth(i) == Some(kind))
    }

    pub(crate) fn split_token(
        &mut self,
        first_kind: SyntaxKind,
        first_len: usize,
        rest_kind: SyntaxKind,
    ) {
        let Some(old_token) = self.source.nth(0) else {
            return;
        };
        let (head, tail) = old_token.lexeme.split_at(first_len);

        self.events.push(Event::AddVirtualToken {
            kind: first_kind,
            lexeme: head,
        });

        self.override_token = Some(Token {
            kind: rest_kind,
            lexeme: tail,
            offset: old_token.offset + first_len,
        });
    }
}

/// Lex and parse a syntax tree
///
/// NOTE: Never call this function in production!
pub fn parse(input: &str) -> Parse {
    let tokens = lex(input).0;
    Parser::new(tokens).parse()
}

#[derive(Clone, Copy)]
pub struct TokenSet {
    bits: [u64; 4],
}

impl TokenSet {
    pub fn contains(&self, kind: u16) -> bool {
        let index = (kind >> 6) as usize;
        let mask = 1u64 << (kind & 63);

        index < 4 && (self.bits[index] & mask) != 0
    }
}

#[macro_export]
macro_rules! tokenset {
    ($($kind:expr),* $(,)?) => {{
        let mut bits = [0u64; 4];
        $(
            let k = $kind as u16;
            bits[(k >> 6) as usize] |= 1u64 << (k & 63);
        )*
        $crate::parser::TokenSet { bits }
    }};
}

#[derive(Clone, Debug)]
pub struct ParseError {
    pub kind: ParseErrorKind,
    pub pos: usize,
}

impl ParseError {
    fn new(kind: ParseErrorKind, pos: usize) -> Self {
        Self { kind, pos }
    }
}

#[derive(Clone, Debug)]
pub enum ParseErrorKind {
    ExpectedToken {
        expected: Vec<SyntaxKind>,
        found: Option<SyntaxKind>,
    },
    ExpectedContextualKeyword {
        keyword: ContextualKeyword,
        found: Option<SyntaxKind>,
    },
    ExpectedConstruct(ExpectedConstruct),
    Message(&'static str),
}

#[derive(Debug, Clone)]
pub enum ExpectedConstruct {
    Declaration,
    TypeDeclaration,
    MemberDeclaration,
    Expression,
    Statement,
    Type,
    Resource,
    QualifiedName,
    Pattern,
    ModuleDirective,
}
