#[derive(Debug, Clone, Copy)]
pub struct Checkpoint {
    pub source_pos: usize,
    pub events_len: usize,
    pub errors_len: usize,
}
