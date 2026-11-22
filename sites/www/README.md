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

`src/scripts/release.ts` reads `https://api.github.com/repos/zyno-io/siple/releases?per_page=10` in the browser
(cached 10 minutes per tab), ignores drafts, prefers the newest published non-prerelease and otherwise the newest
prerelease (shown as "Release candidate"). Assets are matched by suffix (`-macos.dmg`, `-windows-x64.zip`,
`-windows-arm64.zip`, `-linux-x86_64.AppImage`, `-linux-x86_64.tar.gz`, `-linux-aarch64.AppImage`,
`-linux-aarch64.tar.gz`, plus `SHA256SUMS`). The visitor's OS is detected from the user agent, and the CPU
(Windows and Linux on ARM) from `navigator.userAgentData.getHighEntropyValues(['architecture'])` first, then the UA
string; phones and tablets get the full list instead of a featured build.

Linux has two rows, "Linux x86_64" and "Linux on ARM". Each features the AppImage of its architecture with the
tarball as the alternative link. A release without the AppImage (up to v0.2.1) falls back to the tarball as the
primary download, with a note saying so; a release without any aarch64 asset hides the ARM row and an ARM Linux
visitor gets no featured build (the GitHub link), as before. When the CPU can't be told, x86_64 is featured and the
hero note points at the aarch64 builds in the list. The AppImage, tarball and requirement copy comes from Siple's
`sites/docs/guide/install.md`.

Review captures: `npm run preview`, then `node scripts/review-capture.mjs <state> <platform>` (see the header of the
script; `linux-new` and `linux-old` are release fixtures with and without AppImages, platforms `linux`, `linux-arm`,
`linux-unknown`).

The HTML ships pointing every button at `https://github.com/zyno-io/siple/releases`, so it works without JS.
The script sets `<html data-release="release|candidate|none|error">`; elements with `data-when="…"` show only
in the listed states (`pending` = before the script answers). Today every release is a draft, so the page shows
"First release coming soon".

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
- `siple.app` (production) deploys on every release tag (`v*`, built from the tagged commit), or manually:
  Actions > Website > Run workflow on `main`, target `production`.
- Pull requests only build.

Each deploy job reads a per-site **deploy token** (`ZYNOHOSTING_DEPLOY_TOKEN`) from its GitHub environment
(`www-staging`, `www-production`). Create or rotate one with
`zynohosting deploy-tokens create <site> --production --github zyno-io/siple --github-environment <env>`
(plus `--environment alpha` for staging); the CLI stores it as the environment secret without printing it.

The deploy jobs install the CLI from `deploy/package-lock.json` (`npm ci --ignore-scripts`, Node 24 per
`deploy/.nvmrc`), so its whole dependency tree is pinned. To upgrade, bump the version in `deploy/package.json`
and run `npm install --package-lock-only --ignore-scripts` there.

VRT captures (`npm run vrt`) run locally only.
