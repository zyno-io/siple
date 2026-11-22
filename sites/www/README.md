# Siple marketing site

The public site for **Siple**, the scriptable SIP softphone. A static [Astro](https://astro.build) site built
the same way as `pixelci-sales-site` (Astro, npm, Playwright VRT, CI → ZynoHosting; here GitHub Actions), in the PixelCI
design system ("The Review Desk") with Siple's own accent. See `DESIGN.md` and `PRODUCT.md`.

```bash
npm install
npm run dev      # http://localhost:4321
npm run build    # static build → dist/
npm run preview  # serve dist/
npm run vrt      # build, then capture screenshots/ (local only, nothing is uploaded)
npm run og       # regenerate public/og/home.png and the icons
npm run shots    # re-import app screenshots from crates/siple/tests/snapshots
```

## Pages

| Route | Notes |
| --- | --- |
| `/` | Hero with the download console and the real app + `siple-cli`; the "config file" body (`[calls]`, `[control]`, `[speech]`, `[[account]]`, `[ui]`, outlined by a sticky `config.toml` bar); downloads; licence; ZynoTalk promo |
| `/404` | Not found (`noindex`) |
| `/sitemap.xml` | Hand-listed (`src/pages/sitemap.xml.ts`); add new pages there |

## Content sources — read before editing copy

- **Product claims** come only from the Siple repo: `README.md`, `CHANGELOG.md`, `docs/` (CONFIG.md, API.md,
  RELEASING.md), `sites/docs/` and `skills/siple/SKILL.md`. Commands, method names, config keys and the
  `methods | grep | cut` output in the agents terminal are real. No user counts, benchmarks, testimonials,
  prices or store availability.
- **Licence wording**: Siple is *source-available* under FSL-1.1-ALv2. Never "open source" or "free software".
- **ZynoTalk promo** (`src/components/ZynoTalkPromo.astro`) uses only claims on ZynoTalk's own site
  (`../zynotalk-web/src`) and never implies ZynoTalk is required.
- **Screenshots** in `public/shots/` are the app's own GUI snapshot goldens
  (`crates/siple/tests/snapshots/`), re-encoded by `scripts/shots.sh`, never retouched. Re-import them when the
  app's UI changes.

## Downloads and release state

The page makes no request to GitHub's API. The version and date are read at build time from the newest released
section of the root `CHANGELOG.md` (`src/data/release.ts`; headings look like `## 0.2.2 (2026-11-22)`, and
`## Unreleased` is skipped). A malformed newest heading fails the build rather than falling back to an older
version. Set `SIPLE_CHANGELOG` to read a different file (resolved from the working directory).

Every asset URL is baked into the HTML from that version: `Siple-<version>-<suffix>` for each entry in `platforms`
(`src/data/site.ts`: `-macos.dmg`, `-windows-x64.zip`, `-windows-arm64.zip`, `-linux-x86_64.AppImage`,
`-linux-aarch64.AppImage`, plus the Linux `.tar.gz` as each row's "Tarball" link) under
`https://github.com/zyno-io/siple/releases/download/v<version>/`, with the release notes at `.../releases/tag/v<version>`
and `SHA256SUMS` beside the assets. Production is built from a published release's tag and deployed only after
every download link checks live; staging builds from `main` and may name a version that isn't released yet.

`src/scripts/download.ts` only detects the visitor's OS (and the CPU for Windows and Linux on ARM, from
`navigator.userAgentData.getHighEntropyValues(['architecture'])` first, then the UA string), marks that row, points
the hero button at the row's baked link and fills the note under it. Without JS, on a phone or tablet, or on an
unknown OS, the button goes to `#download`. When the Linux CPU can't be told, x86_64 is featured and the hero note
points at the aarch64 builds in the list. The AppImage, tarball and requirement copy comes from Siple's
`sites/docs/guide/install.md`.

Review captures: `npm run preview`, then `node scripts/review-capture.mjs <platform>` (see the header of the
script; platforms `mac`, `linux`, `linux-arm`, `linux-unknown`).

Platform names, requirements and signing notes live in `platforms` in `src/data/site.ts`.

## App store buttons

`stores` in `src/data/site.ts`: Mac App Store and Microsoft Store. Flathub is not listed: the submission is deferred
(Siple `docs/STORE.md`). Both are `available: false`, rendered as disabled, dashed "Coming soon" buttons
in the design system (deliberately not Apple's or Microsoft's badge artwork). At launch set `available: true`
and `url`; to use the official badge, add the artwork to `public/badges/` and set `badge` (and `badgeAlt`).

## Search and social previews

`BaseLayout` sets the canonical URL, Open Graph and Twitter tags, icons and JSON-LD (Organization; the home page
adds WebSite and SoftwareApplication from `src/data/seo.ts`). `scripts/og.mjs` renders `public/og/home.png`
(1200×630) and the icons from `scripts/siple-icon-1024.png` (a copy of the app icon). Rerun and commit when the
headline or the icon changes.

Every raster in `public/` carries its provenance (origin) in its metadata; `npm run shots` / `npm run og`
rewrite the files, so re-embed it afterwards (`impeccable embed-prompt --scan public` lists what is missing).

## Store screenshots

`npm run store-shots` composes the Mac App Store (`store/mac/`, 2880x1800) and Microsoft Store
(`store/windows/`, 3840x2160) sets: headline, subhead and the real app render in a platform-style
window frame. PNG, sRGB, no alpha; not served by the site. Needs Playwright chromium and `magick`.
The app renders in `store/src/` come from `cargo test -p siple --release store_screenshots -- --ignored`
in the siple repo (writes `target/store-shots/`); copy them here to refresh.

## Deploy

The domain is `siple.app`; it is named in `astro.config.mjs`, `public/robots.txt`, `src/data/seo.ts` and
`.github/workflows/www.yml`.

The `Website` GitHub Actions workflow (`.github/workflows/www.yml`) builds `dist/` and publishes it with the
ZynoHosting CLI:

- `www-staging.siple.app` (alpha cluster, `--environment alpha`) deploys automatically on every push to `main`.
- `siple.app` (production) deploys when a stable GitHub release is **published** (`release.yml` uploads a draft;
  publishing it fires the run), built from the release's tag. Release candidates (`v0.3.0-rc.1`) never deploy it.
  Manually (e.g. a copy fix between releases): Actions > Website > Run workflow from `main` or a stable tag
  (`v1.2.3`), target `production`. A tag build fails if the newest CHANGELOG version isn't the tag, and every
  production deploy stops if any download link isn't live yet (so a run from `main` during a release-candidate
  cycle, when the CHANGELOG already names the coming version, deploys nothing).
- Pull requests only build.

Each deploy job reads a per-site **deploy token** (`ZYNOHOSTING_DEPLOY_TOKEN`) from its GitHub environment
(`www-staging`, `www-production`). Create or rotate one with
`zynohosting deploy-tokens create <site> --production --github zyno-io/siple --github-environment <env>`
(plus `--environment alpha` for staging); the CLI stores it as the environment secret without printing it.

The deploy jobs install the CLI from `deploy/package-lock.json` (`npm ci --ignore-scripts`, Node 24 per
`deploy/.nvmrc`), so its whole dependency tree is pinned. To upgrade, bump the version in `deploy/package.json`
and run `npm install --package-lock-only --ignore-scripts` there.

VRT captures (`npm run vrt`) run locally only.
