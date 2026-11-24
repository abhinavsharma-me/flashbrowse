//! Anti-fingerprinting scripts injected via `Page.addScriptToEvaluateOnNewDocument`.
//!
//! Injected before any page script runs. Scope:
//! - Strip `navigator.webdriver`
//! - Stub `window.chrome` runtime
//! - Normalize `plugins`/`mimeTypes`, `languages`, `platform`, `vendor`,
//!   `hardwareConcurrency`, `deviceMemory`, `maxTouchPoints`, `webdriver`
//! - Stub `permissions.query` for `notifications`
//! - Mask WebGL renderer/vendor strings
//!
//! Honest limits (do not over-market as "undetectable"): this does not
//! hide `canvas`/`AudioContext` fingerprint, screen/UA entropy, or TLS
//! fingerprinting. It defeats naive bot checks, not determined
//! fingerprinting. See README § Limitations.

/// Full stealth payload - passed as `Page.addScriptToEvaluateOnNewDocument { source }`.
pub const STEALTH_SCRIPT: &str = r#"(() => {
  // 1. webdriver: try to remove, fall back to undefined. Each block is
  // isolated so one failure cannot disable the rest (H9/M9).
  try {
    delete navigator.webdriver;
    if (navigator.webdriver === undefined) { /* gone - nothing more */ }
    Object.defineProperty(navigator, 'webdriver', { configurable: true, get: () => undefined });
  } catch (_) {}

  // 2. Mock window.chrome runtime
  try {
    if (!window.chrome) {
      window.chrome = {
        app: { isInstalled: false, InstallState: { DISABLED: 'disabled' }, RunningState: { CANNOT_RUN: 'cannot_run' } },
        runtime: { OnInstalledReason: {}, OnRestartRequiredReason: {}, PlatformArch: {}, PlatformNaclArch: {}, PlatformOs: {}, RequestUpdateCheckStatus: {} },
        loadTimes: () => {},
        csi: () => {}
      };
    }
  } catch (_) {}

  // 3. Normalize plugins / mimeTypes (best-effort PluginArray shape)
  try {
    const plugins = Object.freeze([
      { name: 'Chrome PDF Plugin', filename: 'internal-pdf-viewer', description: 'Portable Document Format' },
      { name: 'Chrome PDF Viewer', filename: 'mhjfbmdgcfjbpisiamfdolgcggjagofn', description: '' }
    ]);
    const mimeTypes = Object.freeze([
      { type: 'application/pdf', suffixes: 'pdf', description: 'Portable Document Format' }
    ]);
    Object.defineProperty(navigator, 'plugins', { configurable: true, get: () => plugins });
    Object.defineProperty(navigator, 'mimeTypes', { configurable: true, get: () => mimeTypes });
  } catch (_) {}

  // 4. Normalize languages / locale / platform / vendor / CPU signals
  try {
    Object.defineProperty(navigator, 'languages', { configurable: true, get: () => ['en-US', 'en'] });
    Object.defineProperty(navigator, 'language', { configurable: true, get: () => 'en-US' });
    Object.defineProperty(navigator, 'platform', { configurable: true, get: () => 'Win32' });
    Object.defineProperty(navigator, 'vendor', { configurable: true, get: () => 'Google Inc.' });
    Object.defineProperty(navigator, 'hardwareConcurrency', { configurable: true, get: () => 8 });
    if ('deviceMemory' in navigator) {
      Object.defineProperty(navigator, 'deviceMemory', { configurable: true, get: () => 8 });
    }
    Object.defineProperty(navigator, 'maxTouchPoints', { configurable: true, get: () => 0 });
  } catch (_) {}

  // 5. Permissions query (bound receiver - fixes Illegal Invocation)
  try {
    const originalQuery = window.navigator.permissions.query.bind(window.navigator.permissions);
    window.navigator.permissions.query = (parameters) => (
      parameters.name === 'notifications' && window.Notification ?
        Promise.resolve({ state: Notification.permission }) :
        originalQuery(parameters)
    );
  } catch (_) {}

  // 6. Normalize WebGL renderer/vendor (best-effort string masking)
  try {
    const getParameter = WebGLRenderingContext.prototype.getParameter;
    WebGLRenderingContext.prototype.getParameter = function(parameter) {
      if (parameter === 37445) return 'Google Inc.';
      if (parameter === 37446) return 'ANGLE (Google, Vulkan 1.3.0 (SwiftShader Device (Subzero) (0.0000000059604645)))';
      return getParameter.call(this, parameter);
    };
  } catch (_) {}

})();
"#;

/// Additional page instrumentation used only by the settle shadow benchmark.
pub const SETTLE_DIAGNOSTICS_SCRIPT: &str = r#"(() => {
  try {
    const state = { count: 0, last_ms: 0 };
    Object.defineProperty(window, '__flashbrowseMutationState', {
      configurable: false,
      enumerable: false,
      value: state
    });
    const observer = new MutationObserver((records) => {
      state.count += records.length;
      state.last_ms = performance.now();
    });
    observer.observe(document, {
      subtree: true,
      childList: true,
      characterData: true,
      attributes: true
    });
  } catch (_) {}
})();
"#;

#[cfg(test)]
mod tests {
    use super::{SETTLE_DIAGNOSTICS_SCRIPT, STEALTH_SCRIPT};

    #[test]
    fn script_contains_key_evasions() {
        for needle in [
            "navigator, 'webdriver'",
            "window.chrome",
            "navigator, 'plugins'",
            "navigator, 'languages'",
            "permissions.query",
            "getParameter",
        ] {
            assert!(
                STEALTH_SCRIPT.contains(needle),
                "stealth script missing: {needle}"
            );
        }
    }

    #[test]
    fn mutation_observer_is_separate_from_default_stealth_script() {
        assert!(!STEALTH_SCRIPT.contains("MutationObserver"));
        assert!(SETTLE_DIAGNOSTICS_SCRIPT.contains("MutationObserver"));
        assert!(SETTLE_DIAGNOSTICS_SCRIPT.contains("__flashbrowseMutationState"));
    }
}
