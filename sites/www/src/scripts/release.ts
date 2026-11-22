/**
 * Download buttons that know the latest release and the visitor's computer.
 *
 * The HTML ships working without this: every link points at GitHub Releases
 * and the copy says builds are published there. This script then:
 *  1. reads https://api.github.com/repos/zyno-io/siple/releases (unauthenticated,
 *     cached for 10 minutes per tab so a reload doesn't spend the 60/hour limit),
 *  2. ignores drafts, prefers the newest published non-prerelease, else the
 *     newest prerelease (labelled "Release candidate"),
 *  3. maps assets by suffix (Siple-<version>-macos.dmg, -windows-x64.zip, ...),
 *  4. detects the OS and CPU (Windows and Linux on ARM through User-Agent
 *     Client Hints, then the UA string) and features that build. Linux gets the
 *     AppImage of its architecture, with the tarball as the alternative; a
 *     release without the AppImage (older ones) falls back to the tarball, and
 *     one without aarch64 assets hides the ARM row,
 *  5. on no published release, a failed request or a rate limit, keeps the
 *     GitHub links and says so plainly.
 */
import { platforms, releaseApi, site, type Platform, type PlatformId } from '../data/site';

interface Asset {
  name: string;
  browser_download_url: string;
}
interface Release {
  tag_name: string;
  name: string | null;
  draft: boolean;
  prerelease: boolean;
  html_url: string;
  published_at: string | null;
  assets: Asset[];
}
type Found = { kind: 'release' | 'candidate'; release: Release } | { kind: 'none' } | { kind: 'error' };
type Detected = PlatformId | 'mobile' | null;

/** False when a Linux visitor's CPU could not be told, so the ARM builds are pointed out too. */
let archKnown = true;

const CACHE_KEY = 'siple-releases';
const CACHE_MS = 10 * 60 * 1000;

async function fetchReleases(): Promise<Release[]> {
  try {
    const hit = JSON.parse(sessionStorage.getItem(CACHE_KEY) || 'null');
    if (hit && Date.now() - hit.at < CACHE_MS && Array.isArray(hit.data)) return hit.data;
  } catch {
    /* no cache */
  }
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), 8000);
  try {
    const res = await fetch(releaseApi, { headers: { Accept: 'application/vnd.github+json' }, signal: ctl.signal });
    if (!res.ok) throw new Error(`GitHub API ${res.status}`);
    const data = (await res.json()) as Release[];
    if (!Array.isArray(data)) throw new Error('unexpected response');
    try {
      sessionStorage.setItem(CACHE_KEY, JSON.stringify({ at: Date.now(), data }));
    } catch {
      /* storage blocked */
    }
    return data;
  } finally {
    clearTimeout(timer);
  }
}

function pick(list: Release[]): Found {
  const published = list.filter((r) => !r.draft);
  const stable = published.find((r) => !r.prerelease);
  if (stable) return { kind: 'release', release: stable };
  const rc = published.find((r) => r.prerelease);
  if (rc) return { kind: 'candidate', release: rc };
  return { kind: 'none' };
}

async function detect(): Promise<Detected> {
  const nav = navigator as Navigator & {
    userAgentData?: {
      platform?: string;
      mobile?: boolean;
      getHighEntropyValues?: (hints: string[]) => Promise<{ architecture?: string }>;
    };
  };
  const ua = nav.userAgent || '';
  const plat = nav.userAgentData?.platform || nav.platform || '';
  if (nav.userAgentData?.mobile || /Android|iPhone|iPad|iPod/i.test(ua)) return 'mobile';
  if (/Mac/i.test(plat) || /Macintosh/.test(ua)) {
    // iPadOS asks for the desktop site and says "Macintosh"; it has touch.
    return nav.maxTouchPoints > 1 ? 'mobile' : 'mac';
  }
  // CPU architecture from Client Hints where the browser has them ('arm' | 'x86');
  // Chrome freezes its UA string at "x86_64" on Linux, so the hint comes first.
  let arch = '';
  try {
    arch = (await nav.userAgentData?.getHighEntropyValues?.(['architecture']))?.architecture || '';
  } catch {
    /* hints refused: fall back to the UA string */
  }
  if (/Win/i.test(plat) || /Windows/.test(ua)) {
    // Unknown: assume x64, which also runs on ARM under emulation.
    return arch === 'arm' || (!arch && /ARM64|aarch64/i.test(ua)) ? 'win-arm64' : 'win-x64';
  }
  if (/CrOS/.test(ua)) return null;
  if (/Linux|X11/i.test(plat) || /Linux|X11/.test(ua)) {
    if (arch === 'arm' || (!arch && /aarch64|arm64|armv8/i.test(plat + ua))) return 'linux-arm64';
    // 32-bit ARM has no build.
    if (!arch && /\barm/i.test(plat + ua)) return null;
    // x86, or unknown (x86_64 is the default; the aarch64 row is listed too).
    archKnown = arch === 'x86' || /x86_64|amd64|x64/i.test(plat + ua);
    return 'linux';
  }
  return null;
}

const dateFmt = new Intl.DateTimeFormat('en-GB', { day: 'numeric', month: 'short', year: 'numeric' });
const version = (r: Release) => r.tag_name.replace(/^v/, '');
const each = <T extends Element>(sel: string, fn: (el: T) => void) => document.querySelectorAll<T>(sel).forEach(fn);
const setText = (sel: string, text: string) => each<HTMLElement>(sel, (el) => (el.textContent = text));

function render(found: Found, detected: Detected) {
  const release = found.kind === 'release' || found.kind === 'candidate' ? found.release : undefined;
  const assetFor = (suffix: string) => release?.assets.find((a) => a.name.endsWith(`-${suffix}`));
  /** The download for a platform: its preferred build, else its alternative (Linux: AppImage, else tarball). */
  const resolve = (p: Platform) => {
    const main = assetFor(p.suffix);
    const alt = p.alt && assetFor(p.alt.suffix);
    return main ? { asset: main, alt, fellBack: false } : { asset: alt, alt: undefined, fellBack: !!alt };
  };
  // A release without aarch64 assets has nothing for an ARM Linux visitor: as before, no featured build.
  const mine: Detected =
    detected === 'linux-arm64' && release && !resolve(platforms.find((p) => p.id === detected)!).asset ? null : detected;
  const featured = mine && mine !== 'mobile' ? platforms.find((p) => p.id === mine) : undefined;
  const featuredAsset = featured && resolve(featured).asset;

  document.documentElement.dataset.release = found.kind;
  if (mine) document.documentElement.dataset.platform = mine;

  // Card head and status lines.
  if (release) {
    const v = version(release);
    const when = release.published_at ? dateFmt.format(new Date(release.published_at)) : '';
    setText('[data-dl-version]', `Siple ${v}`);
    setText('[data-dl-date]', when ? `Published ${when}` : '');
    each<HTMLElement>('[data-dl-date]', (el) => {
      if (release.published_at) el.setAttribute('datetime', release.published_at);
    });
    each<HTMLAnchorElement>('[data-dl-notes]', (a) => (a.href = release.html_url));
    const sums = release.assets.find((a) => a.name === 'SHA256SUMS');
    each<HTMLAnchorElement>('[data-dl-sums]', (a) => (a.href = sums ? sums.browser_download_url : release.html_url));
    setText(
      '[data-dl-status-text]',
      found.kind === 'candidate'
        ? `Release candidate ${v}${when ? ` · ${when}` : ''}`
        : `Siple ${v}${when ? ` · ${when}` : ''}`,
    );
  } else if (found.kind === 'none') {
    setText('[data-dl-status-text]', 'First release coming soon. Signed builds will appear on GitHub Releases.');
  } else {
    setText('[data-dl-status-text]', 'Couldn’t reach GitHub just now. Every build is on GitHub Releases.');
  }

  // Platform rows.
  for (const p of platforms) {
    const { asset, alt, fellBack } = resolve(p);
    each<HTMLElement>(`[data-dl-row="${p.id}"]`, (row) => {
      row.classList.toggle('is-mine', p.id === mine);
      // Older releases have no aarch64 files: list x86_64 only.
      row.hidden = !!p.hideIfAbsent && !!release && !asset;
      const link = row.querySelector<HTMLAnchorElement>('[data-dl-link]');
      const file = row.querySelector<HTMLElement>('[data-dl-file]');
      const label = row.querySelector<HTMLElement>('[data-dl-link-label]');
      const note = row.querySelector<HTMLElement>('[data-dl-note]');
      const altLink = row.querySelector<HTMLAnchorElement>('[data-dl-alt]');
      if (asset) {
        if (link) {
          link.href = asset.browser_download_url;
          link.setAttribute('download', '');
          link.setAttribute('aria-label', `Download ${asset.name}`);
        }
        if (file) {
          file.textContent = asset.name;
          file.classList.add('is-file');
        }
        if (label) label.textContent = 'Download';
      } else if (release) {
        if (link) link.href = release.html_url;
        if (file) file.textContent = `Not in ${version(release)} yet`;
        if (label) label.textContent = 'On GitHub';
      }
      if (note) {
        note.textContent = fellBack
          ? `${version(release!)} has no AppImage, so this is the tarball: unpack it and run bin/siple.`
          : note.dataset.default || '';
      }
      if (altLink && p.alt) {
        altLink.hidden = !alt;
        if (alt) {
          altLink.href = alt.browser_download_url;
          altLink.setAttribute('download', '');
          altLink.setAttribute('aria-label', `Download ${alt.name}`);
          altLink.title = alt.name;
          const l = altLink.querySelector<HTMLElement>('[data-dl-alt-label]');
          if (l) l.textContent = `${p.alt.label}: ${alt.name}`;
        }
      }
    });
  }

  // The primary buttons (hero console, download section).
  each<HTMLAnchorElement>('[data-dl-primary]', (a) => {
    const label = a.querySelector<HTMLElement>('[data-dl-label]');
    const icon = a.querySelector<HTMLElement>('[data-dl-icon]');
    if (mine === 'mobile') {
      a.href = '#download';
      if (label) label.textContent = 'Desktop downloads';
      return;
    }
    if (featured && featuredAsset) {
      a.href = featuredAsset.browser_download_url;
      a.setAttribute('download', '');
      if (label) label.textContent = `Download for ${featured.short}`;
      if (icon) icon.className = `fa-brands ${featured.icon}`;
    } else if (release) {
      a.href = release.html_url;
      if (label) label.textContent = 'Download from GitHub';
    } else if (found.kind === 'none') {
      a.href = site.releases;
      if (label) label.textContent = 'Follow releases on GitHub';
      if (icon) icon.className = 'fa-brands fa-github';
    }
  });
  each<HTMLElement>('[data-dl-primary-note]', (el) => {
    if (mine === 'mobile') el.textContent = 'Siple is a desktop app. Open this page on your Mac or PC, or pick a build below.';
    else if (featured && featuredAsset) {
      el.textContent = `${featured.requires.split(' · ')[0]} · ${featuredAsset.name}`;
      // CPU unknown: x86_64 is featured, so point out the ARM builds.
      if (mine === 'linux' && !archKnown && (assetFor('linux-aarch64.AppImage') || assetFor('linux-aarch64.tar.gz')))
        el.textContent += ' · aarch64 builds are listed below';
    } else el.textContent = '';
    el.hidden = !el.textContent;
  });
}

(async () => {
  const minePromise = detect().catch(() => null);
  let found: Found;
  try {
    found = pick(await fetchReleases());
  } catch {
    found = { kind: 'error' };
  }
  render(found, await minePromise);
})();
