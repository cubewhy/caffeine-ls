#[macro_export]
macro_rules! lsp_fixture {
    ($harness:expr, $fixture:expr) => {{
        let fixtures = $crate::fixture::parse_fixtures($fixture);
        for fixture in fixtures {
            $harness.write_fixture_file(fixture.path, fixture.content);
        }
    }};
}

#[macro_export]
macro_rules! lsp_test {
    (
        backend: $backend_init:expr,
        config: $config:expr,
        fixture: $fixture:expr,
        async |$harness:ident| $test_body:block
    ) => {
        #[tokio::test]
        async fn test_case() {
            let harness = LspHarness::start($config, $backend_init);
            let $harness = &harness;

            $crate::lsp_fixture!($harness, $fixture);

            $test_body
        }
    };
}

#[macro_export]
macro_rules! request {
    ($harness:expr, $method:expr, $params:tt) => {
        $harness.request($method, serde_json::json!($params))
    };
}
