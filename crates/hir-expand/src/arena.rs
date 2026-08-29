//! A tiny `Vec`-backed arena with stable ids, used for the items of a
//! per-file `ItemTree` and its `BodyTree`. Insertion order is significant:
//! items are allocated in CST order so iteration is deterministic.

use std::ops::Index;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArenaId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arena<T> {
    data: Vec<T>,
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self { data: Vec::new() }
    }
}

impl<T> Arena<T> {
    pub fn alloc(&mut self, value: T) -> ArenaId {
        let id = ArenaId(self.data.len() as u32);
        self.data.push(value);
        id
    }

    pub fn get(&self, id: ArenaId) -> &T {
        &self.data[id.0 as usize]
    }

    pub fn get_mut(&mut self, id: ArenaId) -> &mut T {
        &mut self.data[id.0 as usize]
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (ArenaId, &T)> {
        self.data
            .iter()
            .enumerate()
            .map(|(idx, item)| (ArenaId(idx as u32), item))
    }
}

impl<T> Index<ArenaId> for Arena<T> {
    type Output = T;

    fn index(&self, id: ArenaId) -> &T {
        self.get(id)
    }
}
