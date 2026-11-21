---
name: snippet-design
description: Snippet's visual language for HTML artifacts, pages, reports and mockups shown to the user (theme variables, type, spacing, phone layout, render checks).
---

# Snippet design

Use this look unless the user wants their own.

## Theme variables and fallbacks

- Background `--sn-bg` #121110; surfaces `--sn-surface` #1B1917, `--sn-surface-2` #221F1B, `--sn-surface-3` #2A2621.
- Text `--sn-text` #EDE7DD, `--sn-text-2` #CFC8BC, `--sn-muted` #A39C90, `--sn-faint` #7A7368.
- Borders `--sn-border` #242120, `--sn-border-strong` #302C27.
- Accent `--sn-accent` #9DB0FF with `--sn-accent-text` #10142A and `--sn-accent-bg`.
- Status `--sn-ok` #7FB88F, `--sn-warn` #D4923C, `--sn-danger` #E5745F.
- `--sn-radius` 12px; `--sn-font` Geist; `--sn-mono` JetBrains Mono.

Pages render on white unless they set their own colours.

## Character

- Quiet and dense: 13 to 15px text at 1.5 line height, headings a step larger at weight 500 to 600.
- Group by stepping surfaces; 1px borders only where surfaces meet; 10 to 14px corners; spacing in steps of 4; no shadows, gradients or glows.
- Accent sparingly, for the one primary action or highlight. Status colours on a 13% tint. Numbers, ids and code in mono.
- Charts take accent, ok, warn and danger, with border-colour gridlines and muted labels.
- Lead with the one thing the page must get across. Real content, short sentence-case labels, inline SVG icons rather than emoji, lists or tables over grids of identical cards.
- A custom look keeps the same discipline: colours and type as variables once, dark and light themes.

## Format

- Self-contained HTML: inline CSS and JS, libraries from cdnjs or jsdelivr. Markdown, SVG and CSV also present well.
- `display: "inline"` embeds short content in the conversation and follows content height; `display: "card"` suits long reports, dashboards or multi-view apps.
- Artifact-local assets are relative paths inside the artifact folder. A new version of a shared artifact reuses its `artifact_id`.

## Phone

Pages are opened on a phone as often as on a desktop. At 360px nothing scrolls sideways (wide tables scroll in their own container); 16px gutter, 40px targets, body text at least 13px, 4.5:1 contrast, semantic elements, alt text, calm motion under `prefers-reduced-motion`.

## Checking

Render at 390x844 and 1280x800 and look at both screenshots before presenting; fix visual defects and broken references, and say so if rendering was not possible.
