use rowan::{GreenNode, GreenNodeBuilder, NodeCache};

use crate::{lexer::token::Token, parser::Event};

pub struct Sink<'a> {
    tokens: Vec<Token<'a>>,
    events: Vec<Event<'a>>,
    builder: GreenNodeBuilder<'a>,
    cursor: usize, // raw token cursor, includes trivia
}

impl<'a> Sink<'a> {
    pub fn new(
        tokens: Vec<Token<'a>>,
        events: Vec<Event<'a>>,
        cache: Option<&'a mut NodeCache>,
    ) -> Self {
        let builder = if let Some(cache) = cache {
            GreenNodeBuilder::with_cache(cache)
        } else {
            GreenNodeBuilder::new()
        };

        Self {
            tokens,
            events,
            builder,
            cursor: 0,
        }
    }

    fn eat_trivia(&mut self) {
        while let Some(token) = self.tokens.get(self.cursor) {
            if !token.kind.is_trivia() {
                break;
            }
            self.builder.token(token.kind.into(), token.lexeme);
            self.cursor += 1;
        }
    }

    fn token(&mut self) {
        self.eat_trivia();

        let Some(token) = self.tokens.get(self.cursor) else {
            return;
        };

        debug_assert!(!token.kind.is_trivia());

        self.builder.token(token.kind.into(), token.lexeme);
        self.cursor += 1;
    }

    fn build(&mut self) {
        // finish root node event
        let last_finish_idx = self
            .events
            .iter()
            .enumerate()
            .rev()
            .find(|(_, e)| matches!(e, Event::FinishNode))
            .map(|(i, _)| i);

        for idx in 0..self.events.len() {
            let event = std::mem::replace(&mut self.events[idx], Event::Tombstone);

            match event {
                Event::Tombstone => {}
                Event::AddToken => {
                    self.token();
                }
                Event::AddVirtualToken { kind, lexeme } => self.builder.token(kind.into(), lexeme),
                Event::AdvanceSource => self.cursor += 1,
                Event::Error(_err) => {
                    continue;
                }
                Event::StartNode {
                    kind,
                    forward_parent,
                } => {
                    let mut kinds = vec![kind];
                    let mut current_idx = idx;
                    let mut fp = forward_parent;

                    while let Some(parent_offset) = fp {
                        current_idx += parent_offset;
                        fp = match std::mem::replace(
                            &mut self.events[current_idx],
                            Event::Tombstone,
                        ) {
                            Event::StartNode {
                                kind,
                                forward_parent,
                            } => {
                                kinds.push(kind);
                                forward_parent
                            }
                            _ => unreachable!("forward_parent must point to StartNode"),
                        };
                    }

                    for kind in kinds.into_iter().rev() {
                        self.builder.start_node(kind.into());
                        self.eat_trivia();
                    }
                }
                Event::FinishNode => {
                    if Some(idx) == last_finish_idx {
                        // ROOT node
                        // consume trivia
                        self.eat_trivia();
                    }
                    self.builder.finish_node();
                }
            }
        }
    }

    pub fn finish(mut self) -> GreenNode {
        self.build();
        self.builder.finish()
    }
}
