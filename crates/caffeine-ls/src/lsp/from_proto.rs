use camino::Utf8PathBuf;
use ide::LineCol;
use ide_db::line_index::WideLineCol;
use lsp_types::Position;
use rowan::{TextRange, TextSize};
use std::path::PathBuf;
use vfs::AbsPathBuf;

use crate::line_index::{LineIndex, PositionEncoding};

pub(crate) fn abs_path(url: &lsp_types::Uri) -> anyhow::Result<AbsPathBuf> {
    let path = url
        .to_file_path()
        .map_err(|()| anyhow::format_err!("url is not a file"))?;
    let path = normalize_windows_path(path);
    Ok(AbsPathBuf::try_from(Utf8PathBuf::from_path_buf(path).unwrap()).unwrap())
}

pub(crate) fn vfs_path(url: &lsp_types::Uri) -> anyhow::Result<vfs::VfsPath> {
    abs_path(url).map(vfs::VfsPath::from)
}

/// On Windows, `Url::to_file_path` keeps the drive letter in the case the
/// client used and can yield a `\\?\` verbatim prefix, so the same file can
/// compare differently across requests. Normalize to an upper-cased drive and
/// drop the verbatim prefix; other hosts pass the path through untouched.
pub(crate) fn normalize_windows_path(path: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return path;
    }
    use std::path::{Component, Prefix};
    let mut comps = path.components();
    match comps.next() {
        Some(Component::Prefix(prefix)) => {
            let drive = match prefix.kind() {
                Prefix::Disk(d) => d,
                Prefix::VerbatimDisk(d) => d,
                _ => return path,
            };
            let mut path = PathBuf::new();
            path.push(format!("{}:", drive.to_ascii_uppercase() as char));
            path.extend(comps);
            path
        }
        _ => path,
    }
}

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

pub fn position_to_offset(text: &str, position: Position) -> Option<TextSize> {
    let mut lines = text.split('\n');
    let mut bytes_offset = 0;

    for _ in 0..position.line {
        let line_text = lines.next()?;
        bytes_offset += line_text.len() + 1; // \n
    }

    let target_line = lines.next()?;
    let mut current_utf16_idx = 0;
    let mut utf8_bytes_inside_line = 0;

    for c in target_line.chars() {
        if current_utf16_idx >= position.character {
            break;
        }
        current_utf16_idx += c.len_utf16() as u32;
        utf8_bytes_inside_line += c.len_utf8();
    }

    Some(TextSize::from(
        (bytes_offset + utf8_bytes_inside_line) as u32,
    ))
}

pub(crate) fn offset(
    line_index: &LineIndex,
    position: lsp_types::Position,
) -> anyhow::Result<TextSize> {
    let line_col = match line_index.encoding {
        PositionEncoding::Utf8 => LineCol {
            line: position.line,
            col: position.character,
        },
        PositionEncoding::Wide(enc) => {
            let line_col = WideLineCol {
                line: position.line,
                col: position.character,
            };
            line_index
                .index
                .to_utf8(enc, line_col)
                .ok_or_else(|| anyhow::anyhow!("Invalid wide col offset"))?
        }
    };
    let line_range = line_index.index.line(line_col.line).ok_or_else(|| {
        anyhow::anyhow!(
            "Invalid offset {line_col:?} (line index length: {:?})",
            line_index.index.len()
        )
    })?;
    let col = TextSize::from(line_col.col);
    let clamped_len = col.min(line_range.len());
    // FIXME: The cause for this is likely our request retrying. Commented out as this log is just too chatty and very easy to trigger.
    // if clamped_len < col {
    //     tracing::error!(
    //         "Position {line_col:?} column exceeds line length {}, clamping it",
    //         u32::from(line_range.len()),
    //     );
    // }
    Ok(line_range.start() + clamped_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_uri_passes_through() {
        if cfg!(windows) {
            return;
        }
        let url = lsp_types::Uri::parse("file:///home/me/App.java").unwrap();
        assert_eq!(abs_path(&url).unwrap().as_str(), "/home/me/App.java");
    }

    #[test]
    fn uri_round_trips_through_an_upper_cased_drive() {
        if cfg!(windows) {
            // The client may percent-encode the drive colon (`d%3A`) and use
            // any letter case; the decoded path must be normalized and the
            // rebuilt URI must compare equal to a freshly-parsed one.
            let url = lsp_types::Uri::parse("file:///d%3A/Desktop/A/App.java").unwrap();
            let abs = abs_path(&url).unwrap();
            assert_eq!(abs.as_str(), "D:/Desktop/A/App.java");

            let rebuilt = crate::lsp::to_proto::url(abs.as_path());
            assert_eq!(
                rebuilt,
                lsp_types::Uri::parse("file:///D:/Desktop/A/App.java").unwrap()
            );
        }
    }

    #[test]
    fn windows_verbatim_prefix_is_stripped() {
        if cfg!(windows) {
            assert_eq!(
                normalize_windows_path(std::path::PathBuf::from(r"\\?\c:\foo\bar")),
                std::path::PathBuf::from(r"C:\foo\bar")
            );
            assert_eq!(
                normalize_windows_path(std::path::PathBuf::from(r"c:\foo\bar")),
                std::path::PathBuf::from(r"C:\foo\bar")
            );
        }
    }
}

pub(crate) fn text_range(
    line_index: &LineIndex,
    range: lsp_types::Range,
) -> anyhow::Result<TextRange> {
    let start = offset(line_index, range.start)?;
    let end = offset(line_index, range.end)?;
    match end < start {
        true => Err(anyhow::anyhow!("Invalid Range")),
        false => Ok(TextRange::new(start, end)),
    }
}
