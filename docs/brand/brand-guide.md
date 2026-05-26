# NestWeaver Brand Guide

**Parent company:** [kehl.io](https://kehl.io) — Software Development & Consulting

NestWeaver follows the kehl.io design system. All visual assets, UI components,
and documentation should use the colors, fonts, and icon library defined here.

---

## Logo

- **Icon:** `assets/logo-icon-dark.svg` (dark background), `assets/logo-icon-light.svg` (light background)
- **Concept:** Interconnected graph nodes with weaving bezier paths — representing code structure being woven into a knowledge graph
- **Parent branding:** kehl.io logos at `assets/kehl-io/`

### Usage rules

- Minimum size: 32px for icon mark
- Always use on solid backgrounds (dark or light), never on photos or gradients
- Do not rotate, stretch, or recolor the logo

---

## Colors

All colors defined in OKLCH color space for perceptual uniformity.

### Brand palette

| Name | OKLCH | Hex (approx.) | Usage |
|------|-------|---------------|-------|
| Cobalt | `oklch(0.82 0.13 210)` | `#5ed0fe` | Primary — links, interactive elements, primary nodes |
| Halo | `oklch(0.84 0.11 210)` | `#77d4f8` | Ring/focus — outlines, hover states |
| Deep | `oklch(0.45 0.14 240)` | `#0862a7` | Deep blue — gradient midpoint, headers |
| Dusk | `oklch(0.38 0.22 310)` | `#6b119a` | Accent purple — gradient endpoint, emphasis |
| Spark | `oklch(0.89 0.31 130)` | `#4dff00` | Success/accent green — highlights, active states |

### Semantic tokens

| Token | Dark mode | Light mode |
|-------|-----------|------------|
| Background | `oklch(9% 0.005 240)` | `oklch(95% 0.005 240)` |
| Foreground | `oklch(95% 0.005 240)` | `oklch(9% 0.005 240)` |
| Card | `oklch(14% 0.005 240)` | `oklch(1 0 0)` |
| Primary | Cobalt | Deep |
| Accent | Dusk | Dusk |
| Muted | `oklch(20% 0.005 240)` | `oklch(88% 0.005 240)` |
| Border | `oklch(20% 0.005 240)` | `oklch(85% 0.005 240)` |
| Destructive | `oklch(0.577 0.245 27.325)` | same |
| Success | Spark | Spark |
| Warning | `oklch(0.75 0.183 55.934)` | same |

### Gradients

Primary gradient (used in logo, hero elements):
```
linear-gradient(135deg, #5ed0fe 0%, #0862a7 50%, #6b119a 100%)
```

Accent gradient (used for highlights, interactive indicators):
```
linear-gradient(135deg, #4dff00 0%, #5ed0fe 100%)
```

---

## Typography

| Role | Font | Fallback |
|------|------|----------|
| Body | Inter Variable | `ui-sans-serif, system-ui, sans-serif` |
| Display | Michroma | `ui-sans-serif, system-ui, sans-serif` |
| Code | JetBrains Mono Variable | `ui-monospace, "Courier New", monospace` |

- **Display (Michroma):** Used for headings, logo text, hero elements. Geometric and technical.
- **Body (Inter):** Used for all paragraph text, UI labels, descriptions.
- **Code (JetBrains Mono):** Used for code snippets, CLI output, technical content.

---

## Icons

**Library:** [Lucide](https://lucide.dev)

Lucide is used across all kehl.io projects. When adding icons to NestWeaver
(docs, web UI, marketing), always source from Lucide. Do not mix icon libraries.

### Recommended icons for NestWeaver concepts

| Concept | Lucide icon |
|---------|-------------|
| Graph/nodes | `git-graph`, `network` |
| Search | `search` |
| Symbol | `code`, `braces` |
| Impact | `zap`, `scan` |
| Repository | `folder-git-2` |
| Instance | `layers` |
| Snapshot | `package` |
| Security | `shield-check` |
| Settings | `settings` |

---

## UI Framework

When building any frontend (docs site, web UI, dashboards):

- **Tailwind CSS v4** with CSS-first config
- **Radix UI** primitives for accessible, headless components
- **tailwind-merge** for class composition
- Dark mode default, light mode via `.light` class

---

## Parent Branding

The kehl.io logo should appear in the footer of any public-facing page:

```
Built by kehl.io
```

Use `assets/kehl-io/logo-icon-dark.svg` (dark backgrounds) or
`assets/kehl-io/logo-full-light.svg` (light backgrounds) alongside the text.
