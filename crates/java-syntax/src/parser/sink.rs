use rowan::{GreenNode, GreenNodeBuilder};

use crate::{lexer::token::Token, parser::Event};

pub struct Sink<'a> {
    tokens: Vec<Token<'a>>,
    events: Vec<Event>,
    builder: GreenNodeBuilder<'static>,
}

impl<'a> Sink<'a> {
    pub fn new(tokens: Vec<Token<'a>>, events: Vec<Event>) -> Self {
        Self {
            tokens,
            events,
            builder: GreenNodeBuilder::new(),
        }
    }

    pub fn finish(self) -> GreenNode {
        self.builder.finish()
    }
}
