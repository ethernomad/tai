// Integration test for the `retrieve_webpage` tool: renders a real page in a
// local headless Chromium/Chrome and returns its HTML.
//
// This lives in tests/ per AGENTS.md (system-boundary: it launches a browser
// process and hits the network) and is marked #[ignore] so plain `cargo test`
// runs only the unit tests. Run it with the integration alias
// (`cargo test-integration`) on a host that has Chromium/Chrome installed and
// network access. If no browser is found the test skips gracefully.
use choreo_ai_protocols::ChatToolCall;
use choreo_daemon::tools::{ToolOutputFormat, ToolRegistry};

#[test]
#[ignore]
fn retrieve_webpage_renders_a_real_page() {
    // No per-test timeout under the stdlib harness, so a hung browser (or a
    // networking stall) would block CI forever. Watchdog-abort if the body
    // outlives a generous budget; nextest's slow-timeout is belt-and-braces.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(90));
        eprintln!("retrieve_webpage_integration: exceeded 90s; aborting to avoid a hang");
        std::process::abort();
    });

    let registry = ToolRegistry::new().build();

    let tool_call = ChatToolCall {
        id: "call_1".to_string(),
        name: "retrieve_webpage".to_string(),
        // Fetch the HTML (default action is "content").
        arguments_json:
            r#"{"url": "https://example.com", "action": "content", "timeout_ms": 20000}"#
                .to_string(),
        caller: None,
    };

    let output = registry
        .execute_json(
            &tool_call,
            ToolOutputFormat::Text,
            None, // x_credentials
            None, // working_dir
            None, // ctx
            None, // image_tx
        )
        .expect("tool execution should return");

    // If no browser is installed on this host, the tool returns a clear error
    // instead of panicking — treat that as a skip so the ignored suite stays
    // green on browser-less machines.
    if output.is_error && output.content.contains("no chromium or chrome binary") {
        eprintln!("retrieve_webpage_integration: skipping (no chromium/chrome installed)");
        return;
    }

    assert!(
        !output.is_error,
        "retrieve_webpage should succeed: {}",
        output.content
    );
    assert!(
        output.content.contains("Example Domain"),
        "rendered HTML should contain the page title, got: {}",
        output.content.chars().take(300).collect::<String>()
    );

    // The tool must be advertised as part of the always-on `core` group.
    let mut active = std::collections::HashSet::new();
    active.insert("core".to_string());
    let defs = registry.available_definitions(&active);
    assert!(
        defs.iter().any(|d| d.function.name == "retrieve_webpage"),
        "retrieve_webpage should be in the core group's definitions"
    );
}

/// `file://` URLs must render a local file directly in the browser (no http
/// round-trip). Chromium resolves and loads them natively, so this needs no
/// network and is just as suitable for the browser-boundary test suite.
#[test]
#[ignore]
fn retrieve_webpage_renders_a_local_file() {
    // Watchdog: a hung browser (or a stuck CDP prompt) would block CI forever.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(90));
        eprintln!("retrieve_webpage_integration: exceeded 90s; aborting to avoid a hang");
        std::process::abort();
    });

    // A distinctive marker so the test cannot false-positive on an empty page.
    const MARKER: &str = "choreo-file-scheme-marker";
    let dir = std::env::temp_dir().join("choreo-retrieve-webpage-file-test");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("index.html");
    std::fs::write(
        &path,
        format!("<!doctype html><html><body><p>{MARKER}</p></body></html>"),
    )
    .expect("write temp html");
    let url = url::Url::from_file_path(&path)
        .expect("temp path should convert to a file:// URL")
        .to_string();

    let registry = ToolRegistry::new().build();
    let tool_call = ChatToolCall {
        id: "call_2".to_string(),
        name: "retrieve_webpage".to_string(),
        arguments_json: format!(r#"{{"url": "{url}", "action": "content", "timeout_ms": 20000}}"#),
        caller: None,
    };

    let output = registry
        .execute_json(
            &tool_call,
            ToolOutputFormat::Text,
            None, // x_credentials
            None, // working_dir
            None, // ctx
            None, // image_tx
        )
        .expect("tool execution should return");

    // Same graceful skip policy as the http test: browser-less hosts stay green.
    if output.is_error && output.content.contains("no chromium or chrome binary") {
        eprintln!("retrieve_webpage_integration: skipping (no chromium/chrome installed)");
        return;
    }

    assert!(
        !output.is_error,
        "file:// retrieve_webpage should succeed: {}",
        output.content
    );
    assert!(
        output.content.contains(MARKER),
        "rendered HTML should contain the marker, got: {}",
        output.content.chars().take(300).collect::<String>()
    );
}

/// Element-scoped screenshots must capture the target element even when it
/// sits **below the fold**. Regression test for the bug where headless_chrome's
/// `Element::capture_screenshot` clipped against the viewport with
/// `captureBeyondViewport` unset, so off-screen elements came back as solid
/// body-background pixels. The fix clips against the element's document-space
/// box with `captureBeyondViewport: true`.
///
/// The test page is tall (3000px of spacer) so the target div starts well past
/// the default 800px viewport, and the target div is a distinctive solid color
/// with known dimensions so a blank capture is detectable: we decode the PNG
/// and assert at least one pixel is exactly that color (a viewport-clip bug
/// would return only the white body background).
#[test]
#[ignore]
fn retrieve_webpage_element_screenshot_below_the_fold() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(90));
        eprintln!("retrieve_webpage_integration: exceeded 90s; aborting to avoid a hang");
        std::process::abort();
    });

    let dir = std::env::temp_dir().join("choreo-retrieve-webpage-elem-shot-test");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("index.html");
    std::fs::write(
        &path,
        "<!doctype html><html><body style='margin:0;background:white'>\
           <div style='height:3000px'></div>\
           <div id='target' style='width:400px;height:200px;background:rgb(255,0,0)'></div>\
         </body></html>",
    )
    .expect("write temp html");
    let url = url::Url::from_file_path(&path)
        .expect("temp path should convert to a file:// URL")
        .to_string();

    let out_path = dir.join("target-shot.png");
    let registry = ToolRegistry::new().build();
    let tool_call = ChatToolCall {
        id: "call_3".to_string(),
        name: "retrieve_webpage".to_string(),
        arguments_json: format!(
            r##"{{"url": "{url}", "action": "screenshot", "selector": "#target", "output_path": "{}", "timeout_ms": 20000}}"##,
            out_path.display()
        ),
        caller: None,
    };

    let output = registry
        .execute_json(&tool_call, ToolOutputFormat::Text, None, None, None, None)
        .expect("tool execution should return");

    if output.is_error && output.content.contains("no chromium or chrome binary") {
        eprintln!("retrieve_webpage_integration: skipping (no chromium/chrome installed)");
        return;
    }
    assert!(
        !output.is_error,
        "element screenshot should succeed: {}",
        output.content
    );

    let png = std::fs::read(&out_path).expect("screenshot should be written to output_path");
    let img = image::load_from_memory(&png).expect("output should be a valid PNG");
    assert!(
        img.to_rgba8()
            .pixels()
            .any(|p| p.0[0] == 255 && p.0[1] == 0 && p.0[2] == 0),
        "element screenshot must contain the target's red pixels (blank capture = viewport-clip bug)"
    );
}
