//! Tests for the parallel workspace-wide diagnostics report
//! (see [`ide::Analysis::workspace_reports`]).

use std::{collections::HashMap, sync::Arc};

use hir::{Classpath, ProjectGraphData, SourceSetId, set_project_graph};
use ide::{Analysis, AnalysisHost, WorkspaceReport};
use ide_db::base_db::{FileChange, SourceRoot, SourceRootId};
use vfs::{AbsPathBuf, FileId, VfsPath, file_set::FileSet};

fn main_source_set(project: u32) -> SourceSetId {
    SourceSetId {
        project: project_model::ProjectId(project),
        kind: project_model::SourceSetKind::Main,
    }
}

struct Fixture {
    host: AnalysisHost,
    files: HashMap<u32, FileId>,
}

impl Fixture {
    fn analysis(&self) -> Analysis {
        self.host.snapshot()
    }

    fn file(&self, raw: u32) -> FileId {
        self.files[&raw]
    }
}

/// A fixture root: a source set and its files as `(raw_file_id, path, text)`.
type FixtureRoot = (SourceSetId, Vec<(u32, &'static str, &'static str)>);

/// Builds a host from fixture roots (mirrors the `symbol_snapshots` fixtures).
fn build(roots: &[FixtureRoot]) -> Fixture {
    let mut host = AnalysisHost::new();
    let mut change = FileChange::default();
    let mut all_roots = Vec::new();
    let mut data = ProjectGraphData::default();
    let mut files = HashMap::new();
    for (idx, (source_set, source_files)) in roots.iter().enumerate() {
        let mut file_set = FileSet::default();
        for (raw_id, path, text) in source_files {
            let file_id = FileId::from_raw(*raw_id);
            files.insert(*raw_id, file_id);
            file_set.insert(
                file_id,
                VfsPath::from(AbsPathBuf::assert_utf8(path.to_owned().into())),
            );
            change.change_file(file_id, Some(text.to_string()));
        }
        all_roots.push(SourceRoot::new(file_set));
        data.source_root_to_source_set
            .insert(SourceRootId(idx as u32), source_set.clone());
        data.source_sets.insert(
            source_set.clone(),
            Arc::new(Classpath {
                entries: Vec::new(),
            }),
        );
    }
    change.set_roots(all_roots);
    host.apply_change(change);
    set_project_graph(host.raw_database_mut(), data);
    Fixture { host, files }
}

/// The reports of `reports` keyed by file id.
fn by_file(reports: &[WorkspaceReport]) -> HashMap<FileId, &WorkspaceReport> {
    reports.iter().map(|report| (report.file, report)).collect()
}

/// The single-file [`ide::Analysis::file_report`] result, for comparison.
fn single(analysis: &Analysis, file: FileId) -> Vec<ide::Diagnostic> {
    analysis.file_report(file).unwrap().to_vec()
}

#[test]
fn workspace_reports_matches_per_file_reports() {
    let fixture = build(&[(
        main_source_set(0),
        vec![
            (1, "/src/p/A.java", "package p;\npublic class A {\n}\n"),
            (
                2,
                "/src/p/B.java",
                "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
            ),
            (
                3,
                "/src/p/C.java",
                "package p;\npublic class C {\n    int m() { return \"x\"; }\n}\n",
            ),
        ],
    )]);
    let analysis = fixture.analysis();
    let reports = analysis.workspace_reports(&[]).unwrap();

    // One report per source file, sorted by file id.
    assert_eq!(reports.len(), 3);
    assert_eq!(reports[0].file, fixture.file(1));
    assert_eq!(reports[1].file, fixture.file(2));
    assert_eq!(reports[2].file, fixture.file(3));

    // The parallel pull matches the single-file report for every file, and is
    // deterministic across repeated pulls — including the precomputed result
    // ids.
    for report in &reports {
        let expected = single(&analysis, report.file);
        assert_eq!(
            report.report.as_slice(),
            expected.as_slice(),
            "report mismatch for file {}",
            report.file.index()
        );
        assert!(
            !report.result_id.is_empty(),
            "every report carries a precomputed result id"
        );
    }
    let second = analysis.workspace_reports(&[]).unwrap();
    assert_eq!(
        reports
            .iter()
            .map(|r| r.result_id.as_str())
            .collect::<Vec<_>>(),
        second
            .iter()
            .map(|r| r.result_id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        reports
            .iter()
            .map(|r| r.report.as_slice())
            .collect::<Vec<_>>(),
        second
            .iter()
            .map(|r| r.report.as_slice())
            .collect::<Vec<_>>()
    );

    // A is clean; B carries the cross-file unresolved-method error; C carries
    // the type mismatch of its body.
    let map = by_file(&reports);
    assert!(map[&fixture.file(1)].report.is_empty());
    assert!(
        map[&fixture.file(2)]
            .report
            .iter()
            .any(|d| d.message.contains("go"))
    );
    assert!(
        map[&fixture.file(3)]
            .report
            .iter()
            .any(|d| d.message.contains("int"))
    );
}

#[test]
fn workspace_reports_empty_without_source_roots() {
    let fixture = build(&[]);
    assert!(
        fixture
            .analysis()
            .workspace_reports(&[])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn workspace_reports_reflect_incremental_edits() {
    let mut fixture = build(&[(
        main_source_set(0),
        vec![
            (1, "/src/p/A.java", "package p;\npublic class A {\n}\n"),
            (
                2,
                "/src/p/B.java",
                "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
            ),
        ],
    )]);

    let before = fixture.analysis().workspace_reports(&[]).unwrap();
    let map = by_file(&before);
    assert!(
        map[&fixture.file(2)]
            .report
            .iter()
            .any(|d| d.message.contains("go"))
    );

    // Fix A on the host (A gains the missing `go()`): a fresh snapshot's pull
    // must re-derive B's cross-file error away through the shared memo tables,
    // leaving A's report clean.
    let mut change = FileChange::default();
    change.change_file(
        fixture.file(1),
        Some("package p;\npublic class A {\n    public void go() {}\n}\n".to_string()),
    );
    fixture.host.apply_change(change);

    let after = fixture.analysis().workspace_reports(&[]).unwrap();
    let map = by_file(&after);
    assert!(map[&fixture.file(1)].report.is_empty());
    assert!(map[&fixture.file(2)].report.is_empty());
}

/// The precomputed `result_id` folds in the client lint keys: the same
/// unchanged report must hash differently under a different lint config, so a
/// `didChangeConfiguration` invalidates every cached id and forces full
/// re-sends.
#[test]
fn workspace_reports_result_ids_fold_lints() {
    let fixture = build(&[(
        main_source_set(0),
        vec![
            (1, "/src/p/A.java", "package p;\npublic class A {\n}\n"),
            (
                2,
                "/src/p/B.java",
                "package p;\npublic class B {\n    void m(A a) { a.go(); }\n}\n",
            ),
        ],
    )]);
    let analysis = fixture.analysis();

    let no_lints = analysis.workspace_reports(&[]).unwrap();
    let with_lints = analysis
        .workspace_reports(&["rawtypes".to_string(), "unchecked".to_string()])
        .unwrap();
    assert_eq!(no_lints.len(), with_lints.len());
    for (plain, lints) in no_lints.iter().zip(with_lints.iter()) {
        assert_eq!(plain.report, lints.report, "reports must be identical");
        assert_ne!(
            plain.result_id, lints.result_id,
            "a different lint config must invalidate the result id"
        );
    }
}
