use rowan::GreenNode;

use crate::{
    kinds::SyntaxKind::{self, *},
    lexer::token::Token,
    parser::{marker::Marker, sink::Sink},
};

mod grammar;
mod marker;
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
    tokens: Vec<Token<'a>>,
    pos: usize,
    events: Vec<Event>,
    errors: Vec<ParseError>,
}

impl<'a> Parser<'a> {
    pub fn new(tokens: Vec<Token<'a>>) -> Self {
        Self {
            tokens,
            errors: Vec::new(),
            events: Vec::new(),
            pos: 0,
        }
    }

    pub fn parse(mut self) -> Parse {
        grammar::root(&mut self);
        let green_node = Sink::new(self.tokens, self.events).finish();

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
        self.tokens.get(self.pos).map(|t| t.kind)
    }

    pub(crate) fn bump(&mut self) {
        if self.pos < self.tokens.len() {
            self.events.push(Event::AddToken);
            self.pos += 1;
        }
    }

    pub(crate) fn report_error(&mut self, error: ParseError) {
        self.errors.push(error);
        self.events.push(Event::Error(error));
    }

    pub(crate) fn is_at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }
}

#[derive(Copy, Clone, Debug)]
pub enum ParseError {}
