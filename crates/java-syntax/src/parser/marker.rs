use drop_bomb::DropBomb;

use crate::{
    parser::{Event, Parser},
    syntax_kind::SyntaxKind,
};

pub struct Marker {
    pub pos: usize,
    bomb: DropBomb,
}

impl Marker {
    pub fn new(pos: usize) -> Self {
        let bomb = DropBomb::new("Marker should be explicitly completed");

        Self { pos, bomb }
    }

    pub fn complete(mut self, p: &mut Parser, kind: SyntaxKind) -> CompletedMarker {
        self.bomb.defuse();

        let idx = self.pos;
        match p.events[idx] {
            Event::Tombstone => {
                p.events[idx] = Event::StartNode {
                    kind,
                    forward_parent: None,
                };
            }
            _ => unreachable!("try to consume marker twice. that's definitely impossible!"),
        }

        p.events.push(Event::FinishNode);
        CompletedMarker::new(self.pos, kind)
    }

    pub fn abandon(mut self, p: &mut Parser) {
        self.bomb.defuse();
        let idx = self.pos;
        if !matches!(p.events[idx], Event::Tombstone) {
            unreachable!("abandon on completed marker");
        }
    }
}

pub struct CompletedMarker {
    pub pos: usize,
    kind: SyntaxKind,
}

impl CompletedMarker {
    pub fn new(pos: usize, kind: SyntaxKind) -> Self {
        Self { pos, kind }
    }

    pub fn precede(self, p: &mut Parser) -> Marker {
        let new_pos = p.events.len();
        p.events.push(Event::Tombstone);

        let idx = self.pos;
        match &mut p.events[idx] {
            Event::StartNode { forward_parent, .. } => {
                assert!(forward_parent.is_none(), "forward_parent is not None");
                *forward_parent = Some(new_pos - self.pos);
            }
            _ => unreachable!("precede on non-start event"),
        }

        Marker::new(new_pos)
    }

    pub fn kind(&self) -> SyntaxKind {
        self.kind
    }
}
