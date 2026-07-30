use crate::{lexical::LexicalIndex, symbol::GlobalSymbolIndex};

pub mod lexical;
pub mod symbol;

pub struct IndexDatabase {
    pub lexical: LexicalIndex,
    pub symbols: GlobalSymbolIndex,
}
