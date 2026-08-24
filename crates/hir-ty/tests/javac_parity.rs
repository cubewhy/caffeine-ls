//! javac-parity snapshot suite: each fixture is compiled both by our type
//! layer (against the *real* JDK jimage, so resolution matches) and by the
//! `javac` binary of the same JDK, and the two diagnostic sets are rendered
//! side by side with a `MATCH`/`MISMATCH` verdict per code. These are the
//! executable form of `docs/jls-coverage.md` and `java_syntax::diag_status`.
//!
//! Skipped (with a note) when no JDK is found at test time. Fixtures are
//! single-focus because javac stops attributing a body after certain errors.

#[macro_use]
mod common;

use std::{collections::HashMap, fs, path::Path, process::Command};

use common::TestDatabase;
use hir_expand::body::BodyTree;
use hir_ty::TypeError;
use syntax::DiagnosticCode;

/// A javac diagnostic parsed from the subprocess output.
struct JavacDiag {
    /// 0-based line.
    line: u32,
    /// 0-based column.
    col: u32,
    code: String,
    message: String,
}

/// A diagnostic of ours, rendered into the same currency.
struct Ours {
    line: u32,
    col: u32,
    code: String,
    message: String,
}

fn wire_code(code: DiagnosticCode) -> String {
    match code {
        DiagnosticCode::Java(c) => c.javac_code().unwrap_or_else(|| c.as_str()).to_owned(),
        DiagnosticCode::Kotlin(_) => unreachable!(),
    }
}

/// Runs the real `javac` of `home` on `files` and returns its diagnostics:
/// canonical `compiler.*` code + 1-based→0-based position from the
/// `-XDrawDiagnostics` pass, plus the verbatim default-mode message block.
fn run_javac(home: &Path, files: &[(&str, &str)]) -> Vec<JavacDiag> {
    let javac = home.join("bin/javac");
    let dir = tempfile::TempDir::new().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();
    let mut paths = Vec::new();
    for (path, text) in files {
        let p = src.join(path.trim_start_matches('/'));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        paths.push(p);
    }

    let default = run(&javac, &[], dir.path().join("out").as_path(), &paths);
    // javac's default header carries a line but no column; key by 0-based line.
    let messages = default_message_blocks(&default);

    let drawn = run(
        &javac,
        &["-XDrawDiagnostics"],
        dir.path().join("out2").as_path(),
        &paths,
    );

    let mut out = Vec::new();
    for line in drawn.lines() {
        let mut split = line.splitn(4, ':');
        let _ = split.next();
        let Some(line_no) = split.next().and_then(|l| l.parse::<u32>().ok()) else {
            continue;
        };
        let Some(col) = split.next().and_then(|c| c.parse::<u32>().ok()) else {
            continue;
        };
        let Some(rest) = split.next() else { continue };
        let rest = rest.trim_start();
        // The code token is `compiler.<kind>.<key>` (it may be followed by a
        // `:` + args); drop any trailing colon.
        let code = rest
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_end_matches(':')
            .to_owned();
        if !code.starts_with("compiler.") {
            continue;
        }
        let message = messages
            .get(&line_no.saturating_sub(1))
            .cloned()
            .unwrap_or_default();
        out.push(JavacDiag {
            line: line_no.saturating_sub(1),
            col: col.saturating_sub(1),
            code,
            message,
        });
    }
    out
}

fn run(javac: &Path, flags: &[&str], out: &Path, paths: &[std::path::PathBuf]) -> String {
    let mut cmd = Command::new(javac);
    cmd.arg("-proc:none").arg("-Xlint:none");
    cmd.args(flags);
    cmd.arg("-d").arg(out);
    cmd.args(paths);
    let output = cmd.output().unwrap();
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Parses default-mode javac output into `0-based line → verbatim message`,
/// dropping the source/caret snapshot lines. The default header has no column:
/// `file:LINE: error: message`.
fn default_message_blocks(stderr: &str) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    let lines: Vec<&str> = stderr.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let mut split = line.splitn(3, ':');
        let _ = split.next();
        let Some(line_no) = split.next().and_then(|l| l.parse::<u32>().ok()) else {
            i += 1;
            continue;
        };
        let Some(rest) = split.next() else {
            i += 1;
            continue;
        };
        let rest = rest.trim_start();
        let Some(severity) = rest
            .strip_prefix("error: ")
            .or_else(|| rest.strip_prefix("warning: "))
        else {
            i += 1;
            continue;
        };
        let main = severity.to_owned();

        // Skip the source snapshot and the caret line (caret starts with `^`).
        let mut j = i + 1;
        if j < lines.len() && lines[j].trim_start().starts_with('^') {
            j += 1;
        } else if j + 1 < lines.len() && lines[j + 1].trim_start().starts_with('^') {
            j += 2;
        }

        let mut block = vec![main];
        while j < lines.len() {
            let next = lines[j];
            if next.trim().is_empty() {
                break;
            }
            // Stop at a summary line ("1 error", "2 warnings").
            if next
                .trim()
                .strip_prefix(|c: char| c.is_ascii_digit())
                .map(|rest| rest.starts_with(" error") || rest.starts_with(" warning"))
                .unwrap_or(false)
            {
                break;
            }
            // Stop at the next diagnostic header (`file:LINE: ...`).
            if next
                .splitn(3, ':')
                .nth(2)
                .map(|r| {
                    let r = r.trim_start();
                    r.starts_with("error: ") || r.starts_with("warning: ")
                })
                .unwrap_or(false)
            {
                break;
            }
            block.push(next.to_owned());
            j += 1;
        }
        let message = block.join("\n").trim_end().to_owned();
        map.insert(line_no.saturating_sub(1), message);
        i = j + 1;
    }
    map
}

/// Our diagnostics for `files`, mirroring the `ide_diagnostics` chain
/// (syntax + type + declaration), against the real-JDK source set of `db`.
fn our_diagnostics(db: &TestDatabase, files: &[(&str, &str)]) -> Vec<Ours> {
    let mut out = Vec::new();
    for (i, (_path, text)) in files.iter().enumerate() {
        let file_id = vfs::FileId::from_raw((i + 1) as u32);
        let line_index = line_index::LineIndex::new(text);
        let tree = hir::file_item_tree(db, file_id);

        for err in common::parse_syntax_errors(db, file_id, text) {
            let start = line_index.line_col(err.range.start());
            out.push(Ours {
                line: start.line,
                col: start.col,
                code: err
                    .code
                    .map(wire_code)
                    .unwrap_or_else(|| "<none>".to_owned()),
                message: err.message,
            });
        }
        for (id, _) in common::all_items(&tree) {
            let Some(types) = hir_ty::body_types(db, file_id, id) else {
                continue;
            };
            for diag in &types.diagnostics {
                let Some(range) = diag.range(&tree.bodies) else {
                    continue;
                };
                let start = line_index.line_col(range.start());
                out.push(Ours {
                    line: start.line,
                    col: start.col,
                    code: wire_code(diag.code()),
                    message: ty_message(diag, &tree.bodies),
                });
            }
        }
        for diag in hir_ty::class_diagnostics(db, file_id) {
            let Some(range) = diag.range() else { continue };
            let start = line_index.line_col(range.start());
            out.push(Ours {
                line: start.line,
                col: start.col,
                code: wire_code(diag.code()),
                message: diag.message(),
            });
        }
    }
    out.sort_by_key(|d| (d.line, d.col));
    out
}

/// The human-readable message of a type diagnostic (§-referenced in
/// `hir_ty::diagnostics`).
fn ty_message(diag: &TypeError, tree: &BodyTree) -> String {
    diag.message(tree)
}

/// Renders both sides of a parity fixture and the per-code verdict.
fn render_parity(files: &[(&str, &str)]) -> String {
    let (db, home) = setup(files)
        .expect("javac parity requires a JDK; set JAVA_HOME (or CAFFEINE_LS_JAVA_HOME)");
    let ours = our_diagnostics(&db, files);
    let javac = run_javac(home.as_std_path(), files);

    let mut lines = Vec::new();
    for (path, text) in files {
        lines.push(format!("FILE {path}:\n{text}"));
    }
    lines.push("LSP (ours):".to_owned());
    for d in &ours {
        lines.push(format!("{}:{}: {}: {}", d.line, d.col, d.code, d.message));
    }
    lines.push("JAVAC:".to_owned());
    for d in &javac {
        lines.push(format!("{}:{}: {}: {}", d.line, d.col, d.code, d.message));
    }

    let ours_by_pos: HashMap<(u32, u32), &Ours> =
        ours.iter().map(|d| ((d.line, d.col), d)).collect();
    let javac_by_pos: HashMap<(u32, u32), &JavacDiag> =
        javac.iter().map(|d| ((d.line, d.col), d)).collect();

    let mut matched = 0;
    let mut javac_only = 0;
    let mut ours_only = 0;
    let mut mismatches = Vec::new();
    let mut matched_codes = std::collections::BTreeSet::new();

    for (pos, jd) in &javac_by_pos {
        match ours_by_pos.get(pos) {
            Some(o) if o.code == jd.code => {
                matched += 1;
                matched_codes.insert(jd.code.as_str());
                if o.message != jd.message {
                    mismatches.push(format!(
                        "  {}\n    ours:  {}\n    javac: {}",
                        jd.code, o.message, jd.message
                    ));
                }
            }
            Some(o) => {
                javac_only += 1;
                mismatches.push(format!(
                    "  code {}\n    ours:  @{:?} {}\n    javac: @{:?} {}",
                    jd.code, pos, o.code, pos, jd.code
                ));
            }
            None => {
                javac_only += 1;
            }
        }
    }
    for (pos, o) in &ours_by_pos {
        if !javac_by_pos.contains_key(pos)
            || javac_by_pos
                .get(pos)
                .map(|j| j.code != o.code)
                .unwrap_or(false)
        {
            ours_only += 1;
            if !javac_by_pos.contains_key(pos) && !matched_codes.contains(o.code.as_str()) {
                mismatches.push(format!("  ours-only @{pos:?}: {}", o.code));
            }
        }
    }

    lines.push("VERDICT:".to_owned());
    lines.push(format!(
        "  matched: {matched} · javac-only: {javac_only} · ours-only: {ours_only}",
    ));
    for m in &mismatches {
        lines.push("MISMATCH:".to_owned());
        lines.push(m.clone());
    }
    lines.join("\n")
}

/// Builds a real-JDK source set owning `files`; `None` when no JDK exists.
fn setup(files: &[(&str, &str)]) -> Option<(TestDatabase, camino::Utf8PathBuf)> {
    let mut db = TestDatabase::new();
    let jdk = common::register_real_jdk(&mut db)?;
    common::register_source_set_with_jdk(&mut db, &jdk, files);
    Some((db, jdk.home))
}

snapshot!(
    parity_cant_resolve_name,
    render_parity(&[(
        "/src/Resolve.java",
        "\
class Resolve {
    void t() {
        missingVar;
    }
}
",
    )])
);
// §6.5: a bare name with no local, field or implicit-receiver member fails to
// resolve; javac 25 reports `compiler.err.cant.resolve.location`.

snapshot!(
    parity_not_stmt,
    render_parity(&[(
        "/src/Stmt.java",
        "\
class Stmt {
    void t() {
        int a = 1;
        missingThing;
    }
}
",
    )])
);
// §14.8: an identifier can only start a statement when it is a statement
// expression (method call, `new`, assignment, inc/dec). A bare name is
// "not a statement" (`compiler.err.not.stmt`) — javac additionally refuses
// to attribute the rest of the block.

snapshot!(
    parity_var_without_initializer,
    render_parity(&[(
        "/src/Var.java",
        "\
class Var {
    void t() {
        var x;
    }
}
",
    )])
);
// §14.4.1: a `var` local's type is inferred from its initializer; without one
// javac reports `compiler.err.cant.infer.local.var.type`.

snapshot!(
    parity_throw_non_throwable,
    render_parity(&[(
        "/src/Throwable.java",
        "\
class Throwable {
    void t() {
        throw new Object();
    }
}
",
    )])
);
// §14.18: a `throw` operand must be assignable to `Throwable`. javac 25
// renders the conversion failure with simple type names
// ("Object cannot be converted to Throwable"): `compiler.err.prob.found.req`.

snapshot!(
    parity_non_boolean_condition,
    render_parity(&[(
        "/src/Cond.java",
        "\
class Cond {
    boolean t() {
        if (1) { return true; }
        return false;
    }
}
",
    )])
);
// §14.9: `if` requires a `boolean` condition; javac maps it to
// `compiler.err.prob.found.req` with `misc.inconvertible.types`.

snapshot!(
    parity_wrong_arity,
    render_parity(&[(
        "/src/Arity.java",
        "\
class Arity {
    void t() {
        \"x\".length(1, 2);
    }
}
",
    )])
);
// §15.12.2: no member of `length` is applicable; javac reports
// `compiler.err.cant.apply.symbol` with the required/found/reason block.

snapshot!(
    parity_incomparable,
    render_parity(&[(
        "/src/Cmp.java",
        "\
class Cmp {
    void t() {
        boolean b = 1 == \"x\";
    }
}
",
    )])
);
// §15.21: `==` between `int` and `String`; javac reports
// `compiler.err.operator.cant.be.applied.1`.

snapshot!(
    parity_for_each,
    render_parity(&[(
        "/src/Each.java",
        "\
class Each {
    void t() {
        for (int x : 3) {}
    }
}
",
    )])
);
// §14.14.2: the for-each expression is neither an array nor an `Iterable`;
// javac reports `compiler.err.foreach.not.applicable.to.type`.

snapshot!(
    parity_generic_array,
    render_parity(&[(
        "/src/GArray.java",
        "\
import java.util.List;

class GArray {
    void t() {
        List<String>[] a = new List<String>[3];
    }
}
",
    )])
);
// §15.10.2: a non-reifiable component type cannot seed an array creation;
// javac reports `compiler.err.generic.array.creation` and the `new` itself.

snapshot!(
    parity_abstract_new,
    render_parity(&[(
        "/src/Abstract.java",
        "\
abstract class A {}
class Abstract {
    void t() {
        new A();
    }
}
",
    )])
);
// §15.9.2: instantiating an abstract class is an error
// (`compiler.err.abstract.cant.be.instantiated`).

snapshot!(
    parity_already_caught,
    render_parity(&[(
        "/src/Caught.java",
        "\
import java.io.IOException;

class Caught {
    void t() {
        try {
            maybe();
        } catch (Exception e) {
        } catch (IOException e) {
        }
    }

    void maybe() throws IOException {}
}
",
    )])
);
// §11.2.3/§14.20: a catch clause whose type is a subtype of an earlier
// clause's is unreachable; javac reports `compiler.err.except.already.caught`
// for the second clause.

snapshot!(
    parity_var_might_not_be_initialized,
    render_parity(&[(
        "/src/Da.java",
        "\
class Da {
    int t(boolean b) {
        int x;
        if (b) {
            x = 1;
        }
        return x;
    }
}
",
    )])
);
// §16: `x` is not definitely assigned at the `return`; javac reports
// `compiler.err.var.might.not.have.been.initialized`.

snapshot!(
    parity_const_case_label,
    render_parity(&[(
        "/src/Case.java",
        "\
class Case {
    void t(int x) {
        int y = x;
        switch (x) {
            case y:
                break;
            default:
        }
    }
}
",
    )])
);
// §14.11.1/§15.28: a case label must be a constant expression;
// javac reports `compiler.err.const.expr.req`.

snapshot!(
    parity_non_static_from_static,
    render_parity(&[(
        "/src/Static.java",
        "\
class Static {
    void m() {}

    static void t() {
        m();
    }
}
",
    )])
);
// §15.12.3/[§8.1.3]: an unqualified invocation of an instance method from a
// static context; javac reports `compiler.err.non-static.cant.be.ref`.
