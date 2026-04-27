# Bundled fonts

These `.ttf` files are loaded into egui at startup (see
`crates/aj-app/src/compose.rs::install_fonts`) so geometric shapes and
emoji codepoints used by the UI render with proper glyphs instead of
the missing-glyph "tofu" rectangle. egui's bundled defaults
(Ubuntu-Light, Hack, NotoEmoji subset) don't cover ▲ ▼ ▶ 👁 etc., which
the brush + layers panels use for buttons.

## Files

| File | Source | License | Used for |
|---|---|---|---|
| `DejaVuSans.ttf` | [dejavu-fonts/dejavu-fonts v2.37](https://github.com/dejavu-fonts/dejavu-fonts/releases/tag/version_2_37) | DejaVu Fonts License (Bitstream Vera derivative — permissive, see `LICENSE.dejavu`) | Proportional fallback. Wide Unicode coverage including geometric-shape codepoints (▲ ▼ ▶ ◀ ● ○ etc.). |
| `NotoEmoji-Regular.ttf` | [googlefonts/noto-emoji v2.034](https://github.com/googlefonts/noto-emoji/tree/v2.034/fonts) | SIL Open Font License 1.1 (`LICENSE.noto-emoji`) | Emoji fallback (monochrome outline, *not* color). egui can't render color emoji glyphs. |

## License

Both licenses are font-specific permissive licenses compatible with the
project's `MIT OR Apache-2.0` code license. They allow embedding the
font data in a binary and redistribution unmodified. Full license text
in the sibling `LICENSE.*` files.

`cargo-deny` does not audit asset files (only Rust crate dependencies),
so these don't affect the workspace `cargo deny check` gate. egui
itself bundles OFL-licensed fonts (Hack, Ubuntu-Light, NotoEmoji subset)
under the same precedent.

## Updating

If a glyph the UI wants is still tofu after a UI change, add either:

1. A larger Unicode-coverage replacement font here, *or*
2. A glyph swap in the calling code to one already covered.

Geometric shapes are typically in `DejaVuSans` already; emoji-style
icons need `NotoEmoji-Regular`. Both lists are coverable via
[unicover.txt](https://github.com/dejavu-fonts/dejavu-fonts/blob/master/unicover.txt)
and the Noto Emoji code-point set respectively.
