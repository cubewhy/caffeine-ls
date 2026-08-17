use kotlin_syntax::SourceFile;

#[test]
fn source_file_parse_reports_errors() {
    let text = "fun main() {\n    val s = \"unterminated\n}";
    let parse = SourceFile::parse(text);
    let (_green, errors) = parse.into();

    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].kind.desc(), "unterminated string literal");
}

#[test]
fn source_file_parse_without_errors() {
    let text = "fun main() {\n    println(\"hello\")\n}";
    let parse = SourceFile::parse(text);
    let (_green, errors) = parse.into();

    assert!(errors.is_empty());
}
