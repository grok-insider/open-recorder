# Vendored fonts

These fonts are embedded into `ord-ui` (via `include_bytes!`) so the editor and
clip library render with consistent typography and full symbol coverage offline.

| File | Family | License | Source |
|------|--------|---------|--------|
| `IBMPlexSans-Regular.ttf`     | IBM Plex Sans     | SIL Open Font License 1.1 | https://github.com/IBM/plex |
| `IBMPlexMono-Regular.ttf`     | IBM Plex Mono     | SIL Open Font License 1.1 | https://github.com/IBM/plex |
| `NotoSansSymbols2-Regular.otf`| Noto Sans Symbols 2 | SIL Open Font License 1.1 | https://github.com/notofonts/noto-fonts |

Both projects are licensed under the SIL Open Font License, Version 1.1, which
permits bundling and redistribution with software. Full license text:
https://openfontlicense.org/open-font-license-official-text/

Files were copied verbatim from the nixpkgs `ibm-plex` and `noto-fonts` packages.
