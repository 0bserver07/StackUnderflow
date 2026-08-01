// Loaded as an external file, not inline: the page's own CSP is
// `script-src 'self'`, which refuses inline <script> — including this one, if
// it were inline. Externalising is the fix; loosening the policy with
// 'unsafe-inline' would have quietly given up the property the page is about.
// Record any CSP violation the browser reports. If a future edit ever added a
// fetch, an image from a CDN, or an analytics beacon, `connect-src 'none'` /
// `default-src 'none'` would refuse it and it would land here — which is what
// `rust/demo/smoke.py` asserts on. The privacy claim is checkable, not prose.
window.__cspViolations = [];
document.addEventListener('securitypolicyviolation', (event) => {
  window.__cspViolations.push(`${event.violatedDirective} blocked ${event.blockedURI}`);
});
