---
name: InfinityDB Website
description: Dark engine-room systems aesthetic — Signal Violet on Midnight Chassis, Archivo + JetBrains Mono, borders over shadows.
colors:
  signal-violet: "oklch(0.7 0.2 310)"
  signal-violet-dim: "oklch(0.7 0.2 310 / 0.45)"
  signal-violet-wash: "oklch(0.7 0.2 310 / 0.07)"
  midnight-chassis: "oklch(0.135 0.012 310)"
  chassis-raise: "oklch(0.16 0.014 310)"
  chassis-panel: "oklch(0.165 0.014 310)"
  seam: "oklch(0.24 0.018 310)"
  seam-strong: "oklch(0.3 0.02 310)"
  ink: "oklch(0.94 0.004 310)"
  ink-body: "oklch(0.78 0.012 310)"
  ink-muted: "oklch(0.66 0.02 310)"
  ink-faint: "oklch(0.52 0.02 310)"
  shipped-green: "oklch(0.75 0.16 150)"
  caution-amber: "oklch(0.8 0.14 80)"
typography:
  display:
    fontFamily: "Archivo, system-ui, sans-serif"
    fontSize: "clamp(36px, 6vw, 64px)"
    fontWeight: 700
    lineHeight: 1.05
    letterSpacing: "-0.035em"
  headline:
    fontFamily: "Archivo, system-ui, sans-serif"
    fontSize: "clamp(26px, 3.6vw, 38px)"
    fontWeight: 650
    lineHeight: 1.15
    letterSpacing: "-0.02em"
  title:
    fontFamily: "Archivo, system-ui, sans-serif"
    fontSize: "20px"
    fontWeight: 600
    letterSpacing: "-0.01em"
  body:
    fontFamily: "Archivo, system-ui, sans-serif"
    fontSize: "16px"
    fontWeight: 400
    lineHeight: 1.6
  label:
    fontFamily: "JetBrains Mono, ui-monospace, monospace"
    fontSize: "11px"
    fontWeight: 500
    letterSpacing: "0.18em"
rounded:
  xs: "5px"
  sm: "8px"
  md: "10px"
  lg: "12px"
  full: "100px"
spacing:
  sp-1: "8px"
  sp-2: "14px"
  sp-3: "22px"
  sp-4: "36px"
  sp-5: "56px"
  sp-6: "88px"
  sp-7: "clamp(72px, 12vw, 128px)"
components:
  button-primary:
    backgroundColor: "{colors.signal-violet}"
    textColor: "oklch(0.14 0.02 310)"
    rounded: "9px"
    padding: "13px 22px"
  button-ghost:
    textColor: "{colors.ink}"
    rounded: "9px"
    padding: "13px 22px"
  nav-cta:
    backgroundColor: "{colors.ink}"
    textColor: "{colors.midnight-chassis}"
    rounded: "{rounded.sm}"
    padding: "9px 16px"
  card:
    backgroundColor: "{colors.chassis-panel}"
    rounded: "{rounded.lg}"
    padding: "26px 28px"
  pill:
    typography: "{typography.label}"
    rounded: "{rounded.full}"
    padding: "4px 9px"
---

# Design System: InfinityDB Website

## 1. Overview

**Creative North Star: "The Engine Room at Night"**

A dark control room where the machinery is visible and humming. Every surface is part of the machine housing — near-black violet-tinted panels seamed with hairline borders — and the interesting things are the instruments mounted on it: a live terminal, a milestone rail with a pulsing "now" dot, shard-cell diagrams, status pills, and evidence blocks that carry their artifact paths like equipment tags. The single Signal Violet accent behaves like an indicator lamp: it marks what is active, proven, or asking for attention, and nothing else.

The system is disciplined but not cold. Per PRODUCT.md the voice is "ambitious, disciplined, warm": generous section padding, soft radial glows behind the hero and the closing CTA, comfortable prose measures, and hand-built diagram components keep the room humane. What it explicitly rejects, from PRODUCT.md's anti-references: benchmark-war database marketing (no hype charts, no big-number heroes — the evidence block replaces them), generic SaaS landing pages (no gradient heroes, logo walls, or pricing cards), and academic plain-text austerity (the honesty is crafted, not dumped).

**Key Characteristics:**
- One accent, used like an indicator lamp — active, proven, or attention, never decoration
- Borders and tonal steps carry all depth; shadows are reserved for one hero object
- Two-voice typography: Archivo speaks prose, JetBrains Mono labels the machinery
- Evidence blocks, milestone pills, and generated tables are first-class brand components
- Motion is quiet and mechanical: fade-ups, a typing terminal, one pulsing roadmap dot

## 2. Colors

A drenched-dark violet monochrome with one saturated signal color and two semantic status hues.

### Primary
- **Signal Violet** (oklch(0.7 0.2 310)): the indicator lamp. Kickers, active nav, milestone "NOW" states, links, callout tags, the primary button fill, hero highlight underline, and the glows. At 45% alpha (**Signal Violet Dim**) it draws active borders; at 7% (**Signal Violet Wash**) it tints emphasized bands like the fabric strip and the `+durability` workload row.

### Neutral
- **Midnight Chassis** (oklch(0.135 0.012 310)): the body background — dark machine housing, faintly violet, never pure black.
- **Chassis Raise** (oklch(0.16 0.014 310)) / **Chassis Panel** (oklch(0.165 0.014 310)): the two tonal steps for raised strips, `pre` blocks, cards, and diagram boxes.
- **Seam** (oklch(0.24 0.018 310)) / **Seam Strong** (oklch(0.3 0.02 310)): 1px hairline borders — the visible joinery of every panel, table, and section divider.
- **Ink** (oklch(0.94 0.004 310)): headings and high-emphasis text. **Ink Body** (oklch(0.78 0.012 310)) for prose, **Ink Muted** (oklch(0.66 0.02 310)) for supporting copy, **Ink Faint** (oklch(0.52 0.02 310)) for microlabels and equipment tags only — never running prose.

### Status accents
- **Shipped Green** (oklch(0.75 0.16 150)): the `SHIPPED` pill and positive status only.
- **Caution Amber** (oklch(0.8 0.14 80)): warning callouts and caution pills only.

### Named Rules
**The Indicator Lamp Rule.** Signal Violet marks state — active, proven, current, interactive — never decoration. If removing the violet from an element would lose no information, the violet is wrong. Green and amber exist solely as status semantics on pills and callouts; they never decorate.

## 3. Typography

**Display/Body Font:** Archivo (with system-ui fallback)
**Label/Mono Font:** JetBrains Mono (with ui-monospace fallback)

**Character:** A grotesque that speaks and a monospace that labels. Archivo carries every sentence with tight, confident headline tracking; JetBrains Mono is the voice of the machine itself — version badges, kickers, pills, table headers, artifact paths, terminal output.

### Hierarchy
- **Display** (700, clamp(36px, 6vw, 64px), 1.05, -0.035em): hero headline only; may carry the Signal Violet `inset box-shadow` underline highlight on one phrase.
- **Headline** (650–700, clamp(26px, 3.6vw, 38px), 1.15, -0.02em): section and page h2s. Page titles run slightly larger (clamp(34px, 5.4vw, 52px), -0.03em).
- **Title** (600, 20px): h3s inside docs and cards.
- **Body** (400, 16px, 1.6): all prose in Ink Body; ledes at 18px Ink Muted, max 46em measure. Docs column caps at 820px.
- **Label** (500, 10–12px, 0.08–0.18em tracking, uppercase): the mono microlabel — kickers, pills, table headers, breadcrumbs, terminal titles, mono-notes.

### Named Rules
**The Two Voices Rule.** Prose is always Archivo; anything small, structural, or machine-generated (labels, tags, paths, code, statuses) is always JetBrains Mono. No third font, ever, and neither voice does the other's job.

## 4. Elevation

Borders over shadows. Depth is conveyed by hairline seams (Seam / Seam Strong) and two tonal background steps (Chassis Raise / Chassis Panel) — surfaces read as panels bolted to the same housing, not floating layers. Exactly one object in the whole site carries a true drop shadow: the hero terminal (`box-shadow: 0 30px 80px oklch(0 0 0 / 0.4)`), which earns it by being the one "lifted" instrument. Atmosphere comes from fixed radial glows (Signal Violet at ~14% alpha behind the hero and CTA band) and a masked dot grid, both `pointer-events: none`.

### Shadow Vocabulary
- **Hero lift** (`box-shadow: 0 30px 80px oklch(0 0 0 / 0.4)`): the terminal only.
- **Pulse ring** (`box-shadow: 0 0 0 N oklch(0.7 0.2 310 / 0.5→0)`): the animated halo on the roadmap's "now" dot.

### Named Rules
**The One Shadow Rule.** New components do not get drop shadows. If a surface needs separation, it gets a seam and a tonal step. The terminal keeps its lift because there is only one hero instrument per room.

## 5. Components

Workshop warmth: precise but hand-built. Components read like labeled equipment — visible seams, mono tags, generous internal padding — rather than mass-produced SaaS blocks.

### Buttons
- **Shape:** softly rounded (9px radius), inline-block, weight 650, 15px.
- **Primary:** Signal Violet fill with near-black violet text (oklch(0.14 0.02 310)), padding 13px 22px. Hover brightens the lamp (`filter: brightness(1.08)`).
- **Ghost:** transparent with a Seam Strong 1px border, Ink text, weight 550. Hover shifts the border to Signal Violet Dim.
- **Nav CTA:** inverted — Ink background, Midnight Chassis text, 8px radius, compact 9px 16px padding.

### Status Pills
- **Style:** mono microlabel (500, 10px, 0.08em tracking) in a 100px full-round outline, padding 4px 9px. Outline-only, never filled.
- **Variants:** `SHIPPED` in Shipped Green, `NOW`/`IN DEV` in Signal Violet, planned milestones in Ink Faint with a Seam Strong border. Every roadmap-feature mention carries one — the pill is the honesty system made visible.

### Cards / Containers
- **Corner Style:** 12px (cards, terminal, status strip); 10px for callouts, evidence blocks, cells, and table wrappers.
- **Background:** Chassis Panel for cards and diagram cells; Chassis Raise for callouts, evidence blocks, and code.
- **Shadow Strategy:** none — seams and tonal steps only (see Elevation).
- **Border:** always 1px Seam Strong.
- **Internal Padding:** 26px 28px for cards; 16–22px for compact panels.

### Evidence Block (signature)
The replacement for the benchmark chart. A Chassis Raise panel (10px radius, Seam Strong border) whose claim text runs in Ink Muted, opens with a Signal Violet **tag** (e.g. "C12 · Allowed"), and closes with the artifact path in 12px mono Ink Faint, `overflow-wrap: anywhere`. Every public number on the site lives inside one.

### Terminal (signature)
The hero instrument: Chassis Raise body, 12px radius, the site's only drop shadow, a chrome bar of three neutral dots plus a mono tracked title, and a 13px/2.05 mono body whose lines type in sequentially with a blinking Signal Violet block cursor.

### Milestone Rail (signature)
The roadmap as a train track: a 14-column grid of stops, 2px connector lines (Signal Violet when done, Seam-toned ahead), 14px dots (filled violet when done, pulsing on "now", dim-outlined at GA), each stop tagged with a mono milestone code and description.

### Tables
Wrapped in a Seam Strong 10px-radius scroll container. Headers are the mono microlabel (500, 11px, 0.08em, uppercase, Ink Faint) on Chassis Raise; cells are 14px Ink Body with Seam row separators; last row loses its border.

### Navigation
Sticky, blurred (14px backdrop blur over Midnight Chassis at 82% alpha), bottom-seamed. Brand mark "∞ InfinityDB" with the mono version badge pill (`v0.2.0-alpha.1 · IN DEV`) beside it. Links are 13px/500 Archivo in Ink Muted, hover to Ink, `aria-current` page in Signal Violet. Collapses to a bordered hamburger below 760px.

### Callouts
Chassis Raise panel, 10px radius, Seam Strong border with a 3px Signal Violet left edge (Caution Amber for `warn`); the leading tag renders in the edge color. This is the system's one legacy left-edge treatment — keep it consistent, don't spread the pattern to new components.

## 6. Do's and Don'ts

### Do:
- **Do** put every public number inside an Evidence Block with its ledger row and artifact path — the site's copy is bound by the ledger-copy CI check (L10); a number without a receipt fails the build.
- **Do** pin a status pill (`SHIPPED` / `NOW` / milestone code) to every capability mention — honesty labels are brand elements, not fine print.
- **Do** use Signal Violet only where it marks state (The Indicator Lamp Rule) and keep it near 10% of any viewport.
- **Do** keep prose in Archivo at Ink Body (oklch(0.78 0.012 310)) or brighter; Ink Faint is for microlabels only.
- **Do** ship a reduced-motion fallback for every animation — the global `prefers-reduced-motion` kill-switch is doctrine, and scroll-driven reveals stay visible-by-default via `@supports (animation-timeline: view())`.

### Don't:
- **Don't** produce "benchmark-war database marketing": no hype multipliers, no cherry-picked charts, no "fastest database" claims, no big-number hero metrics. The evidence-policy block explains why the numbers are absent — that absence is the design.
- **Don't** drift toward the "generic SaaS landing page": no gradient heroes, no logo walls, no pricing-tier cards, no `background-clip: text` gradient headlines, no glassmorphism beyond the nav blur.
- **Don't** swing to "academic / plain-text austerity" either — every honest disclosure still gets crafted presentation (pills, evidence blocks, diagrams), never a wall of undesigned text.
- **Don't** add drop shadows to new surfaces (The One Shadow Rule) — use a seam and a tonal step.
- **Don't** introduce new colored left-edge stripes; the callout is the single grandfathered instance.
- **Don't** edit `site/docs/compat.html` by hand — it is generated from the `inf-wire` registry and CI fails on drift.
