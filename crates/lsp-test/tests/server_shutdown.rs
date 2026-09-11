//! The harness's shutdown contract: a server thread that panics is the failure
//! the test reports, not the client-side send error that follows from the dead
//! connection.

use lsp_server::Connection;
use lsp_test::LspHarness;
use serde_json::json;

#[test]
#[should_panic(expected = "server exploded")]
fn a_panicking_server_is_re_raised_by_drop() {
    let _lsp = LspHarness::start_with_setup(
        json!({}),
        |_| {},
        |connection: Connection| {
            let (id, _params) = connection.initialize_start().unwrap();
            connection
                .initialize_finish(id, json!({ "capabilities": {} }))
                .unwrap();
            // Let the client finish its own handshake before dying, so the death is
            // observed by the join in `Drop` rather than by a request.
            std::thread::sleep(std::time::Duration::from_millis(300));
            panic!("server exploded");
        },
    );
}
