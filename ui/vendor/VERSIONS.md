# Vendored JS libraries

| File                 | Package           | Version | License               | Source |
|----------------------|-------------------|---------|-----------------------|--------|
| `xterm.js`           | @xterm/xterm      | 5.5.0   | MIT                   | https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.js |
| `xterm.css`          | @xterm/xterm      | 5.5.0   | MIT                   | https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/css/xterm.css |
| `xterm-addon-fit.js` | @xterm/addon-fit  | 0.10.0  | MIT                   | https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0/lib/addon-fit.js |
| `marked.min.js`      | marked            | 18.0.4  | MIT                   | https://cdn.jsdelivr.net/npm/marked@18.0.4/lib/marked.esm.js |
| `purify.min.js`      | dompurify         | 3.4.7   | Apache-2.0 OR MPL-2.0 | https://cdn.jsdelivr.net/npm/dompurify@3.4.7/dist/purify.min.js |

## Update policy

Pin exact versions. Bump only when there is a security fix or required API change.
Test after every bump: run `cargo tauri dev` and open the triage modal with a markdown proposal.
Keep `THIRD_PARTY_NOTICES.md` in sync when adding, removing, or updating vendored assets.
