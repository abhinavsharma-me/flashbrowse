//! Screenshot preparation, visual settling, and capture orchestration.

use std::time::Duration;

use serde_json::Value;

use crate::interceptor::blocked_patterns_for_screenshot;
use crate::resource_policy::ResourcePolicy;

use super::config::{ScreenshotOptions, SCREENSHOT_PREPARE_MS_MAX};
use super::error::CoreError;
use super::BrowserSession;

impl BrowserSession {
    /// Capture PNG screenshot (base64). See [`ScreenshotOptions`].
    ///
    /// In agent-default mode, font and image blocks are lifted for the prepare
    /// window, then restored. [`ResourcePolicy::FullRender`] skips that dance.
    ///
    /// Lifting the blocklist alone is not enough: `Network.setBlockedURLs`
    /// only affects future requests, and Chrome never retries a request that
    /// was already blocked. A blocked `<img>` therefore stays broken forever.
    /// So the prepare window also re-requests the images (see
    /// [`Self::reload_blocked_images`]), which is what actually makes pixels
    /// appear.
    pub async fn screenshot(&self, opts: ScreenshotOptions) -> Result<String, CoreError> {
        let needs_lift = self.resource_policy != ResourcePolicy::FullRender;
        if needs_lift {
            self.driver
                .set_blocklist(blocked_patterns_for_screenshot(self.resource_policy))
                .await?;
        }
        // Re-request images that were blocked during the page load. Without
        // this the capture shows broken-image glyphs, no matter how long we
        // wait for them.
        if needs_lift {
            self.reload_blocked_images().await;
        }
        if opts.prepare_lazy_content {
            self.prepare_lazy_content_for_screenshot(opts.full_page)
                .await;
        }
        self.wait_for_visual_ready(opts.timeout_ms).await;
        let data = self.driver.capture_screenshot(opts.full_page).await?;
        if needs_lift {
            self.driver
                .set_resource_policy(self.resource_policy)
                .await?;
        }
        Ok(data)
    }

    /// Convenience wrapper (viewport, default prepare/timeout).
    pub async fn screenshot_viewport(&self) -> Result<String, CoreError> {
        self.screenshot(ScreenshotOptions::default()).await
    }

    /// Re-request images that the network layer blocked during page load.
    ///
    /// `Network.setBlockedURLs` prevents the request; it does not prevent
    /// the `<img>` element from existing, and Chrome does not retry a blocked
    /// request on its own. Re-assigning the source issues a fresh request,
    /// which now succeeds because the blocklist has been lifted.
    ///
    /// Images that already loaded are left alone (`complete && naturalWidth`),
    /// so this never re-fetches a working image. Inline `data:` URIs are
    /// never blocked, so they are skipped too.
    pub(super) async fn reload_blocked_images(&self) {
        // Bounded: a pathological page with thousands of images must not
        // turn one screenshot into thousands of requests.
        const MAX_RETRY: usize = 200;
        let expr = format!(
            r#"(function(){{
  const imgs = Array.from(document.images || []);
  let n = 0;
  for (const img of imgs) {{
    if (n >= {MAX_RETRY}) break;
    if (img.complete && img.naturalWidth > 0) continue; // already loaded
    const src = img.currentSrc || img.src;
    if (!src || src.indexOf('data:') === 0) continue;   // inline data URI
    if (img.srcset) {{
      // Re-trigger the srcset selection rather than pinning one candidate.
      const s = img.srcset;
      img.srcset = '';
      img.srcset = s;
    }} else {{
      img.src = '';
      img.src = src;
    }}
    n++;
  }}
  return n;
}})()"#
        );
        if let Ok(Value::Number(n)) = self.driver.evaluate(&expr).await {
            let count = n.as_u64().unwrap_or(0);
            if count > 0 {
                tracing::debug!(count, "re-requested blocked images");
            }
        }
    }

    /// Surface lazy-loaded content before a capture.
    ///
    /// The page is scrolled through so that `loading="lazy"` images start
    /// fetching, then the agent's original scroll position is restored. The
    /// restore matters: the agent asked for a picture of what it is currently
    /// looking at, so snapping the viewport back to the top would destroy
    /// that context.
    pub(super) async fn prepare_lazy_content_for_screenshot(&self, full_page: bool) {
        // Remember where the agent was. The value is held here (not on
        // `window`) so nothing is left behind in the page's global scope.
        let saved_y = self.evaluate_scroll_y().await;
        let script = r#"(function(){
  const h = Math.max(document.body.scrollHeight, document.documentElement.scrollHeight);
  const step = Math.max(400, window.innerHeight);
  for (let y = 0; y < h; y += step) { window.scrollTo(0, y); }
  return true;
})()"#;
        let _ = self.driver.evaluate(script).await;
        // Give the lazy loader a tick to start the fetches.
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Restore the agent's viewport.
        let target = if full_page { 0.0 } else { saved_y };
        let _ = self
            .driver
            .evaluate(&format!("window.scrollTo(0, {target}); true"))
            .await;
    }

    pub(super) async fn wait_for_visual_ready(&self, timeout_ms: u64) {
        let budget = timeout_ms.clamp(500, SCREENSHOT_PREPARE_MS_MAX);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(budget);
        let quiet_ms = 350u64;
        loop {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!("screenshot prepare deadline reached - capturing best effort");
                break;
            }
            let net_idle = self.net.idle(quiet_ms);
            let images_ok = self.images_ready().await;
            if net_idle && images_ok {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// True when no image is still pending.
    ///
    /// `img.complete` is the readiness signal: it is true once the image has
    /// loaded or failed. Requiring `naturalWidth > 0` as well would mean a
    /// permanently broken image (blocked, 404, unsupported format) keeps the
    /// wait loop spinning until the deadline - every screenshot of a page with
    /// one bad image would cost the full timeout.
    pub(super) async fn images_ready(&self) -> bool {
        let expr = r#"(function(){
  const imgs = Array.from(document.images || []);
  if (imgs.length === 0) return true;
  return imgs.every(function(img){
    if (!img.src) return true;
    return img.complete;
  });
})()"#;
        match self.driver.evaluate(expr).await {
            Ok(Value::Bool(b)) => b,
            _ => true,
        }
    }
}
