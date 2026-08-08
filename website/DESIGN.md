---
name: InfinityDB Website
description: Dual-lamp dark systems aesthetic — Signal Violet + Proof Teal on Near-Black, Archivo 500–800 + JetBrains Mono, hairline slate seams, particle-animated diagrams.
colors:
  near-black: "#06070d"
  panel: "#0b0e19"
  seam: "rgba(139, 144, 168, 0.14)"
  seam-mid: "rgba(139, 144, 168, 0.2)"
  seam-strong: "rgba(139, 144, 168, 0.3)"
  signal-violet: "#7c5cff"
  violet-soft: "#b9a8ff"
  violet-dim: "rgba(124, 92, 255, 0.45)"
  violet-wash: "rgba(124, 92, 255, 0.12)"
  proof-teal: "#3ee6c4"
  teal-dim: "rgba(62, 230, 196, 0.45)"
  teal-wash: "rgba(62, 230, 196, 0.08)"
  ink: "#e8e9f2"
  ink-body: "#c4c8d8"
  ink-muted: "#9096ae"
  ink-faint: "#5c6178"
  caution-amber: "#ffc45c"
  alarm-red: "#ff5c78"
typography:
  display:
    fontFamily: "Archivo, system-ui, sans-serif"
    fontSize: "clamp(36px, 4.6vw, 52px)"
    fontWeight: 800
    lineHeight: 1.04
    letterSpacing: "-0.025em"
  headline:
    fontFamily: "Archivo, system-ui, sans-serif"
    fontSize: "clamp(26px, 3.4vw, 40px)"
    fontWeight: 700
    lineHeight: 1.12
    letterSpacing: "-0.02em"
  title:
    fontFamily: "Archivo, system-ui, sans-serif"
    fontSize: "20px"
    fontWeight: 700
    letterSpacing: "-0.01em"
  body:
    fontFamily: "Archivo, system-ui, sans-serif"
    fontSize: "16px"
    fontWeight: 400
    lineHeight: 1.65
  label:
    fontFamily: "JetBrains Mono, ui-monospace, monospace"
    fontSize: "10-12px"
    fontWeight: 500
    letterSpacing: "0.06em-0.18em"
rounded:
  xs: "5px"
  sm: "8px"
  md: "10px"
  lg: "14px"
  full: "99px"
spacing:
  sp-1: "8px"
  sp-2: "14px"
  sp-3: "22px"
  sp-4: "36px"
  sp-5: "56px"
  sp-6: "88px"
  sp-7: "clamp(72px, 11vw, 104px)"
components:
  button-primary:
    backgroundColor: "{colors.proof-teal}"
    textColor: "{colors.near-black}"
    typography: "mono 700 12px, 0.1em tracking, uppercase"
    rounded: "{rounded.sm}"
    padding: "13px 22px"
    hover: "background {colors.signal-violet}, text {colors.ink}"
  button-ghost:
    border: "1px {colors.seam-strong}"
    textColor: "{colors.ink-muted}"
    rounded: "{rounded.sm}"
    padding: "13px 22px"
    hover: "border + text {colors.proof-teal}"
  nav-cta:
    backgroundColor: "{colors.proof-teal}"
    textColor: "{colors.near-black}"
    rounded: "{rounded.sm}"
    padding: "9px 18px"
  card:
    backgroundColor: "{colors.panel}"
    border: "1px rgba(139,144,168,0.18)"
    rounded: "{rounded.lg}"
    padding: "26px"
    hover: "border {colors.violet-dim}, translateY(-3px)"
  pill:
    typography: "{typography.label}"
    rounded: "{rounded.full}"
    padding: "4px 9px"
---

# Design System: InfinityDB Website

## 1. Overview

**Creative North Star: "Two Lamps on Near-Black"**

A dark instrument panel where data is visibly *moving*. The surface is a
near-black blue (`#06070d`) seamed with hairline slate borders; on it, two
indicator lamps divide all meaning between them. **Signal Violet** is the
lamp of motion and intent — records in flight, the current milestone, the
in-dev surface, prompts, pulses. **Proof Teal** is the lamp of arrival —
links, CTAs, shipped capabilities, measured numbers, the `$` of a live
shell. Diagrams are not illustrations; they are animated instruments (SMIL
particles riding SVG paths through the log spine, the reactor orbit, the
fabric mesh), and the one sanctioned place the two lamps blend is the
violet→teal gradient: the hero headline phrase, the benchmark bar fills,
and the featured-card wash — motion arriving at proof.

The register is unchanged from PRODUCT.md: ambitious, disciplined, warm —
and the honesty machinery (status pills, evidence blocks, the milestone
rail, ledger-cited numbers) remains first-class brand material, now dressed
in the dual-lamp language. Ported from the approved prototypes
`assests/Landing.dc.html` / `Docs.dc.html` / `Blog.dc.html`; the site must
track them.

**Key characteristics:**
- Two accents with disjoint jobs: violet = in motion / current, teal =
  proven / interactive. Their blend is reserved, never ambient.
- Hairline slate seams (`rgba(139,144,168, .14/.2/.3)`) and one tonal step
  (`#0b0e19`) carry all depth; the terminal keeps the site's one drop shadow.
- Two-voice typography, harder-edged than v1: Archivo 800 headlines,
  and JetBrains Mono now also speaks the chrome — nav links, buttons,
  kickers, stat values are mono, uppercase, tracked.
- Diagrams are animated instruments: particles on paths, a pulsing tail
  block, doorbell rings, scroll-triggered bar growth.
- Numbered mono kickers (`01 · ONE ENGINE, FIVE WORKLOADS`) sequence the
  landing page as one continuous readout — a deliberate, named system,
  used only there.

## 2. Colors

### Surfaces & seams
- **Near-Black** `#06070d` — the body. Faintly blue, never pure black.
- **Panel** `#0b0e19` — the single raised step: cards, code, terminal,
  diagram boxes, bands.
- **Seams** — slate `#8b90a8` at 14% (section/nav hairlines), 20% (code
  blocks, evidence), 22–30% (cards, tables, strong borders). Depth is
  seams + the one tonal step, not shadows.

### The two lamps
- **Signal Violet** `#7c5cff` (+ soft `#b9a8ff`, dim 45%, wash 12%):
  motion and intent. Card index numbers (`/01`), the `inf>` prompt,
  in-flight particles, the NOW milestone dot and its pulse, `M# · IN DEV`
  pills, active sidebar item wash, card hover borders.
- **Proof Teal** `#3ee6c4` (+ dim 45%, wash 8%): arrival and proof. Links,
  primary/nav CTAs, kickers, the `$` prompt, SHIPPED pills, done milestone
  dots and connectors, measured-number highlights, the terminal cursor,
  marquee separators (alternating with violet).
- **The blend** — `linear-gradient(90deg, #7c5cff, #3ee6c4)`: the hero
  headline phrase, benchmark bar fills, the featured-card wash
  (135deg, washes). Nowhere else.

### Ink
- **Ink** `#e8e9f2` headings/high-emphasis · **Ink Body** `#c4c8d8` prose ·
  **Ink Muted** `#9096ae` supporting copy and ledes · **Ink Faint**
  `#5c6178` microlabels, equipment tags, artifact paths only — never
  running prose.

### Status
- **Caution Amber** `#ffc45c` — warn callouts, `partial` compat chips.
- **Alarm Red** `#ff5c78` — the Vortex bar and terminal-chrome dot only;
  never marketing.

### Named Rules
**The Two Lamps Rule.** Violet marks what is *in motion* (current,
in-dev, in-flight); teal marks what is *proven or actionable* (shipped,
measured, clickable). If swapping an element's lamp would not change its
meaning, the color is decoration and therefore wrong. The gradient is
their only blend and appears in exactly three places (hero phrase, bar
fills, featured wash).

## 3. Typography

**Display/Body:** Archivo (500–800). **Machine voice:** JetBrains Mono
(400/500/700).

### Hierarchy
- **Display** (800, clamp(36–52px), 1.04, −0.025em): hero + CTA-band
  headlines; the hero carries the gradient on one phrase.
- **Headline** (700, clamp(26–40px), 1.12, −0.02em): landing section h2s.
  Docs article h1s run 800 at clamp(32–44px); docs h2s drop to 24px/700.
- **Title** (700, 20–21px): card and post-card headings.
- **Body** (400, 16px/1.65): prose in Ink Body/Muted; blog ledes 17px
  Ink Body; docs body 14px Ink Muted.
- **Label** (500–700, 9.5–12px mono, 0.06–0.18em, uppercase): kickers,
  nav links, buttons, pills, table headers, stat labels, marquee,
  breadcrumbs, meta lines.

### Named Rules
**The Two Voices Rule (extended).** Prose is always Archivo. Everything
small, structural, machine-generated, or *actionable-chrome* — labels,
tags, paths, code, statuses, nav links, buttons, stat values — is
JetBrains Mono. No third font. In v2 the mono voice grew: it now speaks
the navigation and every button, always uppercase and tracked.

## 4. Elevation

Seams over shadows, unchanged in spirit:
- **The One Shadow Rule.** The terminal keeps the site's only drop shadow
  (`0 24px 80px rgba(0,0,0,0.5)`). Everything else separates with a seam
  and the Panel step. Card hover lifts via `translateY(-3px)` + a violet
  border — never a shadow.
- **Atmosphere:** fixed radial glows — violet top-left + teal top-right in
  the hero, a violet ellipse under the CTA band — plus the hero's masked
  48px line grid. All `pointer-events: none`.
- **Pulse ring** (`box-shadow: 0 0 0 12px rgba(124,92,255,0)` endpoint):
  the roadmap NOW dot only.

## 5. Components

### Buttons
Mono voice: 700, 12px, 0.1em tracking, uppercase, 8px radius,
13px 22px padding. **Primary:** teal fill, near-black text; hover flips
to violet fill + ink text (the lamp handing off). **Ghost:** seam-strong
border, muted text; hover turns border + text teal. **Nav CTA:** compact
teal fill (9px 18px).

### Status Pills (the honesty system)
Outline-only mono microlabels, 99px radius: `SHIPPED`/`SHIPPING` in teal,
`M# · IN DEV`/`NOW` in soft violet, planned milestones in Ink Faint +
seam-strong. Every capability mention carries one. Blog category pills
are the one *filled* variant (teal fill, dark text).

### Cards
Panel background, 18%-seam border, 14px radius, 26px padding; hover:
violet border + 3px lift. Workload cards open with a violet mono index
(`/01`) and a faint uppercase tagline (`TARGETS …` — never "replaces" as
a claim). The compute card wears the gradient wash + teal border.

### Evidence Block (signature, carried from v1)
Panel-raised, 10px radius, seam-mid border: mono teal tag ("C12 ·
Allowed"), Ink Muted claim text in the ledger's allowed wording, mono
Ink Faint artifact path (`overflow-wrap: anywhere`). Every measured
number on the site lives in one or in a stat tile citing its row.

### Stat Tiles
Near-black tile, seam-mid border, 10px radius: mono 700 22px value
(Ink, or teal/violet-soft when the value itself is the lamp), faint
9.5px uppercase label. Used only for ledger-Allowed measurements —
never targets (§18 of the master plan forbids targets in public copy).

### Benchmark Bars (signature, new)
SVG bar pairs with gradient-filled InfinityDB bars and slate competitor
bars, labels in mono. Bars render at full width by default and animate
`0 → width` via SMIL `begin="indefinite"` triggered by an
IntersectionObserver — content is never gated on the animation. Every
chart states box, cells, and run context in the adjacent mono footnote,
and only Allowed/Narrowed rows chart.

### Terminal (signature)
Panel body, 14px radius, the one drop shadow, chrome bar with
red/amber/teal dots + mono title. 12.5px/2 mono body: `$` teal,
`inf>` soft violet, comments faint; roadmap commands render dimmed
below a dashed `ROADMAP SURFACE` divider with milestone tags; teal
block cursor blinks.

### Milestone Rail (signature)
14-stop flex rail: teal connectors + filled dots behind us, violet
pulsing dot on NOW, seam-toned ahead, outlined teal at GA. Mono
labels: code (700), description, status microline.

### Marquee (new)
Full-bleed band between hairlines, `rgba(11,14,25,0.6)` fill: one
mono 11.5px tracked line of product facts separated by ✦ marks
alternating teal/violet, duplicated for a seamless 30s loop, paused
on hover and killed under reduced motion.

### Diagrams (signature)
Hand-built SVGs in the two-lamp language: violet particles flow *in*
(commands, appends), teal particles flow *out* (serves, projections),
via SMIL `animateMotion` on dashed slate paths. Moving dots carry
`class="anim-dot"` and are hidden under `prefers-reduced-motion`.
Node boxes are Panel-filled with lamp-tinted borders matching their
status (teal = shipping, violet = in dev, slate = planned).

### Docs Shell (new)
1280px two-column grid: sticky 250px sidebar (mono 12.5px links,
faint uppercase group headers, violet-wash active item) + 760px
article column (teal doc-kicker, 800-weight h1). Collapses to a
wrapped horizontal link list under 880px.

### Tables
Seam-mid 10px-radius scroll wrapper; mono 10px uppercase faint headers
on Panel; 13.5px muted cells; mono teal first column for key/mode
columns; last row loses its border.

### Navigation
Sticky, 14px blur over `rgba(6,7,13,0.82)`, bottom hairline. Brand:
the dual-circle logo (violet + teal rings — the infinity mark),
`infinityDB` in 800 with teal `DB`, and the version badge pill
(`v0.3.0-alpha.1 · IN DEV` on the landing; a neutral section badge
`DOCS`/`BLOG` on inner pages). Links: mono 12px, 0.1em, uppercase,
muted → teal on hover/current. One filled CTA (`★ GITHUB`).

### Callouts
Panel-raised, 10px radius, seam-mid border with a 3px teal left edge
(amber for `warn`); the leading tag takes the edge color. The blog's
`LAW DERIVED` block is the same family (2px teal edge, no fill).

## 6. Motion

Quiet, mechanical, continuous — the machine is running:
- **fadeUp** entrance staggers on the hero (`.fx-1…6`) and a single
  0.4s fade on inner-page articles.
- **Particle flows** (SMIL animateMotion) on every diagram; the log's
  tail block pulses opacity.
- **typeIn** terminal lines (~0.25–0.35s stagger) + blinking cursor.
- **marquee** 30s linear loop, duplicated track.
- **pulseDot + pulse ring** on the roadmap NOW dot only.
- **Bar growth** on scroll-entry (IntersectionObserver → SMIL
  `beginElement`), skipped entirely under reduced motion.
- **Reduced motion is doctrine:** the global kill-switch zeroes every
  animation/transition, `.anim-dot` particles hide, marquee stops,
  bars and content render complete and static.

## 7. Do's and Don'ts

### Do:
- **Do** keep every measured number inside an Evidence Block or stat
  tile citing an **Allowed** ledger row, with box/cells/tier disclosed
  in the adjacent mono footnote — the ledger-copy CI check fails the
  build otherwise (L10).
- **Do** pin a status pill or milestone tag to every capability mention,
  including terminal commands (`# M7`) and diagram nodes.
- **Do** give each lamp its own job (The Two Lamps Rule) and keep the
  gradient to its three sanctioned sites.
- **Do** state what is *absent* and why (Narrowed/Evidence-pending rows,
  planned posts, cut lines) — designed absence is brand material.
- **Do** ship the reduced-motion fallback for every animation, including
  SMIL (hide `.anim-dot`, skip `beginElement`).

### Don't:
- **Don't** publish targets as numbers anywhere on the site — master
  plan §18 forbids it; targets live in the plan, measurements on the
  page ("MEASURED, NOT PROMISED" is the section's name for a reason).
- **Don't** blend or swap the lamps: no teal NOW dots, no violet links,
  no gradient on anything but the three sanctioned sites.
- **Don't** add drop shadows to new surfaces (One Shadow Rule) — seam +
  panel step + lift transform.
- **Don't** write "REPLACES X" as a claim — capability taglines say
  `TARGETS …`; comparisons exist only as in-run measured rows.
- **Don't** hand-edit `site/docs/compat.html` — it is generated
  (`scripts/gen-compat-page.py`) and CI fails on drift.
- **Don't** spread the numbered-kicker system beyond the landing page —
  it sequences one page's readout; docs and blog use plain kickers.
