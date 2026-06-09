# Vendored fonts

This font is embedded into `ord-hud` (via `include_bytes!`, behind the
`layershell` feature) so the HUD toast notifications render with consistent
typography matching the rest of open-recorder, offline and without system fonts.

| File | Family | License | Source |
|------|--------|---------|--------|
| `IBMPlexSans-Regular.ttf` | IBM Plex Sans | SIL Open Font License 1.1 | https://github.com/IBM/plex |

IBM Plex is licensed under the SIL Open Font License, Version 1.1, which permits
bundling and redistribution with software. Full license text:
https://openfontlicense.org/open-font-license-official-text/

Copied verbatim from the nixpkgs `ibm-plex` package (also used by `ord-ui`).
