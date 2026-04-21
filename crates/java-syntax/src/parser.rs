use rowan::{GreenNode, NodeOrToken};

use crate::{
    kinds::{
        ContextualKeyword,
        SyntaxKind::{self, *},
    },
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

    pub fn errors(&self) -> &[ParseError] {
        &self.errors
    }

    pub fn debug_dump(&self) -> String {
        fn walk(node: rowan::SyntaxNode<Lang>, level: usize, out: &mut String) {
            let indent = "  ".repeat(level);
            out.push_str(&format!("{indent}{:?}\n", node.kind()));

            for child in node.children_with_tokens() {
                match child {
                    NodeOrToken::Node(n) => walk(n, level + 1, out),
                    NodeOrToken::Token(t) => {
                        let indent = "  ".repeat(level + 1);
                        out.push_str(&format!("{indent}{:?} {:?}\n", t.kind(), t.text()));
                    }
                }
            }
        }

        let mut out = String::new();
        walk(self.syntax_node(), 0, &mut out);

        if !self.errors.is_empty() {
            out.push_str("errors:\n");
            for err in &self.errors {
                out.push_str(&format!("  {:?}\n", err));
            }
        }

        out
    }
}

pub enum Event {
    Tombstone,
    AddToken,
    Error(ParseError),
    StartNode {
        kind: SyntaxKind,
        forward_parent: Option<usize>,
    },
    FinishNode,
}

pub struct Parser<'a> {
    source: TokenSource<'a>,
    pub events: Vec<Event>,
    pub errors: Vec<ParseError>,
}

impl<'a> Parser<'a> {
    pub fn new(tokens: Vec<Token<'a>>) -> Self {
        Self {
            source: TokenSource::new(tokens),
            errors: Vec::new(),
            events: Vec::new(),
        }
    }

    pub fn parse(mut self) -> Parse {
        grammar::root(&mut self);
        let green_node = Sink::new(self.source.into_inner(), self.events).finish();

        Parse {
            green_node,
            errors: self.errors,
        }
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
        self.source.current()
    }

    pub(crate) fn current_lexeme(&'a self) -> Option<&'a str> {
        self.source.current_lexeme()
    }

    pub(crate) fn nth(&self, n: usize) -> Option<SyntaxKind> {
        self.source.nth(n).map(|token| token.kind)
    }

    pub(crate) fn bump(&mut self) {
        if !self.source.is_at_end() {
            self.events.push(Event::AddToken);
            self.source.bump();
        }
    }

    pub(crate) fn at_contextual_kw(&self, kw: ContextualKeyword) -> bool {
        self.current() == Some(IDENTIFIER) && self.current_lexeme() == Some(kw.as_str())
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

    pub(crate) fn error_and_bump(&mut self, msg: &'static str) {
        self.error(ParseErrorKind::Message(msg));
        if !self.source.is_at_end() {
            self.bump();
        }
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

    pub(crate) fn eat(&mut self, kind: SyntaxKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }
}

pub fn parse(input: &str) -> Parse {
    let tokens = match crate::lexer::lex(input) {
        Ok(tokens) => tokens,
        Err((tokens, _errors)) => tokens,
    };
    crate::parser::Parser::new(tokens).parse()
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
}
