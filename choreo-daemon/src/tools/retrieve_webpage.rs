use super::{PreparedImage, ToolExecError, context::ToolContext, human_size, resolve_path};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use choreo_keystore::ServiceCredential;
use headless_chrome::Tab;
use headless_chrome::protocol::cdp::Page;
use headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption;
use headless_chrome::{Browser, LaunchOptions};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tracing::{debug, info, warn};
use url::Url;

/// What `retrieve_webpage` should produce from the rendered page.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebpageAction {
    /// Fully rendered HTML (outerHTML); restricted to `selector` when given.
    Content,
    /// Human-readable plain text of the page (innerText); defaults to `<body>`.
    Text,
    /// A PNG screenshot, saved to `output_path` if given and shown inline.
    Screenshot,
    /// A PDF of the page; requires `output_path` (binary output can't be a string).
    Pdf,
}

impl WebpageAction {
    fn as_str(&self) -> &'static str {
        match self {
            WebpageAction::Content => "content",
            WebpageAction::Text => "text",
            WebpageAction::Screenshot => "screenshot",
            WebpageAction::Pdf => "pdf",
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct RetrieveWebpageArgs {
    /// URL to render (http, https, or file scheme; `file://` renders a local
    /// file directly in the browser).
    url: String,
    /// What to retrieve. Defaults to "content".
    action: Option<WebpageAction>,
    /// Milliseconds to wait after load before capturing (lets JS settle). 0 = none.
    wait_ms: Option<u64>,
    /// Navigation / element-wait timeout in milliseconds. Default 30_000.
    timeout_ms: Option<u64>,
    /// Viewport width. Default 1280.
    width: Option<u32>,
    /// Viewport height. Default 800.
    height: Option<u32>,
    /// Screenshot: capture the full scrollable page (surface). Default true.
    full_page: Option<bool>,
    /// Restrict content/text/screenshot capture to this CSS selector.
    selector: Option<String>,
    /// Where to write the result for `screenshot` / `pdf`. Resolved against the
    /// session working directory. PDFs require this.
    output_path: Option<String>,
}

/// Default viewport / nav timeout used when the caller omits them.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Hard cap on a single call's navigation timeout so a mistyped/malicious
/// argument can't pin a worker thread for an unbounded stretch.
const MAX_TIMEOUT_MS: u64 = 120_000;
/// Hard cap on the post-load settle delay for the same reason.
const MAX_WAIT_MS: u64 = 30_000;

/// Binary names/known paths to search for, in *preference* order: chromium
/// first, then the various chrome bundle names, so a Chromium install wins
/// over Chrome when both are present.
const CANDIDATE_NAMES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "google-chrome-beta",
    "google-chrome-unstable",
    "chrome",
    "microsoft-edge",
    "brave-browser",
];

/// Resolve a locally-installed Chromium/Chrome binary, preferring chromium.
///
/// Lookup order:
/// 1. `CHROMIUM_BIN`, then `CHROME_BIN` env overrides (must exist).
/// 2. Names on `PATH` (chromium first).
/// 3. Known absolute install paths for the current OS.
///
/// Returns `None` when nothing usable is found — the caller reports that as a
/// clear error telling the operator to install Chromium (this tool deliberately
/// does NOT auto-download a browser).
fn resolve_browser_binary() -> Option<std::path::PathBuf> {
    for var in ["CHROMIUM_BIN", "CHROME_BIN"] {
        if let Some(p) = std::env::var_os(var) {
            let candidate = std::path::PathBuf::from(p);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }

    // Search each PATH entry for a candidate executable name.
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in CANDIDATE_NAMES {
                let candidate = dir.join(name);
                if is_executable(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }

    // Known absolute locations (best-effort per OS), chromium first.
    #[cfg(target_os = "macos")]
    {
        let mac_paths = [
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ];
        for p in mac_paths {
            let candidate = std::path::PathBuf::from(p);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        for base in [
            "C:/Program Files/Chromium/Application/chrome.exe",
            "C:/Program Files/Google/Chrome/Application/chrome.exe",
            "C:/Program Files (x86)/Google/Chrome/Application/chrome.exe",
            "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",
        ] {
            let candidate = std::path::PathBuf::from(base);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }

    None
}

/// True when `path` exists as a file *and* (on Unix) is executable, so we don't
/// hand the launcher a non-executable or directory candidate that would then
/// fail at spawn time. Windows has no executable bit; existence is enough.
fn is_executable(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// True when `url` has an http, https, or file scheme — the schemes a headless
/// browser can be asked to render. `file://` lets the browser read arbitrary
/// local files from the daemon's host; that reach is intended and is not gated
/// behind any option (the browser process already runs under the same OS-level
/// sandbox as the daemon), so `file://` URLs are passed through untouched.
fn validate_url(url: &str) -> Result<(), ToolExecError> {
    let parsed = Url::parse(url).map_err(|e| ToolExecError(format!("invalid URL '{url}': {e}")))?;
    match parsed.scheme() {
        "http" | "https" | "file" => Ok(()),
        other => Err(ToolExecError(format!(
            "unsupported URL scheme '{other}'; only http/https/file are allowed"
        ))),
    }
}

/// Build the JS expression that extracts text (innerText) from a bound node.
/// `selector` is JSON-embedded so it can't break out of the string literal.
fn text_expression(selector: Option<&str>) -> String {
    match selector {
        Some(sel) => {
            let sel = serde_json::to_string(sel).unwrap_or_else(|_| "\"body\"".to_string());
            format!(
                "(() => {{ const e = document.querySelector({sel}); return e ? e.innerText : ''; }})()"
            )
        }
        None => "(() => { const e = document.body; return e ? e.innerText : ''; })()".to_string(),
    }
}

/// Build the JS expression that extracts HTML (outerHTML) from a bound node.
fn html_expression(selector: &str) -> String {
    let sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"html\"".to_string());
    format!("(() => {{ const e = document.querySelector({sel}); return e ? e.outerHTML : ''; }})()")
}

/// JS that measures the page's full scrollable content size, returned as a
/// `"<width>x<height>"` string. It's a *string* (not an array/object) on
/// purpose: the crate's `evaluate` hard-codes `returnByValue: false`, which
/// always yields `.value` for primitives but not for objects, so a primitive
/// result is the only one we can read back without a handle.
const PAGE_SIZE_JS: &str = "(() => { const d = document.documentElement; const w = Math.max(d.scrollWidth, d.clientWidth); \
     const h = Math.max(d.scrollHeight, d.clientHeight); return w + 'x' + h; })()";

/// Measure the full scrollable content size of the rendered page.
fn page_content_size(tab: &Tab) -> Result<(f64, f64), ToolExecError> {
    let obj = tab
        .evaluate(PAGE_SIZE_JS, false)
        .map_err(|e| ToolExecError(format!("failed to measure page size: {e:#}")))?;
    let raw = remote_text(&obj);
    let (w, h) = raw
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<f64>().ok()?, h.parse::<f64>().ok()?)))
        .unwrap_or((0.0, 0.0));
    debug!(raw_width = %raw, "measured page content size");
    Ok((w, h))
}

/// JS that measures an element's bounding box in **document space** (bounding
/// client rect plus current scroll offsets), returned as a `"x,y,w,h"` string.
/// A string (not an array/object) on purpose: the crate's `evaluate`
/// hard-codes `returnByValue: false`, which yields `.value` only for
/// primitives — see `PAGE_SIZE_JS` for the same rationale.
const ELEMENT_BOX_JS_TEMPLATE: &str = "(() => { const e = document.querySelector({sel}); if (!e) return ''; \
     const r = e.getBoundingClientRect(); \
     return (r.left + window.scrollX) + ',' + (r.top + window.scrollY) + ',' + r.width + ',' + r.height; })()";

/// Measure `selector`'s bounding box in document-space coordinates
/// (`(x, y, width, height)`). Returns `None` when the selector matches nothing
/// (the caller turns that into a user-facing "selector not found" error).
fn element_document_box(
    tab: &Tab,
    selector: &str,
) -> Result<Option<(f64, f64, f64, f64)>, ToolExecError> {
    let sel = serde_json::to_string(selector)
        .map_err(|e| ToolExecError(format!("failed to encode selector: {e}")))?;
    let expr = ELEMENT_BOX_JS_TEMPLATE.replace("{sel}", &sel);
    let obj = tab
        .evaluate(&expr, false)
        .map_err(|e| ToolExecError(format!("failed to measure element box: {e:#}")))?;
    let raw = remote_text(&obj);
    let parsed: Option<(f64, f64, f64, f64)> = raw
        .split(',')
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()
        .and_then(|v| {
            <[f64; 4]>::try_from(v)
                .ok()
                .map(|a| (a[0], a[1], a[2], a[3]))
        });
    debug!(raw = %raw, "measured element document box");
    Ok(parsed)
}

/// Capture a screenshot of the whole visible viewport (or a single element, via
/// `selector`). When `full_page`, this returns the entire scrollable page.
///
/// headless_chrome's `Tab::capture_screenshot` maps its last boolean to the CDP
/// `fromSurface` flag — *not* "full page" — and hard-codes
/// `captureBeyondViewport: None`, so it can only ever capture the current
/// viewport. For a real full-page shot we issue `Page.captureScreenshot`
/// ourselves with `captureBeyondViewport: true` plus a clip covering the whole
/// content area (measured in-page first).
fn capture_screenshot(
    tab: &Tab,
    selector: Option<&str>,
    full_page: bool,
) -> Result<Vec<u8>, ToolExecError> {
    if let Some(sel) = selector {
        // Element screenshots must NOT go through headless_chrome's
        // `Element::capture_screenshot`: it clips against the *viewport* with
        // `captureBeyondViewport` unset, so anything below the fold captures
        // blank body background. Instead, take the element's box in document
        // space and clip against it with `captureBeyondViewport: true` — the
        // same trick the full-page path uses below, so off-screen elements
        // render fully regardless of scroll position or sticky headers.
        let Some((x, y, w, h)) = element_document_box(tab, sel)? else {
            return Err(ToolExecError(format!(
                "selector '{sel}' matched no element"
            )));
        };
        if w <= 0.0 || h <= 0.0 {
            return Err(ToolExecError(format!(
                "selector '{sel}' matched an element with a zero-size box"
            )));
        }
        let result = tab
            .call_method(Page::CaptureScreenshot {
                format: Some(CaptureScreenshotFormatOption::Png),
                quality: None,
                clip: Some(Page::Viewport {
                    x,
                    y,
                    width: w,
                    height: h,
                    scale: 1.0,
                }),
                from_surface: Some(true),
                capture_beyond_viewport: Some(true),
                optimize_for_speed: None,
            })
            .map_err(|e| ToolExecError(format!("failed to screenshot element: {e:#}")))?;
        return BASE64
            .decode(result.data)
            .map_err(|e| ToolExecError(format!("element screenshot decode failed: {e}")));
    }

    if full_page {
        let (w, h) = page_content_size(tab)?;
        // A clip of (0,0,contentW,contentH) plus captureBeyondViewport lets
        // headless Chrome paint the regions outside the current viewport too.
        if w > 0.0 && h > 0.0 {
            let result = tab
                .call_method(Page::CaptureScreenshot {
                    format: Some(CaptureScreenshotFormatOption::Png),
                    quality: None,
                    clip: Some(Page::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: w,
                        height: h,
                        scale: 1.0,
                    }),
                    from_surface: Some(true),
                    capture_beyond_viewport: Some(true),
                    optimize_for_speed: None,
                })
                .map_err(|e| {
                    ToolExecError(format!("failed to capture full-page screenshot: {e:#}"))
                })?;
            return BASE64
                .decode(result.data)
                .map_err(|e| ToolExecError(format!("full-page screenshot decode failed: {e}")));
        }
        debug!(width = %w, height = %h, "content size was unusable; falling back to viewport shot");
    }

    tab.capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
        .map_err(|e| ToolExecError(format!("failed to capture screenshot: {e:#}")))
}

/// Decode PNG dimensions by reading only the 8-byte IHDR detail block, rather
/// than decoding the entire (potentially multi-MB) image. Screenshots here are
/// always PNG, so this is both faster and lighter than `image::load_from_memory`.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // Signature (8) + chunk length (4) + "IHDR" (4) + width (4) + height (4).
    const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() >= 24 && bytes[..8] == SIGNATURE && &bytes[12..16] == b"IHDR" {
        let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        Some((width, height))
    } else {
        None
    }
}

/// The `retrieve_webpage` tool's return value: a human-readable text handle
/// plus an optional screenshot, so the framework's `extract_image` hook reads
/// the image straight off the per-invocation return value (no shared state).
/// Only the Screenshot action sets `image`; Content/Text/Pdf set `None`. `impl
/// Serialize` emits only `text`, so the JSON tool result is a plain string
/// exactly as before.
#[derive(Debug)]
pub struct RetrieveWebpageReturn {
    /// The text handle (captured content, screenshot message, or saved-PDF
    /// message) shown to the model.
    pub text: String,
    /// The prepared screenshot handed to the client via `extract_image`, when
    /// the action was Screenshot.
    pub image: Option<PreparedImage>,
}

impl Serialize for RetrieveWebpageReturn {
    /// Serialize to just the text handle, keeping the JSON wire format a plain
    /// string (identical to the previous `Return = String`).
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

impl JsonSchema for RetrieveWebpageReturn {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("RetrieveWebpageReturn")
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string" })
    }
}

/// A tool that renders a URL in a local headless Chromium/Chrome and returns
/// page content (HTML), plain text, a screenshot, or a PDF.
pub struct RetrieveWebpage {}

impl RetrieveWebpage {
    pub fn new() -> Self {
        RetrieveWebpage {}
    }
}

impl Default for RetrieveWebpage {
    fn default() -> Self {
        Self::new()
    }
}

impl super::Tool for RetrieveWebpage {
    type Args = RetrieveWebpageArgs;
    type Return = RetrieveWebpageReturn;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "retrieve_webpage"
    }
    fn description(&self) -> &'static str {
        "Render a URL in a local headless Chromium/Chrome and return page content (HTML), plain text, a screenshot (PNG), or a PDF. Runs locally and offline; requires a chromium/chrome binary already installed (prefers chromium; override with CHROMIUM_BIN). Screenshots are returned inline or saved to output_path; PDFs require output_path. One-shot per call — no persistent session."
    }
    fn describe_invocation(&self, args: &Self::Args) -> String {
        let action = args
            .action
            .as_ref()
            .map(|a| a.as_str())
            .unwrap_or(WebpageAction::Content.as_str());
        let mut parts = vec![format!(
            "Retrieving web page ({action}). URL: {}.",
            args.url
        )];
        if let Some(sel) = args.selector.as_deref() {
            parts.push(format!(" Selector: {sel}."));
        }
        if let Some(out) = args.output_path.as_deref() {
            parts.push(format!(" Output: {out}."));
        }
        parts.concat()
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.text.clone()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let action = args.action.clone().unwrap_or(WebpageAction::Content);
        let url = args.url.trim();
        let action_str = action.as_str();
        validate_url(url)?;
        info!(url, action = action_str, "retrieve_webpage: rendering page");

        let binary = resolve_browser_binary().ok_or_else(|| {
            warn!("retrieve_webpage: no chromium/chrome binary found on PATH");
            ToolExecError(
                "no chromium or chrome binary found on PATH (or in standard locations). \
                 Install Chromium/Chrome, or set CHROMIUM_BIN / CHROME_BIN to its path"
                    .to_string(),
            )
        })?;
        debug!(path = %binary.display(), "resolved browser binary");

        // Navigation timeout is clamped so a single call can't pin a worker
        // thread indefinitely (a mistyped or hostile argument would otherwise).
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);
        let wait_ms = args.wait_ms.map(|ms| ms.min(MAX_WAIT_MS));

        // Launch a private, headless, one-shot browser instance with an
        // explicit path (so it uses the resolved chromium, never auto-detect)
        // and our viewport.
        let mut builder = LaunchOptions::default_builder();
        builder.headless(true);
        builder.path(Some(binary));
        builder.window_size(Some((
            args.width.unwrap_or(1280),
            args.height.unwrap_or(800),
        )));
        // Keep the DevTools socket alive well past the resolved navigation
        // timeout so a slow page can't get torn down mid-navigation. The idle
        // grace is derived from (and always larger than) the timeout, rather
        // than a fixed constant that could be shorter than a caller's timeout.
        builder.idle_browser_timeout(Duration::from_millis(
            timeout_ms.saturating_mul(2).max(60_000),
        ));
        let options = builder.build().map_err(|e| ToolExecError(e.to_string()))?;

        let browser = Browser::new(options)
            .map_err(|e| ToolExecError(format!("failed to launch headless browser: {e:#}")))?;
        debug!("launched headless browser");

        // Run the whole capture in a closure so the `Browser` (and its Chromium
        // child process) is released on every path, success or error, when it
        // drops. Explicit `close()` is best-effort on top of that.
        let outcome = (|| -> Result<RetrieveWebpageReturn, ToolExecError> {
            let tab = browser
                .new_tab()
                .map_err(|e| ToolExecError(format!("failed to open a tab: {e:#}")))?;

            tab.set_default_timeout(Duration::from_millis(timeout_ms));
            tab.navigate_to(url)
                .map_err(|e| ToolExecError(format!("navigation to {url} failed: {e:#}")))?;
            tab.wait_until_navigated()
                .map_err(|e| ToolExecError(format!("page never finished loading: {e:#}")))?;

            // Optional settle delay so client-side JS can run before capture.
            // Only reached at runtime (never in unit tests, per repo rules).
            if let Some(ms) = wait_ms
                && ms > 0
            {
                std::thread::sleep(Duration::from_millis(ms));
            }
            debug!(timeout_ms, wait_ms, "page navigated; capturing");

            Self::capture(&tab, &args, action, url, working_dir)
        })();

        match &outcome {
            Ok(..) => info!(url, action = action_str, "retrieve_webpage: ok"),
            Err(e) => warn!(url, action = action_str, error = %e, "retrieve_webpage: failed"),
        }

        // The `Browser` is dropped here (and its Chromium child process is
        // terminated by the transport's `Drop`), releasing the instance whether
        // the capture succeeded or failed — see the closure above.
        outcome
    }

    fn extract_image(&self, ret: &Self::Return) -> Option<PreparedImage> {
        ret.image.clone()
    }
}

impl RetrieveWebpage {
    /// Perform the per-action capture on an already-navigated tab. Broken out
    /// of `execute` so each branch stays small and testable; handles the file
    /// write + inline-image hand-off for the binary actions (screenshot/pdf).
    /// The screenshot is returned inside the [`RetrieveWebpageReturn`] so the
    /// framework's `extract_image` hook reads it off the per-invocation return
    /// value (no shared-state parking).
    fn capture(
        tab: &Tab,
        args: &RetrieveWebpageArgs,
        action: WebpageAction,
        url: &str,
        working_dir: Option<&Path>,
    ) -> Result<RetrieveWebpageReturn, ToolExecError> {
        match action {
            WebpageAction::Content => match args.selector.as_deref() {
                Some(sel) => {
                    let obj = tab
                        .evaluate(&html_expression(sel), false)
                        .map_err(|e| ToolExecError(format!("failed to extract HTML: {e:#}")))?;
                    Ok(RetrieveWebpageReturn {
                        text: remote_text(&obj),
                        image: None,
                    })
                }
                None => tab
                    .get_content()
                    .map(|text| RetrieveWebpageReturn { text, image: None })
                    .map_err(|e| ToolExecError(format!("failed to get page HTML: {e:#}"))),
            },

            WebpageAction::Text => {
                let expr = text_expression(args.selector.as_deref());
                let obj = tab
                    .evaluate(&expr, false)
                    .map_err(|e| ToolExecError(format!("failed to extract text: {e:#}")))?;
                Ok(RetrieveWebpageReturn {
                    text: remote_text(&obj),
                    image: None,
                })
            }

            WebpageAction::Screenshot => {
                let bytes = capture_screenshot(
                    tab,
                    args.selector.as_deref(),
                    args.full_page.unwrap_or(true),
                )?;

                // Dimensions come from a cheap PNG-header read, not a full decode.
                let (width, height) = png_dimensions(&bytes).unwrap_or((0, 0));
                let size = bytes.len();
                let alt = Some(format!("Screenshot of {url}"));

                // Optionally persist to output_path, then carry the buffer in the
                // return value for `extract_image`. The write happens *before* the
                // buffer moves into the return, so a potentially multi-MB
                // screenshot is never cloned.
                let message = match args.output_path.as_deref() {
                    Some(out) => {
                        let path = resolve_path(out, working_dir);
                        write_bytes_with_dirs(&path, &bytes)?;
                        format!(
                            "captured screenshot ({width}x{height}, PNG, {size}); saved to {path}",
                            size = human_size(size as u64),
                            path = path.display(),
                        )
                    }
                    None => {
                        format!(
                            "captured screenshot ({width}x{height}, PNG, {size})",
                            size = human_size(size as u64),
                        )
                    }
                };

                // Always offer the screenshot inline to the client.
                Ok(RetrieveWebpageReturn {
                    text: message,
                    image: Some(PreparedImage {
                        mime_type: "image/png".to_string(),
                        data: bytes,
                        width,
                        height,
                        alt,
                    }),
                })
            }

            WebpageAction::Pdf => {
                let out = args.output_path.as_deref().ok_or_else(|| {
                    ToolExecError(
                        "pdf action requires output_path so the binary can be saved".to_string(),
                    )
                })?;
                let bytes = tab
                    .print_to_pdf(None)
                    .map_err(|e| ToolExecError(format!("failed to render PDF: {e:#}")))?;
                let path = resolve_path(out, working_dir);
                write_bytes_with_dirs(&path, &bytes)?;
                Ok(RetrieveWebpageReturn {
                    text: format!(
                        "saved PDF ({size}) to {path}",
                        size = human_size(bytes.len() as u64),
                        path = path.display(),
                    ),
                    image: None,
                })
            }
        }
    }
}

/// Pull the returned string out of a CDP `RemoteObject`: the protocol encodes
/// a primitive string result as a JSON value (`value.as_str()`).
fn extract_text(value: &serde_json::Value) -> String {
    value.as_str().map(str::to_owned).unwrap_or_default()
}

/// Convenience: unwrap a `RemoteObject`'s optional `value` and extract text.
fn remote_text(object: &headless_chrome::protocol::cdp::Runtime::RemoteObject) -> String {
    object.value.as_ref().map(extract_text).unwrap_or_default()
}

/// Write `bytes` to `path`, creating parent directories as needed.
fn write_bytes_with_dirs(path: &Path, bytes: &[u8]) -> Result<(), ToolExecError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| ToolExecError(format!("failed to create output dir: {e}")))?;
    }
    std::fs::write(path, bytes)
        .map_err(|e| ToolExecError(format!("failed to write '{}': {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    #[test]
    fn validate_url_accepts_http_https_and_file() {
        for u in [
            "https://example.com",
            "http://example.com/path?q=1",
            "file:///etc/passwd",
            "file:///tmp/foo.html",
            "file://localhost/etc/passwd",
        ] {
            assert!(validate_url(u).is_ok(), "{u} should be accepted");
        }
    }

    #[test]
    fn validate_url_rejects_other_schemes() {
        for u in [
            "javascript:alert(1)",
            "ftp://x",
            "data:text/html,hi",
            "not a url",
        ] {
            let err = validate_url(u).unwrap_err();
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn text_expression_embeds_selector_safely() {
        // A selector containing a quote must be JSON-escaped, not interpolated,
        // so it cannot break out of the string literal.
        let expr = text_expression(Some("div[data-x=\"y\"]"));
        assert!(expr.contains("\\\"y\\\""));
        assert!(expr.contains("innerText"));
    }

    #[test]
    fn text_expression_defaults_to_body() {
        let expr = text_expression(None);
        assert!(expr.contains("document.body"));
        assert!(expr.contains("innerText"));
    }

    #[test]
    fn html_expression_uses_outer_html() {
        let expr = html_expression("#main");
        assert!(expr.contains("#main"));
        assert!(expr.contains("outerHTML"));
    }

    #[test]
    fn describe_invocation_defaults_to_content() {
        let tool = RetrieveWebpage::new();
        let args = RetrieveWebpageArgs {
            url: "https://example.com".to_string(),
            ..RetrieveWebpageArgs::default()
        };
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("content"));
        assert!(desc.contains("https://example.com"));
    }

    #[test]
    fn describe_invocation_includes_selector_and_output() {
        let tool = RetrieveWebpage::new();
        let args = RetrieveWebpageArgs {
            url: "https://example.com".to_string(),
            action: Some(WebpageAction::Screenshot),
            selector: Some("#main".to_string()),
            output_path: Some("shot.png".to_string()),
            ..RetrieveWebpageArgs::default()
        };
        let desc = tool.describe_invocation(&args);
        assert!(desc.contains("screenshot"));
        assert!(desc.contains("#main"));
        assert!(desc.contains("shot.png"));
    }

    #[test]
    fn extract_text_handles_string_and_absent_value() {
        assert_eq!(extract_text(&serde_json::json!("hello")), "hello");
        // Non-string / null values must degrade to an empty string.
        assert_eq!(extract_text(&serde_json::Value::Null), "");
        assert_eq!(extract_text(&serde_json::json!(42)), "");
    }

    #[test]
    fn png_dimensions_reads_ihdr() {
        // A minimal-but-valid-enough PNG header carrying 12x3456 dimensions.
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR chunk length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&12u32.to_be_bytes());
        png.extend_from_slice(&3456u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((12, 3456)));
    }

    #[test]
    fn png_dimensions_rejects_garbage() {
        assert_eq!(png_dimensions(b"not a png"), None);
        assert_eq!(png_dimensions(&[0u8; 32]), None);
    }
}
