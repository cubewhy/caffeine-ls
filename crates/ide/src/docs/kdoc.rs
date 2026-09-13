//! The KDoc → Markdown renderer.
//!
//! The rules follow the Kotlin documentation's *Documenting Kotlin code*
//! (<https://kotlinlang.org/docs/kotlin-doc.html>), which is the normative
//! source for KDoc structure: a comment `/** … */` whose first paragraph is the
//! summary, followed by the description and by *block tags* introduced by `@`.
//! The token text handed to [`render`] is exactly what the Kotlin lexer
//! produced (`kotlin-syntax`'s `SyntaxKind::KDOC`), delimiters included.
//!
//! Two boundaries are deliberate, and neither is a bug:
//!
//! * a `[Name]` reference is not resolved to a navigation target — the hover
//!   payload carries no link — so it renders as a code span;
//! * `@author`, `@since` and `@suppress` are tooling-only and are dropped, so a
//!   comment that carries nothing else renders empty and the hover shows no
//!   documentation section at all.
//!
//! KDoc's text *is* Markdown, so unlike the JavaDoc renderer this one does not
//! translate HTML: what the author wrote is what a client shows.

/// One block tag: the identifier after `@`, and its content — the rest of the
/// tag's first line and every following line up to the next block tag.
struct BlockTag {
    name: String,
    content: String,
}

/// Renders the KDoc comment `raw` as Markdown. `owner` is the simple name of
/// the declaration the comment documents; KDoc has no bare-value tag, so it is
/// accepted for symmetry with the JavaDoc renderer and unused.
pub(super) fn render(raw: &str, owner: Option<&str>) -> String {
    let _ = owner;
    let (description, tags) = split(raw);
    let mut sections: Vec<String> = Vec::new();

    let description = render_inline(&description);
    if !description.trim().is_empty() {
        sections.push(description.trim().to_owned());
    }

    let params = list_section(tags.iter().filter(|tag| tag.name == "param"), "Parameters");
    if let Some(section) = params {
        sections.push(section);
    }

    let properties = list_section(
        tags.iter().filter(|tag| tag.name == "property"),
        "Properties",
    );
    if let Some(section) = properties {
        sections.push(section);
    }

    // `@receiver` and `@constructor` document the extension receiver and the
    // primary constructor of the declaration the comment is on; both name the
    // declaration they describe rather than taking a parameter name.
    for (tag, heading) in [("receiver", "Receiver"), ("constructor", "Constructor")] {
        let content: Vec<String> = tags
            .iter()
            .filter(|block| block.name == tag)
            .map(|block| render_inline(&block.content))
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
            .collect();
        if !content.is_empty() {
            sections.push(format!("**{heading}:** {}", content.join("\n\n")));
        }
    }

    let returns: Vec<String> = tags
        .iter()
        .filter(|tag| tag.name == "return")
        .map(|tag| render_inline(&tag.content))
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .collect();
    if !returns.is_empty() {
        sections.push(format!("**Returns:** {}", returns.join("\n\n")));
    }

    // `@exception` is the historical spelling of `@throws`, as in JavaDoc.
    let throws = list_section(
        tags.iter()
            .filter(|tag| tag.name == "throws" || tag.name == "exception"),
        "Throws",
    );
    if let Some(section) = throws {
        sections.push(section);
    }

    let see: Vec<String> = tags
        .iter()
        .filter(|tag| tag.name == "see" || tag.name == "sample")
        .map(|tag| render_inline(&tag.content))
        .map(|text| {
            format!(
                "- {}",
                text.split_whitespace().collect::<Vec<_>>().join(" ")
            )
        })
        .collect();
    if !see.is_empty() {
        sections.push(format!("**See also:**\n{}", see.join("\n")));
    }

    sections.join("\n\n")
}

/// A `name description` list section (`**Parameters:**` and friends), or `None`
/// when no tag of the kind carries content.
fn list_section<'a>(tags: impl Iterator<Item = &'a BlockTag>, heading: &str) -> Option<String> {
    let items: Vec<String> = tags
        .filter_map(|tag| {
            let (name, description) = split_first(&tag.content);
            if name.is_empty() {
                return None;
            }
            Some(format!(
                "- `{}` {}",
                name,
                render_inline(description).trim()
            ))
        })
        .collect();
    (!items.is_empty()).then(|| format!("**{heading}:**\n{}", items.join("\n")))
}

/// Splits a KDoc comment into its description and its block tags, stripping the
/// `/** */` delimiters and the leading `*` of every line.
fn split(raw: &str) -> (String, Vec<BlockTag>) {
    let body = raw
        .strip_prefix("/**")
        .unwrap_or(raw)
        .strip_suffix("*/")
        .unwrap_or_else(|| raw.strip_prefix("/**").unwrap_or(raw));

    let mut lines = Vec::new();
    for (index, line) in body.lines().enumerate() {
        let line = line.trim_start();
        // The `*` of the comment's own column, and the single space after it.
        let line = if let Some(rest) = line.strip_prefix('*') {
            rest.strip_prefix(' ').unwrap_or(rest)
        } else if index == 0 {
            // The text after `/**` on the opening line carries no column.
            line
        } else {
            line
        };
        lines.push(line.trim_end().to_owned());
    }

    let mut description = String::new();
    let mut tags: Vec<BlockTag> = Vec::new();
    for line in lines {
        let trimmed = line.trim_start();
        if let Some(tag) = trimmed.strip_prefix('@') {
            let (name, content) = split_first(tag);
            tags.push(BlockTag {
                name: name.to_ascii_lowercase(),
                content: content.to_owned(),
            });
        } else if let Some(last) = tags.last_mut() {
            if !line.trim().is_empty() {
                last.content.push(' ');
                last.content.push_str(line.trim());
            }
        } else {
            description.push_str(&line);
            description.push('\n');
        }
    }
    (description, tags)
}

/// Renders KDoc inline syntax: a `[Name]` or `[Name|label]` declaration
/// reference becomes a code span.
///
/// A Markdown link (`[text](url)`, `[text][ref]`) is left as written: KDoc's
/// own reference syntax has no `(` or `][` after the closing bracket.
fn render_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let after = &rest[start + 1..];
        let Some(end) = after.find(']') else {
            break;
        };
        let inner = &after[..end];
        let following = after[end + 1..].chars().next();
        if following.is_some_and(|c| c == '(' || c == '[') || !is_reference(inner) {
            out.push_str(&rest[..start + 1]);
            rest = after;
            continue;
        }
        let label = inner
            .split_once('|')
            .map(|(_, label)| label)
            .unwrap_or(inner);
        out.push_str(&rest[..start]);
        out.push('`');
        out.push_str(label.trim());
        out.push('`');
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Whether bracketed KDoc content is a declaration reference: a (possibly
/// qualified, possibly generic) name, optionally with a `|`-separated label.
fn is_reference(inner: &str) -> bool {
    !inner.trim().is_empty()
        && inner.chars().all(|c| {
            c.is_alphanumeric() || matches!(c, '_' | '.' | '|' | '<' | '>' | ' ' | '?' | '*')
        })
}

/// Splits the first whitespace-delimited word off `text`.
fn split_first(text: &str) -> (&str, &str) {
    match text.trim().split_once(char::is_whitespace) {
        Some((first, rest)) => (first, rest.trim_start()),
        None => (text.trim(), ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_and_tags() {
        let rendered = render(
            "/**\n * The greeting.\n *\n * More text.\n *\n * @param name the name\n * @return the greeting\n * @throws IllegalStateException when closed\n * @see [other]\n */",
            None,
        );
        assert!(rendered.starts_with("The greeting."), "{rendered}");
        assert!(
            rendered.contains("**Parameters:**\n- `name` the name"),
            "{rendered}"
        );
        assert!(rendered.contains("**Returns:** the greeting"), "{rendered}");
        assert!(
            rendered.contains("**Throws:**\n- `IllegalStateException` when closed"),
            "{rendered}"
        );
        assert!(rendered.contains("**See also:**\n- `other`"), "{rendered}");
    }

    #[test]
    fn reference_links_become_code_spans() {
        assert_eq!(render_inline("See [Point] here"), "See `Point` here");
        assert_eq!(render_inline("See [Point|the point]"), "See `the point`");
        assert_eq!(
            render_inline("A [link](https://example.com) stays"),
            "A [link](https://example.com) stays"
        );
    }

    #[test]
    fn tooling_only_tags_render_empty() {
        // No description and only tooling tags: the hover shows no docs.
        assert_eq!(
            render("/**\n * @author someone\n * @since 1.0\n */", None).trim(),
            ""
        );
    }

    #[test]
    fn kdoc_lines_are_unwrapped() {
        let rendered = render("/**\n * One\n * @param a first\n *   continued\n */", None);
        assert!(rendered.contains("- `a` first continued"), "{rendered}");
    }
}
