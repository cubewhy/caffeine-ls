//! The doc-comment → Markdown renderer.
//!
//! The rules follow the *JavaDoc Documentation Comment Specification for the
//! Standard Doclet* (JDK 25) — the normative source for doc-comment structure:
//! <https://docs.oracle.com/en/java/javase/25/docs/specs/javadoc/doc-comment-spec.html>.
//! The comment delimiters themselves are lexical
//! ([JLS §3.7](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.7)),
//! and the token text handed to [`render`] is exactly what the lexer produced
//! (`/** … */` for a traditional comment, one `///` line per token for a
//! Markdown one).
//!
//! Two boundaries are deliberate, and neither is a bug:
//!
//! * `{@link}`, `{@linkplain}` and `{@value}` *references* are not resolved to
//!   URIs — they render as text, because the hover payload carries no
//!   navigation target.
//! * `{@inheritDoc}` is recognised and dropped: inheritance from overridden
//!   members is not resolved.
//!
//! Anything a standard doclet would resolve and this renderer cannot is kept
//! as written rather than dropped, so a hover never silently loses what the
//! author wrote.

use std::borrow::Cow;

/// How the text of a doc comment is interpreted: a traditional comment
/// carries HTML, a Markdown one carries CommonMark.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Traditional,
    Markdown,
}

/// One block tag: the identifier after `@`, and its content — the rest of the
/// tag's first line and every following line up to the next block tag.
struct BlockTag {
    name: String,
    content: String,
}

/// A parsed doc comment: the main description and the block tags, in source
/// order.
struct DocComment {
    description: String,
    tags: Vec<BlockTag>,
}

/// One inline tag: the identifier after `@{`, and its content up to the
/// matching brace.
struct InlineTag<'a> {
    name: &'a str,
    content: &'a str,
}

/// Renders the doc comment `raw` as Markdown. `owner` is the simple name of
/// the declaration the comment documents, which only a bare `{@value}`
/// displays.
pub(super) fn render(raw: &str, owner: Option<&str>) -> String {
    let (content, mode) = strip(raw);
    let comment = split(&content);
    let mut sections: Vec<String> = Vec::new();

    // The standard doclet "moves deprecated text ahead of the main
    // description, placing it in italics and preceding it with a bold
    // warning".
    let deprecated: Vec<String> = comment
        .tags
        .iter()
        .filter(|tag| tag.name == "deprecated")
        .map(|tag| format!("> **Deprecated.** {}", convert(&tag.content, mode, owner)))
        .collect();
    if !deprecated.is_empty() {
        sections.push(deprecated.join("\n>\n"));
    }

    // The main description: everything before the first block tag. The
    // specification ignores its leading and trailing whitespace.
    let description = convert(&comment.description, mode, owner);
    if !description.trim().is_empty() {
        sections.push(description.trim().to_owned());
    }

    let mut params: Vec<String> = Vec::new();
    for tag in comment.tags.iter().filter(|tag| tag.name == "param") {
        let (name, description) = split_first(&tag.content);
        if name.is_empty() {
            continue;
        }
        // A type parameter is written `<T>` in the tag and named `T`
        // elsewhere.
        let name = trim_brackets(name);
        let description = convert(description, mode, owner);
        params.push(list_item(&format!("`{name}`"), description.trim()));
    }
    if !params.is_empty() {
        sections.push(format!("**Parameters:**\n{}", params.join("\n")));
    }

    let returns: Vec<String> = comment
        .tags
        .iter()
        .filter(|tag| tag.name == "return")
        .map(|tag| convert(&tag.content, mode, owner).trim().to_owned())
        .filter(|text| !text.is_empty())
        .collect();
    if !returns.is_empty() {
        sections.push(format!("**Returns:** {}", returns.join("\n\n")));
    }

    // `@exception` is the historical spelling of `@throws`.
    let throws: Vec<String> = comment
        .tags
        .iter()
        .filter(|tag| tag.name == "throws" || tag.name == "exception")
        .filter_map(|tag| {
            let (name, description) = split_first(&tag.content);
            if name.is_empty() {
                return None;
            }
            let description = convert(description, mode, owner);
            Some(list_item(&format!("`{name}`"), description.trim()))
        })
        .collect();
    if !throws.is_empty() {
        sections.push(format!("**Throws:**\n{}", throws.join("\n")));
    }

    // `@see` content is a reference or a phrase; `see` tags are all grouped
    // under one heading.
    let see: Vec<String> = comment
        .tags
        .iter()
        .filter(|tag| tag.name == "see")
        .map(|tag| convert(&tag.content, mode, owner))
        .map(|text| {
            format!(
                "- {}",
                text.split_whitespace().collect::<Vec<_>>().join(" ")
            )
        })
        .collect();
    if !see.is_empty() {
        sections.push(format!("**See Also:**\n{}", see.join("\n")));
    }

    let since: Vec<String> = comment
        .tags
        .iter()
        .filter(|tag| tag.name == "since")
        .map(|tag| convert(&tag.content, mode, owner).trim().to_owned())
        .filter(|text| !text.is_empty())
        .collect();
    if !since.is_empty() {
        sections.push(format!("**Since:** {}", since.join("\n\n")));
    }

    // Every other block tag: the ones the standard doclet gates behind an
    // option are tooling-only and dropped, and anything else (a user-defined
    // tag, `@apiNote`) keeps its content under its own name.
    const DROPPED: &[&str] = &[
        "author",
        "version",
        "hidden",
        "serial",
        "serialData",
        "serialField",
        "uses",
        "provides",
    ];
    let others: Vec<String> = comment
        .tags
        .iter()
        .filter(|tag| {
            !matches!(
                tag.name.as_str(),
                "deprecated" | "param" | "return" | "throws" | "exception" | "see" | "since"
            ) && !DROPPED.contains(&tag.name.as_str())
        })
        .map(|tag| {
            let content = convert(&tag.content, mode, owner);
            let content = content.trim();
            match content.is_empty() {
                true => format!("**@{}**", tag.name),
                false => format!("**@{}** {content}", tag.name),
            }
        })
        .collect();
    if !others.is_empty() {
        sections.push(others.join("\n\n"));
    }

    // One blank line between sections.
    sections.join("\n\n").trim().to_owned()
}

/// The `@param <name>` description of the doc comment `raw`, rendered like the
/// rest of a hover's documentation; `None` when the comment documents no such
/// parameter, or documents it with no text.
pub(super) fn param(raw: &str, name: &str) -> Option<String> {
    let (content, mode) = strip(raw);
    let comment = split(&content);
    let tag = comment
        .tags
        .iter()
        .find(|tag| tag.name == "param" && trim_brackets(split_first(&tag.content).0) == name)?;
    let description = convert(split_first(&tag.content).1, mode, None);
    let description = description.trim();
    (!description.is_empty()).then(|| description.to_owned())
}

/// The text of the comment with its delimiters removed — and, for a
/// traditional comment, with the leading whitespace and asterisks of every
/// line removed — plus the mode its body is written in.
fn strip(raw: &str) -> (String, Mode) {
    if raw.starts_with("///") {
        (strip_markdown(raw), Mode::Markdown)
    } else {
        (strip_traditional(raw), Mode::Traditional)
    }
}

/// Strips the delimiters of a traditional comment and the leading whitespace
/// and asterisks of each of its lines.
///
/// "If any line in such a comment begins with asterisks after any leading
/// whitespace, the leading whitespace and asterisks are removed. Any
/// whitespace appearing after the asterisks is not removed." The unterminated
/// comment the lexer reports (`UnterminatedComment`) keeps the rest of the
/// text: `*/` is optional here.
fn strip_traditional(raw: &str) -> String {
    let body = raw.strip_prefix("/**").unwrap_or(raw);
    // Trailing whitespace is ignored by the specification, so the delimiter is
    // recognised through it.
    let body = body
        .trim_end()
        .strip_suffix("*/")
        .unwrap_or(body.trim_end());
    body.split('\n')
        .map(strip_leading_asterisks)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line of a traditional comment without its leading whitespace and
/// asterisks; a line that begins with neither is unchanged.
fn strip_leading_asterisks(line: &str) -> &str {
    let trimmed = line.trim_start_matches(leading_whitespace);
    let stars = trimmed.trim_start_matches('*');
    match stars.len() == trimmed.len() {
        true => line,
        false => stars,
    }
}

/// The text of a Markdown comment: "any leading whitespace and the three
/// initial `/` characters are removed from each line", and the lines are then
/// shifted left by the least common leading whitespace ("similar to
/// `String.stripIndent`, except that there is no need for any special
/// treatment for a trailing blank line"). Trailing whitespace is kept: it may
/// be a CommonMark hard line break.
fn strip_markdown(raw: &str) -> String {
    let lines: Vec<&str> = raw
        .split('\n')
        .map(|line| {
            let trimmed = line.trim_start_matches(leading_whitespace);
            trimmed.strip_prefix("///").unwrap_or(trimmed)
        })
        .collect();
    let common = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| indent(line))
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| {
            let cut = common.min(indent(line));
            line.split_at(cut).1
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The number of leading horizontal whitespace characters of `line`.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(leading_whitespace).len()
}

/// The whitespace a doc comment's leading-asterisk and Markdown rules strip:
/// horizontal whitespace, as
/// [JLS §3.6](https://docs.oracle.com/javase/specs/jls/se26/html/jls-3.html#jls-3.6)
/// defines it.
const leading_whitespace: &[char] = &[' ', '\t', '\u{c}'];

/// Splits the stripped comment into its main description and its block tags.
///
/// A block tag "must appear at the beginning of a line, ignoring leading
/// asterisks, whitespace characters, and the initial comment delimiter", and
/// "[l]ines beginning with `@` that are enclosed within an inline tag are not
/// considered as beginning a block tag" — hence the brace depth.
fn split(content: &str) -> DocComment {
    let mut description: Vec<&str> = Vec::new();
    let mut tags: Vec<BlockTag> = Vec::new();
    let mut depth = 0usize;
    for line in content.split('\n') {
        let opening = if depth == 0 { block_tag(line) } else { None };
        match opening {
            Some((name, rest)) => tags.push(BlockTag {
                name: name.to_owned(),
                content: rest.trim_start().to_owned(),
            }),
            // A continuation line belongs to the tag it follows, or to the
            // description when no tag has opened yet.
            None => match tags.last_mut() {
                Some(tag) => {
                    tag.content.push('\n');
                    tag.content.push_str(line);
                }
                None => description.push(line),
            },
        }
        depth = brace_depth(line, depth);
    }
    DocComment {
        description: description.join("\n").trim().to_owned(),
        tags,
    }
}

/// The block tag that opens on `line` (`@param x` → `param`), with the rest of
/// the line; `None` when the line opens none. A tag opens where the first
/// non-whitespace character is `@` followed by an identifier, so an escaped
/// `@@`, a `@/` and an `@` on its own stay text.
fn block_tag(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim_start().strip_prefix('@')?;
    let end = rest.find(|c: char| !is_identifier(c)).unwrap_or(rest.len());
    (!rest[..end].is_empty()).then(|| (&rest[..end], &rest[end..]))
}

/// Whether `c` may be part of an identifier (a tag name).
fn is_identifier(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The brace depth after `line`, given the depth before it: an inline tag
/// opens at `{@` and its content has balanced braces, so the tag's own closing
/// brace is the one that brings the depth back to zero.
fn brace_depth(line: &str, depth: usize) -> usize {
    let mut depth = depth;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if depth > 0 || chars.peek() == Some(&'@') => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth
}

/// Renders the descriptive text `text` as Markdown: inline tags are converted
/// in both modes, while the surrounding content is HTML in a traditional
/// comment (converted) and CommonMark in a Markdown one (passed through,
/// raw HTML included).
fn convert(text: &str, mode: Mode, owner: Option<&str>) -> String {
    let unescaped = unescape(text, mode);
    let text: &str = &unescaped;
    let mut out = String::with_capacity(text.len() + 16);
    let mut rest = text;
    // The `href` of the `<a>` an `</a>` closes: the attributes are written on
    // the opening tag, so the closing one cannot render the destination on its
    // own.
    let mut href: Option<String> = None;
    // A run of whitespace around a line break, held back until something
    // follows it on the line (see [`push_literal`]).
    let mut pending = false;
    loop {
        let inline_at = rest.find("{@");
        let html_at = match mode {
            Mode::Traditional => rest.find('<'),
            Mode::Markdown => None,
        };
        let idx = match (inline_at, html_at) {
            (Some(inline), Some(html)) => inline.min(html),
            (Some(inline), None) => inline,
            (None, Some(html)) => html,
            (None, None) => {
                push_literal(rest, mode, &mut out, &mut pending);
                return out;
            }
        };
        let before = &rest[..idx];
        let tail = &rest[idx..];
        // An inline tag outranks the `<` of an HTML tag only by position: the
        // two cannot start on the same byte, and `{@code <T>}` keeps its
        // angle brackets because the whole tag is consumed at once.
        if tail.starts_with("{@") {
            push_literal(before, mode, &mut out, &mut pending);
            match parse_inline(tail) {
                Some((tag, consumed)) => {
                    let rendered = inline(&tag, owner);
                    flush_pending(&mut out, &mut pending, &rendered);
                    out.push_str(&rendered);
                    rest = &tail[consumed..];
                }
                // An unterminated tag is not a tag: keep it as text.
                None => {
                    out.push_str("{@");
                    rest = &tail[2..];
                }
            }
        } else {
            push_literal(before, mode, &mut out, &mut pending);
            match element(tail, &mut href) {
                Some((rendered, layout, consumed)) => {
                    match layout {
                        Layout::Inline => {
                            flush_pending(&mut out, &mut pending, &rendered);
                            out.push_str(&rendered);
                        }
                        // A list item opens its own line; the whitespace before
                        // it is the list's, not the item's.
                        Layout::Item => {
                            pending = false;
                            out.push_str(&rendered);
                        }
                        Layout::Block => {
                            pending = false;
                            push_block(&mut out, &rendered);
                        }
                    }
                    rest = &tail[consumed..];
                }
                None => {
                    out.push('<');
                    rest = &tail[1..];
                }
            }
        }
    }
}

/// The comment's escape sequences applied: "`@@`, to represent `@`" and
/// "`@*`, to represent `*`, where it would otherwise be discarded at the
/// beginning of a line". They are context-sensitive and recognised only where
/// the character would otherwise be interpreted, which for both is the
/// beginning of a line — ignoring the leading whitespace a traditional
/// comment's asterisk stripping leaves behind. A Markdown comment has no
/// escape sequences: its text is CommonMark, which escapes with a backslash.
fn unescape(text: &str, mode: Mode) -> Cow<'_, str> {
    if mode == Mode::Markdown || (!text.contains("@@") && !text.contains("@*")) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let leading = indent(line);
        let rest = &line[leading..];
        match rest.starts_with('@') && matches!(rest.as_bytes().get(1), Some(b'@' | b'*')) {
            true => {
                out.push_str(&line[..leading]);
                out.push_str(&rest[1..]);
            }
            false => out.push_str(line),
        }
    }
    Cow::Owned(out)
}

/// Writes the literal text `text` of a `mode` comment to `out`, taking the
/// whitespace HTML collapses out of a traditional comment.
///
/// A run that is nothing but whitespace around a line break renders as a
/// single space, and only where two things actually share the line — so the
/// source's formatting of a block-level element does not litter the Markdown
/// with blank lines. [`flush_pending`] emits it when the next thing on the
/// line arrives; a block-level element discards it, its own line breaks being
/// the boundary ([`push_block`]).
fn push_literal(text: &str, mode: Mode, out: &mut String, pending: &mut bool) {
    match mode {
        // A Markdown comment is verbatim: its whitespace may be significant
        // (an indented code block, a hard line break), and its entities are
        // CommonMark's own business.
        Mode::Markdown => out.push_str(text),
        Mode::Traditional if text.trim().is_empty() && text.contains('\n') => *pending = true,
        Mode::Traditional => {
            flush_pending(out, pending, text);
            push_entities(text, out);
        }
    }
}

/// Emits the space a held-back run of whitespace renders as, when `next`
/// follows it on the same line and does not bring whitespace of its own.
fn flush_pending(out: &mut String, pending: &mut bool, next: &str) {
    if !*pending || next.is_empty() {
        return;
    }
    *pending = false;
    if !out.is_empty() && !out.ends_with('\n') && !next.starts_with(char::is_whitespace) {
        out.push(' ');
    }
}

/// The HTML at the start of `text` (which begins with `<`) as Markdown, the
/// number of bytes it spans, and how its Markdown sits in the text around it.
/// `None` when it is not an element at all, so the caller keeps the `<` as
/// text.
///
/// An element whose name the renderer does not know contributes its content
/// alone: its tags are dropped (a documented fallback — `table`s and `div`s
/// degrade to their text), and it counts as inline, so the whitespace before
/// it is not lost.
fn element(text: &str, href: &mut Option<String>) -> Option<(String, Layout, usize)> {
    // A comment is dropped whole, `-->` included.
    if let Some(rest) = text.strip_prefix("<!--") {
        let consumed = rest.find("-->").map_or(text.len(), |end| end + 3 + 4);
        return Some((String::new(), Layout::Inline, consumed));
    }
    let end = text.find('>')?;
    let inner = &text[1..end];
    let (closing, inner) = match inner.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, inner),
    };
    let name_end = inner
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(inner.len());
    if name_end == 0 {
        return None;
    }
    let name = &inner[..name_end];
    let attrs = &inner[name_end..];
    let consumed = end + 1;

    // `pre` keeps its content verbatim, in a fenced block; `sub`/`sup` are
    // kept as inline HTML only when their content carries no markup.
    if name.eq_ignore_ascii_case("pre") {
        if closing || attrs.trim_end().ends_with('/') {
            return Some((String::new(), Layout::Block, consumed));
        }
        let (content, length) = element_content(&text[consumed..], "pre");
        return Some((
            format!("\n```\n{content}\n```\n"),
            Layout::Block,
            consumed + length,
        ));
    }
    if name.eq_ignore_ascii_case("sub") || name.eq_ignore_ascii_case("sup") {
        if closing {
            return Some((String::new(), Layout::Inline, consumed));
        }
        let (content, length) = element_content(&text[consumed..], name);
        let rendered = match content.contains('<') {
            true => String::new(),
            false => {
                let mut out = format!("<{name}>");
                push_entities(content, &mut out);
                out.push_str(&format!("</{name}>"));
                out
            }
        };
        return Some((rendered, Layout::Inline, consumed + length));
    }

    let rendered = html_tag(name, closing, attrs, href);
    let layout = match rendered.is_empty() {
        true => Layout::Inline,
        false => layout(name),
    };
    Some((rendered, layout, consumed))
}

/// How an element's Markdown sits in the text around it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// Inline: it may share a line with what precedes it, so the whitespace
    /// between the two still separates them.
    Inline,
    /// A block element: it opens on a line of its own, one blank line after
    /// what precedes it.
    Block,
    /// An item of a list — `li`, `dt`, `dd`: it opens its own line, while the
    /// spacing of the list around it belongs to the list's own elements.
    Item,
}

/// How the element named `name` sits in the text around it: the elements whose
/// Markdown may share a line are inline, the items of a list are items, and
/// everything else — paragraphs, headings, quotations, lists, rules — is
/// block-level, where HTML renders the surrounding whitespace as a boundary
/// rather than as text.
fn layout(name: &str) -> Layout {
    match name.to_ascii_lowercase().as_str() {
        "code" | "tt" | "b" | "strong" | "i" | "em" | "a" | "sub" | "sup" | "br" => Layout::Inline,
        "li" | "dt" | "dd" => Layout::Item,
        _ => Layout::Block,
    }
}

/// Appends the Markdown of a block-level element: it opens on a line of its
/// own, one blank line after what precedes it.
///
/// HTML renders the whitespace around a block-level element as a boundary
/// rather than as text, so the line the source left open is closed here and
/// the element opens with exactly one blank line — the comment's own
/// indentation leaves neither a line of trailing spaces nor a run of blank
/// lines in the Markdown, whatever the source did.
fn push_block(out: &mut String, replacement: &str) {
    while out.ends_with([' ', '\t']) {
        out.pop();
    }
    let body = replacement.trim_start_matches('\n');
    let trailing = out.len() - out.trim_end_matches('\n').len();
    out.truncate(out.len() - trailing);
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(body);
}

/// The content of the element named `name` that starts at `text` (which begins
/// with the text just after its opening tag), and the number of bytes through
/// its closing tag. An element that is never closed swallows the rest of the
/// text, as an HTML parser would.
fn element_content<'a>(text: &'a str, name: &str) -> (&'a str, usize) {
    let mut from = 0;
    while let Some(relative) = text[from..].find("</") {
        let idx = from + relative;
        if let Some(consumed) = closing_tag(&text[idx..], name) {
            return (&text[..idx], idx + consumed);
        }
        from = idx + 2;
    }
    (text, text.len())
}

/// The length of the closing tag of `name` at the start of `text` (`</pre>`),
/// if that is what `text` begins with. Tag names are case-insensitive in HTML.
fn closing_tag(text: &str, name: &str) -> Option<usize> {
    let rest = text.strip_prefix("</")?;
    if !rest.get(..name.len())?.eq_ignore_ascii_case(name) {
        return None;
    }
    let after = &rest[name.len()..];
    let end = after.find('>')?;
    after[..end]
        .trim()
        .is_empty()
        .then_some(2 + name.len() + end + 1)
}

/// The Markdown for one HTML element, by its (case-insensitive) name. An
/// unknown element is dropped, its content rendered inline by the caller.
fn html_tag(name: &str, closing: bool, attrs: &str, href: &mut Option<String>) -> String {
    match name.to_ascii_lowercase().as_str() {
        "p" => "\n\n".to_owned(),
        "br" => match closing {
            true => String::new(),
            // A CommonMark hard line break.
            false => "  \n".to_owned(),
        },
        "ul" | "ol" | "dl" => match closing {
            true => "\n".to_owned(),
            false => "\n\n".to_owned(),
        },
        "li" => match closing {
            true => "\n".to_owned(),
            false => "- ".to_owned(),
        },
        "dt" => match closing {
            true => "**".to_owned(),
            false => "- **".to_owned(),
        },
        "dd" => match closing {
            true => "\n".to_owned(),
            false => " ".to_owned(),
        },
        "blockquote" => match closing {
            true => "\n".to_owned(),
            false => "\n\n> ".to_owned(),
        },
        "hr" => "\n\n---\n\n".to_owned(),
        "code" | "tt" => "`".to_owned(),
        "b" | "strong" => "**".to_owned(),
        "i" | "em" => "*".to_owned(),
        "a" if closing => match href.take() {
            Some(href) => {
                let mut out = String::from("](");
                push_entities(&href, &mut out);
                out.push(')');
                out
            }
            None => "]".to_owned(),
        },
        "a" => {
            *href = attr_value(attrs, "href");
            "[".to_owned()
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => match closing {
            true => "\n\n".to_owned(),
            false => format!("\n\n{} ", "#".repeat(name.len() - 1)),
        },
        _ => String::new(),
    }
}

/// The value of the attribute `name` in a tag's attribute text, unquoted; a
/// valueless attribute is `None`, as is one that never ends.
fn attr_value(attrs: &str, name: &str) -> Option<String> {
    let mut rest = attrs;
    loop {
        let idx = rest.find(|c: char| c.is_ascii_alphabetic())?;
        rest = &rest[idx..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .unwrap_or(rest.len());
        let (attribute, tail) = rest.split_at(end);
        let tail = tail.trim_start();
        let Some(tail) = tail.strip_prefix('=') else {
            rest = tail;
            continue;
        };
        let tail = tail.trim_start();
        let (value, tail) = match tail.chars().next()? {
            quote @ ('"' | '\'') => {
                let quoted = &tail[quote.len_utf8()..];
                let end = quoted.find(quote)?;
                (&quoted[..end], &quoted[end + 1..])
            }
            _ => {
                let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
                tail.split_at(end)
            }
        };
        if attribute.eq_ignore_ascii_case(name) {
            return Some(value.to_owned());
        }
        rest = tail;
    }
}

/// The inline tag at the start of `text` (which begins with `{@`) and the
/// number of bytes it spans; `None` when it is not terminated.
///
/// "When such text explicitly contains braces, the braces must be balanced ...
/// so that the closing brace of the inline tag can be determined. No other
/// lexical analysis of the text is performed."
fn parse_inline(text: &str) -> Option<(InlineTag<'_>, usize)> {
    let body = &text[2..];
    let name_end = body.find(|c: char| !is_identifier(c)).unwrap_or(body.len());
    if name_end == 0 {
        return None;
    }
    let name = &body[..name_end];
    let mut depth = 1usize;
    for (idx, c) in body[name_end..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((
                        InlineTag {
                            name,
                            content: &body[name_end..name_end + idx],
                        },
                        2 + name_end + idx + 1,
                    ));
                }
            }
            _ => {}
        }
    }
    None
}

/// The Markdown for one inline tag. `owner` is the enclosing declaration's
/// simple name, which only a bare `{@value}` needs.
fn inline(tag: &InlineTag<'_>, owner: Option<&str>) -> String {
    let content = tag.content.trim();
    match tag.name {
        // `text` in code font, not interpreted as HTML markup or nested tags.
        "code" => match content.contains('`') {
            true => format!("<code>{content}</code>"),
            false => format!("`{content}`"),
        },
        // `text` with the markup left alone: escaped, so CommonMark shows it
        // as written.
        "literal" => {
            let mut out = String::with_capacity(content.len());
            push_entities(content, &mut out);
            escape_markdown(&out)
        }
        // A link renders as its label; with no label the text is the
        // reference itself, in the monospace font the doclet uses.
        "link" | "linkplain" => {
            let (reference, label) = split_reference(content);
            match label.is_empty() {
                true => format!("`{reference}`"),
                false => {
                    let mut out = String::new();
                    push_entities(label, &mut out);
                    out
                }
            }
        }
        // The definition of a constant is not in the comment: the tag renders
        // as a reference, which is the constant's simple name.
        "value" => {
            let reference = split_reference(content).0;
            let name = match reference.is_empty() {
                true => owner.unwrap_or_default(),
                false => reference.rsplit(['.', '#']).next().unwrap_or(reference),
            };
            match name.is_empty() {
                true => "{@value}".to_owned(),
                false => format!("`{name}`"),
            }
        }
        "summary" => {
            let mut out = String::new();
            push_entities(content, &mut out);
            out
        }
        "systemProperty" => format!("`{content}`"),
        // `docRoot` and `index` are tooling-only; `snippet` and `inheritDoc`
        // would need machines this renderer does not have (see the module
        // docs).
        "docRoot" | "index" | "snippet" | "inheritDoc" => String::new(),
        // An unknown tag keeps its text, so nothing is silently lost.
        _ => {
            let mut out = String::new();
            push_entities(&format!("{{@{}{}}}", tag.name, tag.content), &mut out);
            escape_markdown(&out)
        }
    }
}

/// Decodes the HTML entities a doc comment may carry — the named ones the
/// specification names plus any numeric one; an entity the renderer does not
/// know is left as written.
fn push_entities(text: &str, out: &mut String) {
    if !text.contains('&') {
        out.push_str(text);
        return;
    }
    let mut rest = text;
    while let Some(idx) = rest.find('&') {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx..];
        let Some(end) = tail.find(';') else {
            out.push_str(tail);
            return;
        };
        match entity(&tail[1..end]) {
            Some(value) => {
                out.push_str(&value);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
}

/// The character of one HTML entity, or `None` when it is not one the renderer
/// knows.
fn entity(name: &str) -> Option<String> {
    let character = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "copy" => '\u{a9}',
        "lbrace" => '{',
        "rbrace" => '}',
        _ => {
            let digits = name.strip_prefix('#')?;
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse().ok()?,
            };
            return char::from_u32(code).map(String::from);
        }
    };
    Some(character.to_string())
}

/// `text` with CommonMark's escapable punctuation backslash-escaped, so an
/// editor shows it as written rather than interpreting it as markup.
fn escape_markdown(text: &str) -> String {
    const PUNCTUATION: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if PUNCTUATION.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// One entry of a rendered list section: `- head`, with the description after
/// an em dash when the tag wrote one.
fn list_item(head: &str, description: &str) -> String {
    match description.is_empty() {
        true => format!("- {head}"),
        false => format!("- {head} — {description}"),
    }
}

/// The first whitespace-delimited word of `content` and the rest of it, both
/// without their surrounding whitespace.
fn split_first(content: &str) -> (&str, &str) {
    let content = content.trim_start();
    match content.find(char::is_whitespace) {
        Some(end) => (&content[..end], content[end..].trim_start()),
        None => (content, ""),
    }
}

/// The reference of a `{@link}`/`{@value}` tag and the label that follows it,
/// both without their surrounding whitespace.
///
/// A reference may carry a parameter list, and "whitespace characters may
/// appear between tokens in the parameter list" while "whitespace characters
/// may not appear elsewhere in the reference" — so the first whitespace
/// *outside* parentheses ends the reference, and everything after it is the
/// label.
fn split_reference(content: &str) -> (&str, &str) {
    let content = content.trim_start();
    let mut depth = 0usize;
    for (idx, c) in content.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if c.is_whitespace() && depth == 0 => {
                return (&content[..idx], content[idx..].trim_start());
            }
            _ => {}
        }
    }
    (content, "")
}

/// A name as a tag writes a type parameter (`<T>`) without the angle brackets.
fn trim_brackets(name: &str) -> &str {
    name.strip_prefix('<')
        .and_then(|name| name.strip_suffix('>'))
        .unwrap_or(name)
}
