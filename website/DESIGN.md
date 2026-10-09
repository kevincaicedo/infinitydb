# InfinityDB — website design

The visual system of the InfinityDB website: identity, tokens, type,
layout, components, motion and voice. The source design is the canvas
*InfinityDB — Website & Identity* (identity, mascot, landing, docs and blog
artboards, light, dark and mobile). This file is the written contract the
implementation in `build.py`, `src/assets/site.css` and `src/assets/site.js`
answers to. When the two disagree, fix the code or amend this file first.

## 1. The idea

**A pixel, a loop, one owner.** The mark is the reactor loop drawn on a
pixel grid. One signal pixel sits where the loop crosses itself: the point
every request passes through, once. Everything else is ink and paper.

Three rules carry the whole system:

1. **Ink and paper do the work.** Two neutrals, many greys, hairlines.
2. **Signal marks what is live**: the current milestone, the log tail, a
   request in flight, the page you are on. Never decoration, never a
   background, never body text in light mode.
3. **Everything is a function of something.** Pixel art is a grid, the
   pixel field is a function of the frame number, covers are seeded. Nothing
   is drawn by hand that a rule can draw.

## 2. The mark

| Property | Rule |
|---|---|
| Grid | 15 × 7 modules; one module is one pixel. |
| Signal | One signal pixel at the crossing, module (7, 3). |
| Clearspace | 2 modules on every side. |
| Lockup | Mark and wordmark (Geist 600, −0.035em) share one baseline, 4 modules apart. |
| Optical sizes | 15 × 7 from 24 px up (2 px per module or more); an 11 × 5 cut at 16 px and below (favicon, app icon). |
| Loading | The signal pixel runs the loop: thirty-two steps, one per module, never faster. |
| Never | Rotate, outline, round, recolor, or put Moss inside the lockup. |

Both cuts are defined once in `build.py` (`LOGO`, `LOGO_SMALL`) and drawn
as one SVG path per color with `shape-rendering: crispEdges`. The favicon
(`assets/favicon.svg`) is the 11 × 5 cut and follows the system theme.

## 3. Color

Tokens are CSS custom properties on `:root`, redefined for dark mode under
`prefers-color-scheme: dark` (unless the visitor chose light) and under
`[data-theme="dark"]`.

| Token | Light | Dark | Role |
|---|---|---|---|
| `--bg` | `#F4F4F0` paper | `#0B0B0A` night | Page ground |
| `--surface` | `#FAFAF7` | `#111110` | Panels, figures, cards |
| `--raise` | `#FFFFFF` | `#1A1A18` | Active nav item, Moss's body |
| `--code` | `#ECECE6` | `#141413` | Code blocks, inline code |
| `--ink` | `#0E0E0C` | `#EDEDE7` | Text, pixels, primary action |
| `--ink2` | `#34342F` | `#C8C8C1` | Body copy |
| `--muted` | `#6B6B65` graphite | `#94948D` | Secondary text, labels |
| `--dim` | `#8A8A83` | `#6A6A64` | Dimmed pixels, inactive lines |
| `--faint` | `#BDBDB6` | `#3A3A37` | Background pixels |
| `--line` | `#D9D9D2` hairline | `#2A2A27` | Grid lines, borders |
| `--line2` | `#E8E8E2` | `#1E1E1C` | Idle cells, code borders |
| `--signal` | `#FF5A1F` | `#FF6526` | The present, sparingly |
| `--signal-text` | `#B83A0B` | `#FF7A3F` | Signal that must be read as text |
| `--btn-text` | `#F4F4F0` | `#0B0B0A` | Text on ink buttons |

No gradients, no shadows (the one exception is the search results
dropdown, which floats over content), no alpha for tone: soft edges come
from dithering.

## 4. Type

One sans, one mono, one pixel face, all from Google Fonts.

| Face | Weights | Use |
|---|---|---|
| **Geist** | 400, 500, 600 | Display and text. Display sizes track tight (−0.05em at 72 px and up, −0.04em at 40–56 px). |
| **Geist Mono** | 400, 500 | Labels, navigation, code. Uppercase labels at +0.06 to +0.08em. |
| **Doto** | 900 | Pixel numerals only: section numbers, milestones, laws. Never sentences. |

Scale used on the site (desktop → phone):

| Role | Size |
|---|---|
| Hero | 92 / 0.96 → 46 / 0.98 |
| Page title (blog, post) | 72–80 → 36 |
| Section heading | 48 / 1.08 → 30 |
| Feature heading | 36 / 1.1 → 28 |
| Docs title | 48 → 36; docs h2 26 → 22 |
| Lede | 18–20 → 15–16 |
| Body | 16–18 / 1.6–1.7 |
| Eyebrow | Mono 12, uppercase, +0.08em |
| Figure label | Mono 11, uppercase, +0.06em |

## 5. Layout

- **Desktop artboard** 1440 px: 120 px gutters, a 12-column grid with
  24 px gaps, content 1200 px wide. Sections are separated by hairlines,
  never by background changes.
- **Gutters** step down with the viewport: 120 px from 1280 px, 64 px from
  900 px, 40 px from 640 px, 20 px below.
- **Breakpoints**: 1280 (roadmap becomes a vertical rail, docs lose the
  "On this page" column), 1100 (navigation collapses to the menu), 900
  (grids stack, docs sidebar becomes a drawer), 640 (phone).
- **Phone**: one column, 20 px gutters, 48 px touch targets (44 px
  minimum for icon buttons). Figures keep their geometry and scale as a
  unit, and the pixel field drops to a finer 6 px grid.
- Corners are square everywhere.

## 6. Components

| Component | Shape |
|---|---|
| Announcement bar | 40 px (36 px phone), mono 12: signal square, current milestone, blurb, "Follow the build". |
| Navigation | 72 px (60 px phone). Brand, five mono links, GitHub (outline) and Get started (ink). The active page carries a 6 px signal square. |
| Menu (phone) | Full-screen panel: 64 px links at 28 px, theme switch, Get started, alpha badge. |
| Buttons | 48 px primary (ink on paper) and outline; 40 px and 32 px small variants. |
| Badge / law tag | Mono 11–12 uppercase in a hairline box, 26–30 px tall. |
| Eyebrow | Mono 12 uppercase, muted, above every section heading. |
| Workload card | Status line (solid square = available, outline = planned), title, two lines, class line. |
| Law cell | Pixel code (Doto), title, one sentence, in a hairline grid. The last cell is ink: "Read next". |
| Callout | Surface panel with an 8 px ink square and a mono label. |
| Code block | `--code` panel, 34 px mono header with a copy button; prompts muted and never copied; output muted. |
| Table | Ink top rule, mono uppercase headers, hairline rows. |
| Chips | 36 px mono filters; the selected chip is ink. |
| Docs sidebar | 280 px; groups with eyebrows; 32 px items (44 px in the phone drawer); active item on `--raise` with a signal square; planned items muted with their milestone tag. |
| Roadmap train | One stop per big milestone (M0 to M11): done = ink square, now = signal square with a ring and "you are here", next = outline. On narrow screens a vertical rail with the current row raised. |
| Post cover | A seeded, dithered pixel illustration (`cover()` in `build.py`), one signal pixel. |

## 7. Figures

The five "how it works" figures and the tiering deep dive are drawn on a
fixed stage (558 × 418 inside a 560 × 420 frame; 1198 × 198 for the wide
track) and scale as a unit with their container. The scale is computed in
CSS (`tan(atan2(100cqw, 558px))`), with a ResizeObserver fallback. Below
440 px of figure width, labels grow, secondary labels hide and borders
thicken, so a phone reads the figure instead of a miniature of it.

Every figure has an `aria-label` that says what it shows. Motion inside a
figure is CSS keyframes on transforms and opacity; nothing is
script-driven.

## 8. The pixel field

The background texture of the brand, behind the hero.

- **Grid**: 8 px cells, 4 px pixels (6 px cells on phones). Nothing sits
  between cells.
- **Tone**: 1-bit. Soft edges come from a 4 × 4 ordered (Bayer) dither,
  never from alpha.
- **Motion**: a pure function of the frame number, so every frame can be
  replayed. The frame counter is printed in the corner.
- **Accent**: one signal pixel rides the crest: a request, in flight.
- **Respect**: reduced motion draws one still frame; the loop pauses when
  the hero is off screen or the tab is hidden.

## 9. Moss

Moss is the mascot: a tardigrade, a moss piglet. Tardigrades survive
boiling, freezing and vacuum; Moss survives torn writes, lying fsyncs and
power cuts, in simulation, every night. Eight legs, one for each cell.

| Pose | Meaning |
|---|---|
| Idle | polling, blinking now and then |
| Walk | the reactor loop: two frames, forever |
| Park | waiting on the next completion (the 404 page) |
| Seed | every failure is a seed; Moss keeps it |

Rules: integer scaling only (1×, 2×, 4×, 8×), never blurred or rotated.
One signal pixel per pose: the blush or the seed, never both. Ink and paper
swap between themes. Moss is a character, not the logo.

## 10. Motion

Motion is stepped and mechanical: `steps()` for masks and cursors, short
linear keyframes for blinks, one cubic-bezier for travel. Every animation
stops under `prefers-reduced-motion: reduce`, and every figure still reads
when frozen.

## 11. Voice

- **Plain words first, then what is underneath.** Every feature leads
  with a sentence a non-engineer can follow, then an "Under the hood" line
  for the engineers.
- **Short declaratives.** Commas, colons and full stops; no em-dash
  chains, no exclamation marks, no emoji.
- **No numbers without evidence.** The site publishes no benchmark,
  latency, throughput or memory figure. Configuration values, format
  bounds and counts are not claims. A claim appears only when it is
  measured to the standard the Evidence section describes.
- **Status is always visible.** Shipped work says "Alpha"; anything else
  carries its milestone (`Planned · M7`). Nothing planned is described as
  available, and nothing is tagged as released before it is.
- **Big milestones only.** Public copy names M0 to M11 and describes a
  phase by what it does ("the trust phase of M4"), never by a dot
  milestone or a story, decision or review identifier. The repository's
  public-docs gate enforces it on every page of the site.
- **Commands are real.** Every command and reply on the site is one the
  current tree accepts and prints.

## 12. Accessibility

Real `<a>`, `<button>`, `<input>` and `<label>` everywhere; icon-only
buttons carry `aria-label`; figures carry `aria-label`; text meets 4.5:1
(signal used as text switches to `--signal-text`); focus is a 2 px ink
outline; a skip link opens every page; the site reads without JavaScript.
