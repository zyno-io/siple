/**
 * Features the download for the visitor's computer. Every link, file name and version is in the HTML already, fixed
 * at build time from the CHANGELOG (src/data/release.ts); nothing is fetched. This script only:
 *  1. detects the OS and CPU (Windows and Linux on ARM through User-Agent Client Hints, then the UA string),
 *  2. marks that platform's row in the download list and points the hero button at its baked link,
 *  3. fills the note under the button (requirement and file name).
 * Without JS, on a phone, or on an unknown OS, the primary button leads to the download list.
 */
import { platforms, type PlatformId } from '../data/site';

type Detected = PlatformId | 'mobile' | null;

/** False when a Linux visitor's CPU could not be told, so the ARM builds are pointed out too. */
let archKnown = true;

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

const each = <T extends Element>(sel: string, fn: (el: T) => void) => document.querySelectorAll<T>(sel).forEach(fn);
const rowOf = (id: PlatformId) => document.querySelector<HTMLElement>(`[data-dl-row="${id}"]`);
/** The build-time download URL of a platform, from its row in the download list. */
const urlOf = (id: PlatformId) => rowOf(id)?.querySelector<HTMLAnchorElement>('[data-dl-link]')?.href;
const fileOf = (id: PlatformId) => rowOf(id)?.querySelector<HTMLElement>('.dl-row__file')?.textContent?.trim();

function render(mine: Detected) {
  const featured = mine && mine !== 'mobile' ? platforms.find((p) => p.id === mine) : undefined;
  const href = featured && urlOf(featured.id);
  const file = featured && fileOf(featured.id);
  if (featured) document.documentElement.dataset.platform = featured.id;
  else if (mine === 'mobile') document.documentElement.dataset.platform = 'mobile';
  each<HTMLElement>('[data-dl-row]', (row) => row.classList.toggle('is-mine', row.dataset.dlRow === featured?.id));

  // The primary buttons (hero console, download section).
  each<HTMLAnchorElement>('[data-dl-primary]', (a) => {
    const label = a.querySelector<HTMLElement>('[data-dl-label]');
    const icon = a.querySelector<HTMLElement>('[data-dl-icon]');
    if (mine === 'mobile') {
      if (label) label.textContent = 'Desktop downloads';
    } else if (featured && href) {
      a.href = href;
      a.setAttribute('download', '');
      if (label) label.textContent = `Download for ${featured.short}`;
      if (icon) icon.className = `fa-brands ${featured.icon}`;
    }
  });
  each<HTMLElement>('[data-dl-primary-note]', (el) => {
    if (mine === 'mobile') el.textContent = 'Siple is a desktop app. Open this page on your Mac or PC, or pick a build below.';
    else if (featured && href && file) {
      el.textContent = `${featured.requires.split(' · ')[0]} · ${file}`;
      // CPU unknown: x86_64 is featured, so point out the ARM builds.
      if (mine === 'linux' && !archKnown) el.textContent += ' · aarch64 builds are listed below';
    } else el.textContent = '';
    el.hidden = !el.textContent;
  });
}

detect()
  .catch(() => null)
  .then(render);
