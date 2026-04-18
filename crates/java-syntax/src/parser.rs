use rowan::GreenNode;

use crate::{
    kinds::SyntaxKind::{self, *},
    lexer::token::Token,
    parser::{marker::Marker, reader::TokenSource, sink::Sink},
};

mod grammar;
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
    pos: usize,
    events: Vec<Event>,
    errors: Vec<ParseError>,
}

impl<'a> Parser<'a> {
    pub fn new(tokens: Vec<Token<'a>>) -> Self {
        Self {
            source: TokenSource::new(tokens),
            errors: Vec::new(),
            events: Vec::new(),
            pos: 0,
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

    pub(crate) fn start(&mut self) -> Marker {
        let pos = self.events.len();
        self.events.push(Event::Tombstone);
        Marker::new(pos)
    }

    pub(crate) fn current(&self) -> Option<SyntaxKind> {
        self.source.current()
    }

    pub(crate) fn nth(&self, n: usize) -> Option<SyntaxKind> {
        self.source.nth(n)
    }

    pub(crate) fn bump(&mut self) {
        if !self.source.is_at_end() {
            self.events.push(Event::AddToken);
            self.source.bump();
        }
    }

    pub(crate) fn error(&mut self, error_kind: ParseErrorKind) {
        let error = ParseError::new(error_kind, self.pos);

        self.errors.push(error.clone());
        self.events.push(Event::Error(error));
    }

    pub(crate) fn expect(&mut self, kind: SyntaxKind) {
        if !self.eat(kind) {
            self.error_expected(&[kind]);
        }
    }

    pub(crate) fn error_expected(&mut self, expected: &[SyntaxKind]) {
        self.error(ParseErrorKind::Expected(expected.to_vec()));
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
        self.current().is_some_and(|kind| set.contains(kind))
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

#[derive(Debug, Clone, Copy)]
pub struct TokenSet(&'static [SyntaxKind]);

impl TokenSet {
    pub const fn new(kinds: &'static [SyntaxKind]) -> Self {
        Self(kinds)
    }

    pub fn contains(self, kind: SyntaxKind) -> bool {
        self.0.contains(&kind)
    }
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
    Expected(Vec<SyntaxKind>),
    Message(&'static str),
}
