// Theme bootstrap: stamps <html data-theme> before the first paint.
//
// A classic script (not a module), loaded with a plain blocking
// <script src> in <head> ahead of the stylesheets, so it has run before
// anything is painted and a stored light theme never flashes dark. It is a
// file, not an inline script, because the gateway serves the dashboard with
// `Content-Security-Policy: script-src 'self'`, which refuses inline scripts.
//
// A stored choice wins, otherwise the operating system decides. Keep the
// rule and the colours in step with js/lib/theme.js, which takes over once
// the app is running (THEME_KEY, THEME_COLOR).
(function () {
  var root = document.documentElement;
  var theme = 'dark';
  try {
    var stored = localStorage.getItem('sy.theme');
    if (stored === 'light' || stored === 'dark') theme = stored;
    else if (window.matchMedia && matchMedia('(prefers-color-scheme: light)').matches) theme = 'light';
  } catch (e) {
    /* storage blocked: stay on the default */
  }
  root.setAttribute('data-theme', theme);
  var meta = document.querySelector('meta[name="theme-color"]');
  if (meta) meta.setAttribute('content', theme === 'light' ? '#eff1f4' : '#101316');
})();
