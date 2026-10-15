# PdfCraft — Architecture & Design Reference

Source analyzed: `C:\Users\Eric George\Downloads\pdf craft` (PdfCraft |Modded|, v0.4.0, pure-Rust PDF workbench — clean-room Acrobat reimplementation).
This file lives in `C:\Users\Eric George\Downloads\scancraft\design.md` as the design-system reference for ScanCraft.

> PdfCraft is a Cargo workspace (`resolver = "3"`, `edition = "2024"`, Rust `1.90`, `MIT OR Apache-2.0`, `unsafe_code = "forbid"`).
> Members: `crates/*`, `apps/*`, `xtask`. Default: `crates/* + apps/pdfcraft + apps/pdfcraft-cli + xtask`. Excluded: `vendor/`.

---

## 1. Architecture at a glance

```
L0 standalone (no PDF deps):  geom ─┐
                              filters┤──> cos only
                              crypt ─┘
L1 standalone:                cos (tolerant parser / xref / COW edits / incremental+full writer / decrypt)
L2 typed views:               content, model ─ (cos) ; fonts (cos + skrifa)
L3 domain:                    render (hayro+lopdf bootstrap) | annot | forms | js (boa) | xfa
L4 features:                  edit | organize | redact (+edit) | xfdf | sign | ocr | create |
                              export | optimize | preflight | compare | measure | a11y | print
L6 facade:                    engine (sessions / docs / history / catalog — ONLY frontend entry point)
L7 presentation/OS:           platform ─┐ (no inter-dep, shared OS services)
                              ui-egui (engine+render+platform+egui/eframe) ─┤
                              automation (engine+render+organize+platform, 123 headless tools + MCP)
L8 binaries (exempt):         apps/pdfcraft (desktop) | apps/pdfcraft-cli | apps/pdfcraft-web (wasm) | xtask
```

Rules (enforced by `cargo xtask layers`, `xtask/src/layers.rs`):

- Edges only **downward**. Nothing below L7 depends on `egui / winit / eframe / rfd`.
- Same-layer sideways allowed in a few cases (e.g. `redact -> edit`, `a11y -> preflight`).
- `cos / filters / crypt` are standalone.
- `render` public API (`PageRenderer / RenderPool / inspect / text`) is stable so the M1–M2 swap (own `cos/model/DisplayList` renderer replacing `hayro+lopdf`) stays internal.
- Planned-but-missing crates listed in `xtask/src/layers.rs TABLE`: `arlington, color, ops, text, viewport, tools, ui-common, testkit, oracle, ml, ai, media, search`.

Vendored patches (`[patch.crates-io]` → `vendor/`): `hayro, hayro-syntax, hayro-interpret, hayro-jbig2, lopdf` — all meant to go upstream.

---

## 2. Complete crate inventory (30 + xtask)

| Crate `pdfcraft-*` | Layer | What it does | `src/` files |
|---|---|---|---|
| `geom` | L0 | Geometry primitives: `PageRect` (pt, y-up), `Rotation` | `lib.rs` (69 lines) |
| `filters` | L0 | Every stream filter: Flate, LZW, ASCII85/Hex, RunLength, predictors; encode+decode, property-tested | `lib, flate, lzw, predictor, ascii85, asciihex, runlength` |
| `crypt` | L0 | Standard security handler R2–R6: RC4, AES-128/256, SASLprep Unicode passwords, permissions, crypt filters | `lib, aes_cbc, rc4, tests` |
| `cos` | L1 | PDF object layer: lazy tolerant parse, repair, copy-on-write edits, incremental + full writing, decrypt-on-read | `lib, document, object, parser, security, writer` |
| `content` | L2 | Content streams: operators + spans, inline images, serializer, `Matrix` | `lib, tests` |
| `model` | L2 | Typed views: page tree + `INHERITABLE`, `Page::crop/rotation/view_matrix` | `lib.rs` (161 lines) |
| `fonts` | L2 | Metrics/encodings for generated appearances: WinAnsi, Helvetica widths/wrap, `CRAFT_FONTS`, `script_outline` | `lib, craft, encodings, pdf, script` |
| `render` | L3 | **Bootstrap renderer**: `hayro` raster + `lopdf` inspect + reading-order text extract. API stable for M2 DisplayList swap | `lib (35), raster, inspect, text` |
| `annot` | L3 | Comments: builders + appearance streams (notes, text-markup, shapes, ink, text-boxes), replies/status/edits, links, ISO 12.5 | `lib, appearance, links, tests` |
| `forms` | L3 | AcroForm: field model, filling with regenerated appearances, Clear form, authoring, AF scripts, detect | `lib, actions, af, appearance, author, detect, scripting, tests` |
| `js` | L3 | Sandboxed Acrobat JS on `boa_engine 0.22 annex-b`: event/getField/app/util/console; FormCalc + XFA bindings | `lib, formcalc, xfa, tests` |
| `xfa` | L3 | XFA → PDF: XDP packets → pages + AcroForm fields, layout engine | `lib, data, fixtures, layout, model, packets, parse, pdf, script, text, tests` |
| `edit` | L4 | Page content edits: header/footer, watermark/background, Bates numbering, added text/image, flatten | `lib, added, flatten, images, text, tests` |
| `organize` | L4 | Pages/structure: rotate/delete/move/insert, combine/extract/split, bookmarks/outline, page labels, doc info, dedupe, InitialView, import/prune/pdfx | `lib, boxes, dedupe, import, labels, outline, pdfx, prune, view, tests` |
| `redact` | L4 | Redaction: marks/apply (glyph/image/vector/XObject/field)/verify + sanitize | `lib, image, interp, patterns, sanitize, tags, tests` |
| `xfdf` | L4 | Data exchange: XFDF/FDF/XML/CSV/text import/export | `lib, tests` |
| `sign` | L4 | PAdES signatures: DER/X509/CMS/PKCS12, B-B sign/validate, Keychain/CNG, DSS/timestamp stubs, revocation | `lib, cms, der, dss, keychain, keys, pdf, pkcs12, revocation, timestamp, windows, x509` |
| `ocr` | L4 | Scan & OCR on `ocrs 0.13 / rten 0.26`: searchable-image invisible text layer (Latin-focused) | `lib.rs` |
| `create` | L4 | Create from blank/images/text/docx; `source_kind`, `convert_to_pdf`, office extract | `lib, extract, office, tests` |
| `export` | L4 | Export paragraphs+images → HTML/docx/RTF | `lib, zip` |
| `optimize` | L4 | Optimizer: bicubic downsample, recompress, discards, resource merge, audit/links | `lib, audit, images, links` |
| `preflight` | L4 | Standards: PDF/A verify + convert to A-2b/3b (ICC/XMP) | `lib, icc, xmp, tests` |
| `compare` | L4 | Compare: word-level diff grouped to changes + page placements | `lib.rs` |
| `measure` | L4 | Calibrated 2D measure: viewports, Line/Poly/PolygonDimension, snap, CSV | `lib, snap, tests` |
| `a11y` | L4 | Accessibility Checker: 32 rules, report + fixes | `lib, alt, content, report, structure, tests` |
| `print` | L4 | Printing: `impose` sheets (Size/Multiple/Booklet/Poster/CutStack) + `spool` (CUPS/Win) + ranges | `lib (626), range, spool, tests` |
| `engine` | L6 | **Façade every frontend uses**: sessions, docs, undo history (COW+incremental), `catalog.rs` tool catalogue, `commands.rs`, actions/links/js/ocr/optimizer/export/compare/xfa/signature_image | `lib (3168), commands, catalog, actions, links, js, ocr, optimizer, export, compare, xfa, signature_image` |
| `platform` | L7 | OS services shared by `ui-egui`+`automation`: atomic staging writes, `portable.txt`, settings/log paths, single-instance helpers | `lib.rs, staging.rs` |
| `ui-egui` | L7 | **egui shell** (presentation only): chrome/canvas/panels/dialogs/theme/i18n/control-channel. 60 files, docs/render/catalog live in `engine` | see §3 |
| `automation` | L7 | Headless tool table (123 tools) + opt-in MCP stdio server + `pdfcraft-cli run`; `a11y/comments/content/forms/links/mcp/measure/printing/redact/signing/tools` | `lib + 11 modules` |
| `xtask` | L8 | Gates: `layers / parity / assets / fuzz / demo-pdf / screenshots` | `src/layers.rs` (+ tasks) |

---

## 3. Design crates & elements (visual system)

All visual design lives in **L7** (`crates/ui-egui`, `crates/platform`, `apps/*`). Engine crates never touch egui.

### 3.1 `crates/ui-egui` — the design system (60 files in `src/`)

Core shell:

| File | Design responsibility |
|---|---|
| `lib.rs` (1829 lines) | App entry, L7 doc (`plan/acrobat/02-ui-ux.md §1`), state, update loop |
| `theme.rs` (314) | **Design tokens** — single source of truth, every widget reads `Tokens::get` |
| `widgets.rs` (328) | Custom widget kit: `mode_tab`, `pill_button`, `ghost_button`, `search_box`, `section_title`, `toast`, `menu_item` |
| `icons.rs` (80) + `icon_data.rs` (189) | Vector icon system (~180 Lucide SVGs, see §3.3) |
| `chrome.rs` (619) | Window chrome: `tab_strip` 38px, titlebar, mode bar, right rail |
| `canvas.rs` (3072) | Document canvas: tiling, zoom/fit/layouts, overlays (see §3.4) |
| `panels.rs` (1065) | Left 272px tool browser + right panels (Comments/Bookmarks/Pages/Fields/Layers/Attachments) |
| `palette.rs` (172) | ⌘K command palette: fuzzy substring over commands+tools, 12 hits |
| `dialogs.rs` (~77 kB) | All dialogs (split/combine/properties/print/protect/…) |
| `commands.rs` | Command registry binding engine commands to UI |
| `control.rs` (~34 kB) | Opt-in agent control channel (`--control`, loopback + token) |
| `home.rs` | Welcome screen + `All tools` catalogue (generated from `engine::catalog`) |
| `bidi.rs` | RTL display-order for tabs/recents/props (egui shapes but does not reorder) |
| `system_fonts.rs` | Last-resort installed-face fallback (never embedded), 32 MB cap, `PDFCRAFT_SYSTEM_FONTS=0` to disable |
| `autoscroll.rs`, `wheel_pager.rs`, `zoom_snap.rs`, `pageboxes.rs`, `drag_pointer.rs` | Interaction details: smooth scroll/paging bursts, zoom anchoring, page hit-boxes |
| `pickers.rs`, `files.rs`, `recovery.rs`, `portable.rs`, `single_instance.rs`, `prepare.rs`, `editing.rs` | File UX: native pickers (via `rfd` + `pollster` off UI thread), autosave/recovery, portable mode |
| `*_ui.rs` (25 files) | Per-tool views: `combine_ui (78kB)`, `sign_ui (53kB)`, `print_ui (567)`, `comments`, `comments_panel`, `comment_props`, `forms_ui`, `content_ui`, `edit_text_ui`, `js_ui`, `a11y_ui`, `actions_ui`, `compare_ui`, `create_ui`, `create_multiple_ui`, `crop`, `link_ui`, `marks_ui`, `measure_ui`, `ocr_ui`, `optimize_ui`, `export_ui`, `protect`, `redact_ui`, `search_ui`, `signature_drag`, `stamps_ui`, `standards_ui`, `fill_sign`, `folders_ui` |
| `updates.rs` | Help → Check updates (`ureq`) |
| `credits.rs` + `build.rs` | Generated contributor credits (`contributors.json` + `people.toml` → `credits.rs`) |
| `i18n/` | Localization: `mod.rs`, `catalog.rs`, TSV per locale (see §3.5) |
| `examples/shot.rs`, `tests/` | Screenshot harness (`cargo xtask screenshots`), kittest UI tests (`egui_kittest`) |

### 3.2 Design tokens — `theme.rs` (`Tokens::get`)

- `ThemeKind::Light | Dark`, `ThemePreference::System | Light | Dark` with `.resolve(system, fallback)`. Auto follows OS.
- `apply(ctx)`: sets egui `Visuals + Stroke + CornerRadius`, fonts (`Inter + JetBrainsMono + DancingScript + craft CJK/Arab/Telugu`), popup sync. WCAG AA contrast.
- Light: `titlebar #E9E9EB`, `chrome/panel #FFFFFF`, `pasteboard #F1F1F3`, `border #DADADE`, `divider #E8E8EB`, `text #222226`, `muted #5E5E66`, `accent #1B63E0`, `accent_soft #E3ECFD`, `selected #E6EEFD`, `radius 6`.
- Dark: `titlebar #1B1B1E`, `chrome #26262A`, `pasteboard #19191C`, `border #3C3C43`, `text #ECECEF`, `accent #4B8BF5`, `accent_soft #243657`, `radius 6`.
- Full struct: `titlebar, chrome, panel, pasteboard, card, border, divider, text, text_muted, text_faint, icon, hover, pressed, selected, accent, accent_text, accent_soft, field, badge_new, page_shadow, radius, kind`.
- Typography helpers: `theme::regular / medium / semibold(size)`; `section_title` = 10.5pt semibold uppercase +0.6 letter-spacing.

### 3.3 Icons — `icons.rs` + `icon_data.rs` + `assets/icons/`

- ~180 **Lucide ISC** SVGs embedded with `include_bytes!`.
- Pipeline: `white_icons()` recolors `currentColor → #FFFFFF`, `stroke-width 2 → 1.75` → `egui_extras svg` rasterized at exact on-screen px → `tint()` per use.
- API: `exists(name)`, `image(name,size,tint)`, `paint(ui,rect,name,size,tint)`, `button(ui,name,box_size,selected,tooltip)` (transparent until hover; `selected` = `accent_soft` fill + `accent_text` tint).
- Rule (PhotoCraft lesson): **never Unicode glyphs as icons**.
- Tests enforce every `TOOL_GROUPS` + `COMMANDS` icon exists (`icons.rs` `#[cfg(test)]`).
- License file: `assets/icons/LICENSE-lucide.txt`.

### 3.4 Canvas & layout — `canvas.rs` + `chrome.rs` + `panels.rs`

- Constants: `PT = 96/72`, `GAP 14`, `MARGIN 28`, `SIDE 70`, `TILE_THRESHOLD 4096 / TILE 1024 / BASE 2048`, `THUMB_W 132`.
- Whole-page rasters from `RenderPool` (hayro), stale-stretch while re-rendering, tiled path for huge pages.
- Overlays: link rects/hover, form-field highlight, comment/measure/redact/mark layers.
- `Fit` modes + `PageLayout`: Continuous / Single / TwoUp (+ cover), view rotation, Read mode, deep-zoom tiling.
- Chrome: 38px `tab_strip` (`titlebar` fill, macOS 80px traffic-light inset, drag/double-click, `house` home, dirty-dot for unsaved, scrollable tabs), mode bar (`AllTools / Read / Edit / Convert`), right rail page nav.
- Panels: left `272px` (`AllTools` generated from `engine::catalog` + hue dot, collapsed `14px`); right: Comments (threaded) / Bookmarks (nested outline + page labels `Cover, i, ii…`) / Pages / Fields / Layers / Attachments.

### 3.5 i18n — `i18n/`

- `tl! / tl_ctx! / fmt` macros, per-thread language, `LANGUAGES` list.
- Format: TSV `context<TAB>en<TAB>translation`, `@id / @plural` support, strict validation.
- Coverage: `ja / es / fr / ru / zh-hans / zh-hant / te` full (~1800–1984 strings) + `cs / pt-br` menus-only. Command search accepts translated label + English label + stable command id.
- Docs: `docs/localization.md`. Menu path: **Edit → Preferences…** (⌘,/Ctrl-,), Auto = system language.

### 3.6 `crates/platform` + `apps/*` (windowing / OS design)

- `platform/src/lib.rs + staging.rs`: atomic writes (temp + swap), `portable.txt` → `PdfCraftData/` next to exe, per-user settings/log paths, single-instance helpers.
- `apps/pdfcraft`: `main.rs + logging.rs (env_logger → stderr + logs/pdfcraft.log) + updates.rs + apple_events.rs + build.rs (icon + VERSIONINFO)`. Deps: `eframe[default_fonts, wgpu, accesskit, wayland, x11, persistence, links]`, `ureq[rustls] + rustls-native-certs`, macOS `fmv-macos-events`.
- `apps/pdfcraft-web`: `main.rs + index.html + Trunk.toml`, `eframe[default_fonts, glow, persistence, accesskit]`, `publish = false`.
- `apps/pdfcraft-cli`: `inspect / render / run / tools / mcp / ui` subcommands + `tests/` (edit, mcp_compact, render, run_root, split, stderr, stdout, text).

### 3.7 PDF-side "design" crates (page appearance, not app chrome)

| Crate | Design-relevant work |
|---|---|
| `render` | Raster (`raster.rs`: `RenderConfig/Request/Tile/RenderedPage`, `MAX_PIXELS/SIDE`, `device_pixels/effective_scale`), inspection (`inspect.rs`: outline/annots/fields/layers/attachments/metadata), reading-order text (`text.rs`: `PageText/TextGlyph`) |
| `fonts` | Appearance typography: WinAnsi encodings, Helvetica widths/wrap, `CRAFT_FONTS_DIR` (Japanese/CJK/Arabic/Telugu, release-pinned), `script_outline` for world scripts |
| `geom` / `model` / `content` | `PageRect/Rotation`, page-tree/inheritable boxes, `crop/rotation/view_matrix`, content `Matrix`/operators/spans/serializer |
| `annot` | Comment appearance streams (highlight/shapes/ink/text-box/AP dicts) |
| `forms` | Field appearance regeneration on fill |
| `print` (`lib/range/spool`) | Print styling/imposition: Size / Multiple (n-up + CutStack) / Booklet / Poster, papers/orientation/odd-even, comments&forms stamp, live sheet preview → print-ready PDF → CUPS/Win spool |
| `edit` | Added-page design: header/footer, watermark/background, Bates |
| `engine` | `catalog.rs` (`TOOL_GROUPS`: every tool + icon + section — drives `home.rs` + `panels.rs` + icon tests), `commands.rs` (`COMMANDS`: drives palette + icon tests) |

### 3.8 Assets & brand

- `assets/fonts/`: `Inter-Regular/Medium/SemiBold, JetBrainsMono-Regular, DancingScript` (all OFL + `OFL-*.txt`). CJK/Arabic/Telugu/Japanese via optional `CRAFT_FONTS_DIR=../craft-fonts` (separate repo, SIL OFL).
- `assets/app-icon/`: `pdfcraft.svg / pdfcraft-small.svg / pdfcraft.icns / pdfcraft.ico / pdfcraft-1024.png + hicolor/{16..512,scalable}` + `README.md`.
- `assets/demo/`: `showcase.html / csv` (source for `cargo xtask demo-pdf` showcase PDF).
- `assets/icons/`: Lucide set + license.
- `docs/brand/`: `artcraft-logo{,-white}.svg/png, artcraft-mark{,-black}.svg/png` + `LICENSE-brand.txt` — **trademark, not open source**; unmodified use only inside PdfCraft; forks must remove.
- `docs/images/` (13 png): `viewer, scripts, twoup, dark, find, bookmarks, organize, split, properties, forms, layers, palette, tools` — regenerated by `cargo xtask screenshots`.
- Attribution: `ATTRIBUTION.md + ATTRIBUTION.toml + NOTICE`; audit via `cargo xtask assets` (hash/license/Adobe-string check).

---

## 4. External UI/design dependencies

Workspace-pinned (`Cargo.toml [workspace.dependencies]` + `Cargo.lock`):

- `egui 0.36.2`, `eframe 0.36.2` (desktop `wgpu + accesskit + wayland + x11 + persistence + links`; web `glow`), `egui_extras 0.36.2 [svg]`, `egui_kittest 0.36.2 [eframe, wgpu]`, `rfd 0.17.2` (native dialogs), `winit 0.30.13`, `wgpu 30.0.1`, `accesskit 0.24.1` (a11y tree → also drives `--control inspect`), `arboard 3.6.1 [image-data]` (clipboard images), `unicode-bidi 0.3.18` (RTL), `skrifa 0.44` (font fallback check + interface glyph coverage test), `kurbo 0.13.1`, `png 0.18`, `image 0.25`, `base64 0.22`, `pollster 1.0` (pickers off UI thread), `sys-locale 0.3` (Win display lang), `objc2 0.6 + app-kit/foundation` (mac drag position), `wasm-bindgen / js-sys / web-sys` (web), `vello_cpu 0.0.8/0.1.0` (via hayro), `hayro 0.7.1 + hayro-syntax 0.7.2 + hayro-interpret` (patched `vendor/`), `lopdf 0.45` (patched), `boa_engine 0.22` (JS), `ocrs 0.13.1 / rten 0.26` (OCR).
- Env knobs (`apps/pdfcraft/src/logging.rs`, `docs/development.md`): `RUST_LOG, WGPU_POWER_PREF/BACKEND, CRAFT_FONTS_DIR, PDFCRAFT_SYSTEM_FONTS, PDFCRAFT_*` paths.

---

## 5. Design principles & guardrails

From `AGENTS.md` (wins over `CLAUDE.md`), `CLAUDE.md`, `ROADMAP.md`:

- **Engine-first**: parsing/rendering/editing in reusable library crates; UI is one swappable layer. Frontends only talk to `pdfcraft-engine`.
- **Headless-everything + opt-in agent control**: all 123 engine tools reachable via `pdfcraft-cli run` (JSON-Schema table), MCP stdio server (`pdfcraft-cli mcp [--compact] [--root]`), Rust API (`pdfcraft_automation::Automation::call`); app driving via `pdfcraft --control /tmp/pc.json` (loopback + token). Never starts servers / opens ports on its own. `--root` sandboxes file access.
- **Never-crash**: `deny unwrap/expect/panic/unimplemented/todo/unreachable`, no `unsafe`, `get()` + checked math + caps + depth limits, `catch_unwind` net, regression test per fuzz crash. Corpus: 983 pdf.js files → 963 open+render, 958 edit-save round-trips, 0 crashes.
- **Clean-room**: behavior from ISO 32000 + black-box observation only. Asset policy (§1): only `ISC/MIT/BSD/Apache/OFL/Ubuntu/Bitstream/CC0/BY/BY-SA/PD/original` + ArtCraft trademark exception; no Adobe assets incl. SourceHan/NotoCJK; every file in `ATTRIBUTION.toml`.
- **Fidelity model**: graph is model, incremental save preserves original bytes (atomic temp+swap, qpdf-verified), deep undo (`Undo Rotate pages`, works after save), encrypted stays encrypted.
- **Quality gates**: `fmt + clippy -D warnings`, 200+ unit/property/UI tests, `xtask layers` + wasm build check + asset-license audit; output verified with hayro/qpdf/poppler.
- **Status (Oct 2026, ROADMAP)**: P0 222/251 (88%), All 411/806 (51%), ~30–35% effort. Next in order: own renderer → hardening + Acrobat fidelity harness → existing-text editing → Pro workflows (sign/OCR/Office/preflight/XFA) → 1.0 polish. No `docs/architecture.md` in repo — machine truth is `parity/acrobat-features.toml` (806 features) + `xtask parity/layers/assets`; `plan/` is gitignored.

---

## 6. How to reuse this for ScanCraft

- Copy the **layering**: `geom/filters/crypt → cos → content/model/fonts → features → engine façade → ui-egui/platform/automation → apps`. Enforce with an `xtask layers` table.
- Copy the **token pattern**: one `Tokens::get(ctx)` struct (`titlebar/chrome/panel/pasteboard/card/border/text/accent/radius…`) + `theme.rs`-style `apply()`; every widget reads tokens, no hardcoded colors.
- Copy the **icon pipeline**: embedded Lucide SVGs → recolor-once → raster at exact px → `tint()` per use + tests that every catalog/command icon exists.
- Copy the **catalog-driven UI**: single `TOOL_GROUPS`/`COMMANDS` table in `engine` drives home screen, side panels, ⌘K palette, and icon tests.
- Keep PDF/scan engine egui-free; keep all `egui/eframe/rfd/winit` imports in L7 + apps only.

*Generated 2026-10-15 from `C:\Users\Eric George\Downloads\pdf craft` (`Cargo.toml`, `crates/*/Cargo.toml + src/`, `apps/*/Cargo.toml + src/`, `assets/`, `docs/`, `README.md`, `AGENTS.md`, `CLAUDE.md`, `ROADMAP.md`).*
