# Suede visual style guide

This is a front-end styling reference for reproducing Suede's visual language in another project. It intentionally does not describe application architecture, data flow, or back-end behavior.

## Overall character

Suede is a compact, practical operator dashboard. It uses quiet neutral surfaces, thin borders, modest rounding, restrained shadows, and one blue accent. Information density is fairly high, but consistent spacing and a clear sans/monospace split keep it readable. Light and dark themes follow the user's operating-system preference.

## Fonts

- **Interface and prose:** Plus Jakarta Sans, variable weight 200–800. Fallback to `system-ui`, `-apple-system`, `"Segoe UI"`, then `sans-serif`.
- **Data and code:** Cascadia Code, variable weight 200–700. Fallback to `ui-monospace`, `Consolas`, then `monospace`.
- Use locally hosted WOFF2 files, normal and italic, with `font-display: swap`.
- Use Plus Jakarta Sans for headings, navigation, labels, buttons, and explanatory text. Use Cascadia Code for values, identifiers, timestamps, form inputs, technical labels, and code.
- In data text, enable tabular numerals and slashed zero, and disable ligatures: `font-variant-numeric: tabular-nums slashed-zero; font-variant-ligatures: none`.
- Re-enable common/contextual ligatures in code blocks and code editors.

### Type scale

| Role | Size / line height | Weight and treatment |
| --- | --- | --- |
| Body | `15px / 1.55` | 400 |
| Page title | `20px / 1.2` | 700, `-0.02em` tracking |
| Card heading | `15px` | 700 |
| Section eyebrow | `13px` | 600, uppercase, `0.05em` tracking, muted |
| Table/body detail | `14px` | 400 |
| Data and form values | `13px` | Monospace |
| Labels and captions | `12–13px` | 500–600, muted |
| Table headers | `12px` | 600, uppercase, `0.04em` tracking, muted |
| Badges/counts | `11–12px` | 600–700 |

Documentation pages can use a more generous scale: responsive `2–3rem` hero titles with `1.1` line height, while retaining the same families, bold headings, and slightly tight heading tracking.

## Color tokens

| Token | Light | Dark | Purpose |
| --- | --- | --- | --- |
| Page background | `#f5f6f8` | `#13151a` | Canvas |
| Panel | `#ffffff` | `#1b1e24` | Cards, header, dialogs |
| Sunken | `#f0f2f5` | `#15171c` | Inputs, inset areas, expanded rows |
| Text | `#16191d` | `#e8eaed` | Primary content |
| Muted text | `#667085` | `#98a2b3` | Secondary content and labels |
| Border | `#e2e6ea` | `#2a2f38` | Dividers and outlines |
| Accent | `#3b5bdb` | `#7d92f0` | Primary actions, links, focus |
| Success | `#2f9e44` | `#51cf66` | Healthy state |
| Warning | `#b8860b` | `#f0b849` | Caution state |
| Error | `#c92a2a` | `#ff6b6b` | Failure/destructive state |

Keep most of the interface neutral. Reserve color for actions, status, selection, and charts. Never rely on color alone: pair it with a label, icon, border style, or other state marker.

## Surfaces, shape, and depth

- Page cards use the panel color, a `1px` border, `10px` radius, and `16px` internal padding.
- Use a very light card shadow in light mode: `0 1px 3px rgba(0,0,0,.07)`. Remove routine card shadows in dark mode.
- Inputs and ordinary buttons use a `7px` radius. Nested cards, fieldsets, and previews use `8–9px`. Dialogs use `12px`.
- Pills and switches use a fully rounded `999px` radius.
- Dialogs may use deeper elevation, around `0 14px 44px rgba(0,0,0,.3)`, over a `rgba(0,0,0,.45)` backdrop.

## Spacing and layout

- Use a compact spacing rhythm based mainly on `4`, `8`, `12`, `16`, and `20px`.
- Main content is centered, with `20px` side padding and an approximately `940px` maximum width.
- Cards are separated by `16px`. Card grids switch from one column to two only when enough room exists for roughly two 900px work areas; do not squeeze dense controls into narrow columns.
- Form fields use responsive CSS grids: minimum field width around `160px`, or `260px` for wide fields, with `12px` gaps.
- Galleries use responsive columns with a minimum width around `190px` and `14px` gaps.
- Let rows and action groups wrap on smaller screens. Wide tables should scroll horizontally inside their own container.
- Use container queries for layouts that depend on card width, and ordinary media queries for page-level changes.

## Common components

- **Sticky header:** panel background, `1px` bottom border, minimum `56px` height, `20px` horizontal padding, and wrapped actions aligned to the right.
- **Navigation tabs:** transparent, muted text, `7px 14px` padding, and `8px` radius. The selected tab gains a panel background, border, and primary text color.
- **Primary button:** accent fill, white text, `7px 14px` padding, `7px` radius, weight 500. Ghost and destructive variants use transparent backgrounds with accent or error text.
- **Pills:** `12px`, weight 600, `3px 10px` padding, thin border, fully rounded. Status pills color both text and border.
- **Inputs:** sunken background, `1px` border, `7px 10px` padding, and `7px` radius. Labels sit above at `12px`, weight 500, in muted text. Technical input values use the monospace family at `13px`.
- **Tables:** `14px` body text, collapsed borders, and `8px 10px` cells. Headers use the small uppercase style described above.
- **Alerts:** panel surface with a standard border and a `4px` colored left edge. Use a square left side and `9px` rounding on the right. Include a written status/title so the edge color is supplementary.
- **Cards with imagery:** `9px` radius and clipped overflow; image area is approximately `108px` high with `object-fit: cover`, followed by a compact `9px 11px` body.
- **Toast:** centered near the bottom, using primary text color as its background and page color as its text, with `10px 18px` padding and `8px` radius.

## Interaction and accessibility

- Use a visible `2px` accent-colored focus outline with `1–2px` offset on all interactive controls.
- Hover states should usually shift muted text to primary text and add a sunken background or stronger border, rather than introducing a new color.
- Disabled controls use approximately `0.5` opacity and a `not-allowed` cursor.
- Keep motion short and functional, generally `125–200ms`. Use motion for hover/focus transitions, status progress, and spinners—not decoration.
- Respect `prefers-reduced-motion`; stop repeating progress and spinner animations when it is enabled.
- Keep text wrapping resilient, provide horizontal overflow for tables/code, and preserve native form semantics even when controls are custom styled.

## Minimal token starter

```css
:root {
  --font-sans: "Plus Jakarta Sans", system-ui, -apple-system, "Segoe UI", sans-serif;
  --font-mono: "Cascadia Code", ui-monospace, Consolas, monospace;
  --bg: #f5f6f8;
  --panel: #fff;
  --sunken: #f0f2f5;
  --ink: #16191d;
  --muted: #667085;
  --line: #e2e6ea;
  --accent: #3b5bdb;
  --ok: #2f9e44;
  --warn: #b8860b;
  --bad: #c92a2a;
  --shadow: 0 1px 3px rgba(0, 0, 0, 0.07);
}

@media (prefers-color-scheme: dark) {
  :root {
    --bg: #13151a;
    --panel: #1b1e24;
    --sunken: #15171c;
    --ink: #e8eaed;
    --muted: #98a2b3;
    --line: #2a2f38;
    --accent: #7d92f0;
    --ok: #51cf66;
    --warn: #f0b849;
    --bad: #ff6b6b;
    --shadow: none;
  }
}
```

The closest visual match will come from preserving the neutral layering, compact spacing, Jakarta/Cascadia role split, and subtle border-based hierarchy—not from copying every individual selector.
