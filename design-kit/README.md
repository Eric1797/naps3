# Design Kit (extracted from PdfCraft)

Reusable visual assets for ScanCraft and future apps. Extracted from
`C:\Users\Eric George\Downloads\pdf craft` (PdfCraft |Modded|, v0.4.0).

```
design-kit/
  icons/   185 Lucide SVGs + LICENSE-lucide.txt (ISC)
  fonts/   Inter / JetBrains Mono / Dancing Script + OFL texts (OFL-1.1)
  ui/      theme.rs, widgets.rs, icons.rs, icon_data.rs, palette.rs
           (PdfCraft egui design system — MIT OR Apache-2.0, see below)
```

Deliberately **excluded**: `docs/brand/` (ArtCraft trademark — forks must
remove it) and `assets/app-icon/` (PdfCraft-specific product icon).

## Design tokens (`ui/theme.rs` → `Tokens::get`)

| Token      | Light     | Dark      |
|------------|-----------|-----------|
| titlebar   | `#E9E9EB` | `#1B1B1E` |
| chrome     | `#FFFFFF` | `#26262A` |
| panel      | `#FFFFFF` | `#26262A` |
| pasteboard | `#F1F1F3` | `#19191C` |
| border     | `#DADADE` | `#3C3C43` |
| text       | `#222226` | `#ECECEF` |
| text_muted | `#5E5E66` | _(see file)_ |
| accent     | `#1B63E0` | `#4B8BF5` |
| accent_soft| `#E3ECFD` | `#243657` |
| selected   | `#E6EEFD` | _(see file)_ |
| radius     | `6`       | `6`       |

`ThemeKind::{Light, Dark}`, `ThemePreference::{System, Light, Dark}` with
`.resolve()`. Applied onto egui `Visuals + Stroke + CornerRadius` (WCAG AA).

## Widget kit (`ui/widgets.rs`)

- `mode_tab` — 13.5pt tab with underline bar when active
- `pill_button` — 28h/14r, primary fills accent
- `ghost_button` — transparent until hovered
- `search_box` — 32h/16r lookalike that opens the ⌘K palette
- `section_title` — 10.5pt semibold uppercase +0.6 spacing
- `toast` — bottom-center, ~3.5s

## Icons (`ui/icons.rs` + `ui/icon_data.rs` + `icons/`)

- Embedded Lucide SVGs, `currentColor → #FFF`, stroke `2 → 1.75`,
  rasterized via `egui_extras` svg at exact px, `tint()` per use.
- Rule: never Unicode glyphs as icons (PhotoCraft lesson).
- Tests assert every catalog/command icon exists.

## Command palette (`ui/palette.rs`)

⌘K fuzzy substring over commands + tool catalogue, top-12 hits.

## Canvas metrics (from PdfCraft `canvas.rs`)

`PT = 96/72`, `GAP 14`, `MARGIN 28`, `SIDE 70`,
`TILE_THRESHOLD 4096 / TILE 1024 / BASE 2048`, `THUMB_W 132`.

## Type

- UI: Inter Regular/Medium/SemiBold, mono: JetBrains Mono, script accent:
  Dancing Script (`fonts/`). CJK/Arabic/Telugu come from the optional
  `craft-fonts` build input (not vendored here).

## Reusing in another app

1. Copy `icons/` + `fonts/` (keep the license files next to them).
2. Copy `ui/theme.rs` + `ui/widgets.rs` as the starting point; rewire
   `Tokens::get` to your app state and rename the accent.
3. Copy `ui/icons.rs` + `ui/icon_data.rs` for the SVG pipeline
   (needs `egui` + `egui_extras/svg`).
4. Copy `ui/palette.rs` once you have a command table to search.

## Licenses

- `icons/` — ISC (Lucide), see `icons/LICENSE-lucide.txt`.
- `fonts/` — SIL Open Font License 1.1, see `OFL-*.txt`.
- `ui/*.rs` — MIT OR Apache-2.0, © ArtCraft Team / PdfCraft
  contributors (portions © storytold/pdfcraft). Keep this notice and the
  file headers when reusing.
- ScanCraft itself is GPL-2.0-or-later; these permissively-licensed
  assets are compatible with that.
