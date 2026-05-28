# Third-Party Notices

This file documents third-party browser assets vendored under `ui/vendor/`.

## Vendored JavaScript and CSS

| Asset | Package | Version | License | Notice |
|---|---|---:|---|---|
| `ui/vendor/xterm.js` | @xterm/xterm | 5.5.0 | MIT | Copyright (c) 2014 The xterm.js authors. |
| `ui/vendor/xterm.css` | @xterm/xterm | 5.5.0 | MIT | Copyright (c) 2014 The xterm.js authors. |
| `ui/vendor/xterm-addon-fit.js` | @xterm/addon-fit | 0.10.0 | MIT | Copyright (c) The xterm.js authors. |
| `ui/vendor/marked.min.js` | marked | 18.0.4 | MIT | Copyright (c) 2018-2026, MarkedJS. |
| `ui/vendor/purify.min.js` | DOMPurify | 3.4.7 | Apache-2.0 OR MPL-2.0 | Copyright (c) Cure53 and other contributors. Cadenza uses this asset under Apache-2.0. |

Source URLs and update policy are tracked in `ui/vendor/VERSIONS.md`.

The vendored `@xterm/xterm` bundle is minified and may trigger generic
secret-scanner entropy rules. `ui/vendor/xterm.js` was verified against the
official `@xterm/xterm@5.5.0` jsDelivr artifact by SHA-256 before adding the
repository-level gitleaks allowlist.

## MIT License Text for Vendored MIT Assets

The following MIT license text applies to the vendored MIT assets listed above.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## Apache-2.0 Notice for DOMPurify

DOMPurify is distributed under `Apache-2.0 OR MPL-2.0`. Cadenza uses the
vendored DOMPurify asset under Apache-2.0. A copy of Apache-2.0 is included in
`LICENSE-APACHE`.
