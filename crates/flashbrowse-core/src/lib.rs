//! `flashbrowse-core`: compression, action graph, stealth, navigation logic.
//!
//! ```text
//! BrowserSession ──▶ stealth + interceptor ──▶ Page.navigate
//!       └──▶ Accessibility.getFullAXTree ──▶ tree::compress ──▶ tree::render
//!       └──▶ actions::{click, fill, scroll, press_key} ──▶ rebuild_tree
//! ```

pub mod actions;
pub mod driver;
pub mod engine;
pub mod interceptor;
pub mod markdown;
pub mod observability;
pub mod paths;
pub mod resource_policy;
pub mod session;
pub mod stealth;
pub mod tree;
pub mod url_norm;

pub use engine::BrowserEngine;
pub use resource_policy::ResourcePolicy;
pub use session::{
    BrowserSession, CoreError, DetachedSession, DialogInfo, DialogPolicy, ScreenshotOptions,
    SessionConfig, SCREENSHOT_PREPARE_MS_DEFAULT, SCREENSHOT_PREPARE_MS_MAX,
};
pub use tree::{
    compress_with_options, render, render_nested, ActionMap, CompressOptions, SemanticNode,
};
pub use url_norm::{normalize_url, UrlError};
