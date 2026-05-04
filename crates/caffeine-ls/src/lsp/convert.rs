use lsp_types::{Position, Url};
use rowan::TextSize;
use vfs::FileId;

pub fn offset_to_position(text: &str, offset: TextSize) -> Position {
    let offset = u32::from(offset) as usize;
    let safe_offset = offset.min(text.len());
    let head = &text[..safe_offset];

    let line = head.chars().filter(|&c| c == '\n').count() as u32;

    let last_line_start = head.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let last_line_text = &head[last_line_start..];

    let character = last_line_text.encode_utf16().count() as u32;

    Position { line, character }
}

pub fn file_id_to_url(vfs: &vfs::Vfs, file_id: FileId) -> Option<Url> {
    let vfs_path = vfs.file_path(file_id);

    if let Some(abs_path) = vfs_path.as_path() {
        Url::from_file_path(abs_path).ok()
    } else {
        Url::parse(&vfs_path.to_string()).ok()
    }
}
