use crate::{
    kinds::SyntaxKind,
    parser::{Event, Parser},
};

pub struct Marker {
    pub pos: usize,
}

impl Marker {
    pub fn new(pos: usize) -> Self {
        Self { pos }
    }

    pub fn complete(self, p: &mut Parser, kind: SyntaxKind) -> CompletedMarker {
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
        CompletedMarker::new(self.pos)
    }

    fn abandon(self, p: &mut Parser) {
        let idx = self.pos as usize;
        if !matches!(p.events[idx], Event::Tombstone) {
            unreachable!("abandon on completed marker");
        }
    }
}

pub struct CompletedMarker {
    pub pos: usize,
}

impl CompletedMarker {
    pub fn new(pos: usize) -> Self {
        Self { pos }
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
}
