#[macro_export]
macro_rules! lsp_fixture {
    ($harness:expr, $fixture:expr) => {{
        let fixtures = $crate::fixture::parse_fixtures($fixture);
        for fixture in fixtures {
            $harness.write_fixture_file(fixture.path, fixture.content);
        }
    }};
}
