# Bundled fonts

All fonts are compiled into the crate with `include_bytes!`; system fonts are never read, so output is identical on any host.

| File | Family | Used for | Source | Licence |
|---|---|---|---|---|
| `fonts/Inter.ttf`, `fonts/Inter-Italic.ttf` | Inter 4.1 (variable, wght 100 to 900) | `sans-serif`, `system-ui`, `ui-sans-serif`, `ui-rounded`, `cursive`, `fantasy`, `math`, `fangsong`, unstyled text, `font-family: Inter` | https://github.com/rsms/inter/releases/tag/v4.1 | SIL Open Font License 1.1, `fonts/Inter-OFL.txt` |
| `fonts/DejaVuSerif.ttf`, `fonts/DejaVuSerif-Bold.ttf` | DejaVu Serif 2.37 | `serif`, `ui-serif` | Debian `fonts-dejavu-core` | Bitstream Vera / Arev fonts licence (permissive, public-domain changes), `fonts/DejaVu-LICENSE.txt` |
| `fonts/JetBrainsMono.ttf`, `fonts/JetBrainsMono-Italic.ttf` | JetBrains Mono 2.304 (variable, wght 100 to 800) | `monospace`, `ui-monospace` | https://github.com/JetBrains/JetBrainsMono/releases/tag/v2.304 | SIL Open Font License 1.1, `fonts/JetBrainsMono-OFL.txt` |
| `fonts/TwemojiMozilla.ttf` | Twemoji Mozilla 0.7.0 (COLRv0) | `emoji` generic; emoji clusters fall back to it | https://github.com/mozilla/twemoji-colr/releases/tag/v0.7.0 | Font code Apache-2.0; glyph artwork by Twitter, CC-BY 4.0, `fonts/Twemoji-LICENSE.md` |

Blitz's own `moz-bullet-font` (list markers) is registered too. Total font payload is about 4.5 MB.

Attribution required by CC-BY 4.0 for the emoji artwork: "Emoji graphics by Twitter, Inc and other contributors, licensed under CC-BY 4.0".
