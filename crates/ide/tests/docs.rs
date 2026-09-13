//! Insta snapshot tests for the Javadoc → Markdown renderer
//! ([`ide::docs::render_javadoc`]), one case per rule of the JDK 25
//! doc-comment specification.

use insta::assert_snapshot;

/// The traditional form of the spec's leading-asterisk example: the leading
/// whitespace and asterisks of each line go, the whitespace after them stays.
#[test]
fn leading_asterisks() {
    let raw = "/**
 * This is a comment.
 *   Indented by two spaces.
 */";
    assert_snapshot!("leading_asterisks", ide::docs::render_javadoc(raw));
}

/// Escape sequences are context-sensitive and apply at the beginning of a
/// line: `@@` and `@*` are unescaped there, and a `@` elsewhere is text.
#[test]
fn escapes() {
    let raw = "/**
 * @@escaped at the start
 * @*star at the start
 * and @mid-line text
 */";
    assert_snapshot!("escapes", ide::docs::render_javadoc(raw));
}

/// The HTML a traditional comment carries: paragraphs, breaks, inline markup,
/// lists and links.
#[test]
fn html() {
    let raw = "/**
 * First paragraph.
 * <p>Second <b>bold</b> and <i>italic</i> and <code>code</code>.
 * <br>After a break.
 * <ul>
 * <li>one</li>
 * <li>two</li>
 * </ul>
 * <dl><dt>term</dt><dd>definition</dd></dl>
 * <blockquote>quoted</blockquote>
 * <h3>Heading</h3>
 * <hr>
 * See <a href=\"https://example.com/a?x=1&amp;y=2\">the site</a> and <a name=\"x\">this</a>.
 * <table><tr><td>unknown elements</td></tr></table>
 */";
    assert_snapshot!("html", ide::docs::render_javadoc(raw));
}

/// `pre` keeps its content verbatim, in a fenced block.
#[test]
fn html_pre() {
    let raw = "/** <pre>
 * if (a < b) {
 *     run();  // <b>not markup</b>
 * }
 * </pre> An example. */";
    assert_snapshot!("html_pre", ide::docs::render_javadoc(raw));
}

/// Entities are decoded — the named ones and numeric ones — while an entity
/// the renderer does not know is left as written. A decoded entity is text,
/// not markup.
#[test]
fn entities() {
    let raw = "/** &amp; &lt; &gt; &quot; &apos; &#39; &nbsp; &copy; &lbrace;x&rbrace; &#65; &#x42; &unknown; &lt;b&gt; */";
    assert_snapshot!("entities", ide::docs::render_javadoc(raw));
}

/// Every inline tag of the specification, in one comment — including one that
/// spans lines. A bare `{@value}` needs the enclosing declaration's name and
/// is snapshotted separately.
#[test]
fn inline_tags() {
    let raw = "/** {@code A<B>C} {@literal 3 < 4} {@link java.util.List}\n\
               * {@link java.util.List a list} {@linkplain #run() runs} {@value Foo#MAX}\n\
               * {@summary A summary.} {@systemProperty java.home} {@docRoot} {@index word}\n\
               * {@snippet :} {@inheritDoc} {@unknown something}\n\
               * {@link\n\
               *   java.util.Map}\n\
               */\n";
    assert_snapshot!("inline_tags", ide::docs::render_javadoc(raw));
}

/// A bare `{@value}` displays the constant's own simple name, which the
/// renderer is told about; without it the tag stays as written.
#[test]
fn inline_value() {
    let raw = "/** The maximum, {@value}, of {@value #MIN}. */";
    let named = ide::docs::render_javadoc_of(raw, "MAX");
    let unnamed = ide::docs::render_javadoc(raw);
    assert_snapshot!(
        "inline_value",
        format!("named:\n{named}\n\nunnamed:\n{unnamed}")
    );
}

/// The standard block tags, each in its own section, and the hoisted
/// `@deprecated` paragraph that precedes the main description.
#[test]
fn block_tags() {
    let raw = "/**
 * Adds two numbers.
 *
 * @deprecated Use {@link #add(long, long)} instead.
 * @param a the first number
 * @param <T> the type of the numbers
 * @return the sum
 * @throws ArithmeticException if the sum overflows
 * @exception IllegalStateException if it is not
 * @see #add(long, long)
 * @see <a href=\"https://example.com\">the manual</a>
 * @since 1.2
 */";
    assert_snapshot!("block_tags", ide::docs::render_javadoc(raw));
}

/// An unknown block tag keeps its content under its own name; the tags the
/// standard doclet gates behind an option are tooling-only and dropped.
#[test]
fn unknown_block_tags() {
    let raw = "/**
 * Description.
 *
 * @apiNote This is a note.
 * @author Someone
 * @version 1.0
 * @hidden
 * @serial include
 */";
    assert_snapshot!("unknown_block_tags", ide::docs::render_javadoc(raw));
}

/// A comment with no main description at all: only the tags' sections are
/// rendered.
#[test]
fn missing_description() {
    let raw = "/**
 * @param x the value
 */";
    assert_snapshot!("missing_description", ide::docs::render_javadoc(raw));
}

/// A comment that documents nothing an editor can show renders empty.
#[test]
fn tooling_only_renders_empty() {
    let raw = "/**
 * @author Someone
 */";
    assert_snapshot!("tooling_only_renders_empty", ide::docs::render_javadoc(raw));
}

/// A Markdown comment: the run's shared indentation is shifted out, the
/// trailing whitespace of a line is kept (it may be a hard break), and a
/// fenced code block survives with its own indentation.
#[test]
fn markdown_comment() {
    let raw = "/// The description.
    ///
    ///     an indented code block
    ///
    /// ```java
    /// /** Hello World! */
    /// public class HelloWorld {}
    /// ```
    ///
    /// @param name the name
    /// @return the greeting
    ";
    assert_snapshot!("markdown_comment", ide::docs::render_javadoc(raw));
}

/// In a Markdown comment, HTML is CommonMark content: it passes through
/// untouched. Inline tags are still converted.
#[test]
fn markdown_html_passthrough() {
    let raw = "/// Use <b>bold</b> and {@code <T>} and &amp; as written.\n";
    assert_snapshot!("markdown_html_passthrough", ide::docs::render_javadoc(raw));
}

/// An unterminated traditional comment (the lexer's `UnterminatedComment`)
/// keeps the text it has.
#[test]
fn unterminated_comment() {
    let raw = "/** There is no end";
    assert_snapshot!("unterminated_comment", ide::docs::render_javadoc(raw));
}
