//! The webviews' half of the proxy setting (#465): the core's `http` module owns the setting, this
//! turns it into what wry can take. `crate::http` re-exports both halves.

use std::sync::OnceLock;

/// The proxy setting in the one shape a webview takes: `http://` or `socks5://` with an explicit
/// port. Anything else fails the window build in `tauri-runtime-wry` (`parse_proxy_url`), so it
/// leaves the webviews on the system proxy instead. Credentials do not reach WebView2, which wry
/// only hands a host and port.
///
/// The webviews load the artwork and the sign-in page themselves, so before this the setting never
/// reached them and the images broke behind a proxy that only Limusic knew about (#465).
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn webview_proxy() -> Option<tauri::Url> {
    static URL: OnceLock<Option<tauri::Url>> = OnceLock::new();
    URL.get_or_init(|| {
        let p = encore_core::http::proxy()?;
        let url = webview_url(p);
        if url.is_none() {
            // Scheme only, as in `client`.
            let scheme = p.split_once("://").map_or("(none)", |(s, _)| s);
            tracing::warn!(scheme, "webviews need http:// or socks5:// with a port, going direct");
        }
        url
    })
    .clone()
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn webview_url(proxy: &str) -> Option<tauri::Url> {
    tauri::Url::parse(proxy).ok().filter(|u| {
        matches!(u.scheme(), "http" | "socks5") && u.host_str().is_some() && u.port().is_some()
    })
}

/// Gives a webview window the user's proxy, on Windows. Every webview has to get the same one:
/// wry turns it into a WebView2 browser argument, and WebView2 refuses to create an environment
/// whose options differ from the one already running on the same data folder (ERROR_INVALID_STATE),
/// so a window built without it would fail to open. Linux sets it once on the shared WebKit context
/// instead (`tune_webview`), and macOS would need tauri's `macos-proxy` feature.
pub fn proxied<'a, R: tauri::Runtime, M: tauri::Manager<R>>(
    b: tauri::WebviewWindowBuilder<'a, R, M>,
) -> tauri::WebviewWindowBuilder<'a, R, M> {
    #[cfg(windows)]
    {
        let mut b = b;
        let proxy = webview_proxy();
        if let Some(args) = browser_args(proxy.as_ref()) {
            b = b.additional_browser_args(&args);
        }
        if let Some(url) = proxy {
            b = b.proxy_url(url);
        }
        return b;
    }
    #[allow(unreachable_code)]
    b
}

/// WebView2's documented switch-passing variable, `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` (how
/// WebDriver/msedgedriver opens the DevTools port, and how Microsoft support asks for logging
/// flags), is not applied when the app passes arguments of its own, which wry always does. When the
/// variable is set, pass wry's own defaults plus its switches, identically for every webview.
/// Unset (every normal launch), wry keeps building the arguments itself.
#[cfg(windows)]
fn browser_args(proxy: Option<&tauri::Url>) -> Option<String> {
    let extra = std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").ok()?;
    let extra = extra.trim();
    if extra.is_empty() {
        return None;
    }
    // wry 0.55's defaults (webview2/mod.rs): the mini menu and SmartScreen off, autoplay on (every
    // Tauri webview has autoplay set), and the proxy, which wry only adds to its own defaults.
    let mut args = String::from(
        "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required",
    );
    if let Some(u) = proxy {
        args.push_str(&format!(
            " --proxy-server={}://{}:{}",
            u.scheme(),
            u.host_str().unwrap_or_default(),
            u.port().unwrap_or_default()
        ));
    }
    args.push(' ');
    args.push_str(extra);
    Some(args)
}

#[cfg(test)]
mod tests {
    use super::webview_url;

    #[test]
    fn webview_url_takes_only_what_wry_can_build() {
        assert!(webview_url("http://127.0.0.1:7890").is_some());
        assert!(webview_url("socks5://user:pass@proxy.lan:1080").is_some());
        // `parse_proxy_url` refuses these schemes, which would fail the window build.
        assert!(webview_url("https://proxy.lan:8443").is_none());
        assert!(webview_url("socks5h://proxy.lan:1080").is_none());
        // No port reaches wry as an empty one, and the url crate drops a default :80.
        assert!(webview_url("http://proxy.lan").is_none());
        assert!(webview_url("http://proxy.lan:80").is_none());
        assert!(webview_url("127.0.0.1:7890").is_none());
    }
}

