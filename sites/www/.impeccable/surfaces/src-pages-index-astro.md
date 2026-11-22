---
version: 1
slug: "src-pages-index-astro"
primary_target: "src/pages/index.astro"
related_targets: []
---

# Surface: Siple home page (`/`)

Mode: **Persuade**. Scope: the single marketing page for Siple (plus 404). World: pinned by the
owner, the PixelCI design system ("The Review Desk") adapted to Siple's identity (teal-blue accent
from the app icon and the app's own theme.rs; the app's round status lamps).

Audience: desk users / IT admins with a SIP account, and builders running scripts or AI agents.
Action: download the right build for this computer (working, release-aware), or follow to GitHub
while no release is published. Proof: the app's real rendered screenshots, real `siple-cli`
commands, real config keys. Constraints: no invented numbers, testimonials or store listings;
"source-available", never "open source"; ZynoTalk promo is a sibling, never a requirement.

Decided unattended (subagent run, no question channel): surface round dealt structures 7, 6, 5
from the grounded list; the dealt lead (7, "the config file is the page") is built. Code-led: the
brief pins the world and requires real screenshots rather than generated imagery, and no one was
present to approve comps.

Grounded structures, by resonance: 1 two hands on one call (GUI + terminal side by side);
2 the app is the page; 3 one call's lifecycle as the scroll; 4 release desk first; 5 BLF lamp
field as the section index; 6 the `siple-cli events` stream as the page; 7 the config file is the
page. Dealt: 7 (lead), 6, 5.

Challengers (identity is fixed at surface scope, so all declined; disciplines kept):
ANSI BBS nightboard: declined; kept "presence lands as a live event": the release status is a
lamp that resolves from the live GitHub API. Nixie counter: declined; kept fixed-position tabular
numerals for version and date. Game Boy field: declined; kept "one marker always shows where to go
next": exactly one featured download row. Sampler, neon circuit, alphabet storm: declined, no
transferable discipline beyond the above.

## Direction contract

THESIS: Siple keeps everything in one `config.toml` that the GUI, the API and the user all edit
live. The page is that file: each feature section is one real config table, and one line of it
(`[ui] theme`) is live on the page. Refuses the category default of a headline over a feature-card
grid with a fake device mockup.

OWN-WORLD: Review Desk: cool grey desk (#f6f7f9 / #0c0d10), white hairline cards, 12px radius,
resting shadows, Inter for people's words, monospace only for machine text (table names, keys,
commands, filenames, versions). One accent: Siple teal-blue (#17769A light / #3FB2D4 dark, dark
ink on it), used for the one primary action, selection and focus. Status as tag triplets; the
app's round lamp (LED dot with halo) for live state. Real screenshots on a Screenshot Mat.

STORY: understand (a native SIP softphone with real desk-phone features) → believe (the real app,
real transfers and lamps; agents drive the same calls with real commands; any PBX) → act
(download the detected build, or GitHub while unreleased) → trust (signed builds, checksums,
plain licence) → sibling (ZynoTalk if you need a line).

FIRST VIEWPORT (1440x900): sticky 60px header (icon + Siple wordmark, Features, AI agents,
Licence, Docs, GitHub, theme, Download). Left-aligned H1 across ~8 of 12 columns, lede beneath,
then the download console in one row: primary button for the detected platform (label names the
OS), "All platforms" and GitHub secondary, and a lamp line with release state (version + date,
or "First release coming soon"). Beside the copy (right ~540px) a code-rendered terminal card
showing the same call driven by `siple-cli ctl`, sitting on the top edge of the real two-lines
screenshot (~1116px on the faint desk grid, lower part cut by the fold), overlapping it by
~12px so the app's own header stays readable. Revised during build: docking it at the window's
lower right put it below the fold at 1440x900, losing the mechanism from the first viewport.
The `config.toml` outline is a sticky horizontal band under the header at every width, not a
side rail: a 200px rail would take width from the screenshots (the system's "Screenshot Gets the
Width Rule"). Each config-table section also shows a real excerpt of its table.

FORM: grounded structure 7 of 7 (dealt lead): "the config file is the page". seed:c37dd8ce117a43ae.
Signature interaction: the `[ui] theme = "system" | "light" | "dark"` control is a live config line:
choosing a value re-themes the page and swaps every screenshot to the app's own light or dark
render, exactly as Siple hot-reloads. Second interaction: the `[calls]` section's state list swaps
real screenshots (two lines, incoming, attended transfer, busy lamps). Motion grammar: one
orchestrated hero settle (screenshot rises, terminal lines type in once), cross-fades on swaps,
everything static under reduced motion.

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
