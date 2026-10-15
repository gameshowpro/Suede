# Suede typography

The application and documentation use **The Pragmatic Engineer** scheme:
Plus Jakarta Sans for headings, labels, and prose, and Cascadia Code for data,
identifiers, inputs, and code. These are the canonical, unmodified WOFF2 assets.
No font download or conversion runs during builds or at runtime.

| Family | Weights | Styles | Source |
| --- | --- | --- | --- |
| Plus Jakarta Sans | 200–800 | Normal and italic | [Tokotype, commit `18d1cd2`](https://github.com/tokotype/PlusJakartaSans/tree/18d1cd2f7ea10481919d2f05c1f7064b7307fc26/fonts/webfonts) |
| Cascadia Code | 200–700 | Normal and italic | [Microsoft, release `v2407.24`](https://github.com/microsoft/cascadia-code/releases/tag/v2407.24) |

Jakarta's upstream variable filenames were renamed to avoid brackets in URLs.
Cascadia's files come from the release archive's `woff2/` directory. Both
families use the SIL Open Font License; their licenses are retained here and
distributed alongside the fonts. `sources.json` records download locations
and SHA-256 hashes for verification when updating them.

`fonts.css` defines the shared faces and font stacks. The application embeds
the CSS, fonts, and licenses at compile time and serves them under
`/assets/fonts/`. MkDocs runs `scripts/fonts_hook.py` to copy the same package
to the ignored `docs/generated/fonts/` directory. CSS font URLs are relative
so the documentation also works under a URL prefix. Material and the standalone
Scalar page have their default font CDN requests disabled.

Headings use weight 700 and section labels use 600. Data and inline code use
tabular digits, slashed zero, and disabled ligatures so flags and identifiers
remain unambiguous. The raw configuration editor and code blocks enable
programming ligatures. Cascadia Code has the same underlying design as the
Cascadia Mono used in the comparison, with optional programming ligatures.

Normal Jakarta is preloaded on the main pages; the application also preloads
normal Cascadia because data fields are visible immediately. Italics load only
when needed. `font-display: swap` keeps text readable if a font is delayed or
fails; local hosting and preloading reduce the fallback interval but do not
guarantee zero layout shift. Existing light and dark theme colors are preserved.
