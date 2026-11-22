// Dev-only: captures review screenshots into .impeccable/review/ from a running
// `npm run preview` (http://127.0.0.1:4321). Not part of the build or CI.
//   node scripts/review-capture.mjs [state] [platform]
//     state:    none (default) | candidate | release | error
//               | linux-new (a release with Linux AppImages and aarch64 files)
//               | linux-old (v0.2.1: tarball only, no AppImage, no aarch64)
//     platform: mac (default) | linux | linux-arm | linux-unknown. The Linux platforms capture the
//               #download section only, at desktop and mobile widths. Client Hints are faked
//               (x86 / arm / none); linux-unknown also sends a UA with no CPU in it.
import { chromium } from '@playwright/test';
const state = process.argv[2] || 'none';
const out = new URL('../.impeccable/review/', import.meta.url).pathname;
const base = 'http://127.0.0.1:4321/';
const asset = (v, s) => ({ name: `Siple-${v}-${s}`, browser_download_url: `https://github.com/zyno-io/siple/releases/download/v${v}/Siple-${v}-${s}` });
const LINUX_NEW = ['linux-x86_64.AppImage', 'linux-aarch64.AppImage', 'linux-x86_64.tar.gz', 'linux-aarch64.tar.gz'];
const rel = (v, pre, linux = ['linux-x86_64.tar.gz']) => ({
  tag_name: `v${v}`, name: `Siple ${v}`, draft: false, prerelease: pre, html_url: `https://github.com/zyno-io/siple/releases/tag/v${v}`,
  published_at: '2026-11-17T07:42:06Z',
  assets: ['macos.dmg', 'macos.zip', 'windows-x64.zip', 'windows-arm64.zip', ...linux, 'SHA256SUMS'].map((s) => s === 'SHA256SUMS' ? { name: 'SHA256SUMS', browser_download_url: `https://github.com/zyno-io/siple/releases/download/v${v}/SHA256SUMS` } : asset(v, s)),
});
const body = { none: '[]', candidate: JSON.stringify([rel('0.1.0-rc.5', true)]), release: JSON.stringify([rel('0.1.0', false), rel('0.1.0-rc.5', true)]),
  'linux-new': JSON.stringify([rel('0.3.0', false, LINUX_NEW), rel('0.2.1', false)]),
  'linux-old': JSON.stringify([rel('0.2.1', false)]) }[state];
const platform = process.argv[3] || 'mac';
const MAC_UA = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36';
const IPHONE_UA = 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1';
const LINUX_UAS = {
  linux: 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36',
  'linux-arm': 'Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36',
  'linux-unknown': 'Mozilla/5.0 (X11; Linux) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36',
};
const browser = await chromium.launch();
const shots = LINUX_UAS[platform] ? [
  { name: 'dl-desktop', w: 1440, h: 900, scheme: 'light', ua: LINUX_UAS[platform], section: true },
  { name: 'dl-desktop-dark', w: 1440, h: 900, scheme: 'dark', ua: LINUX_UAS[platform], section: true },
  { name: 'dl-mobile', w: 390, h: 844, scheme: 'light', ua: LINUX_UAS[platform], section: true, scale: 2 },
] : [
  { name: 'desktop', w: 1440, h: 900, full: true, scheme: 'light', ua: MAC_UA },
  { name: 'desktop-dark', w: 1440, h: 900, full: true, scheme: 'dark', ua: MAC_UA },
  { name: 'desktop-fold', w: 1440, h: 900, full: false, scheme: 'light', ua: MAC_UA, motion: true },
  { name: 'desktop-1280-fold', w: 1280, h: 800, full: false, scheme: 'light', ua: MAC_UA, motion: true },
  { name: 'mobile', w: 390, h: 844, full: true, scheme: 'light', ua: IPHONE_UA, mobile: true },
  { name: 'mobile-dark', w: 390, h: 844, full: true, scheme: 'dark', ua: IPHONE_UA, mobile: true },
  { name: 'tablet', w: 768, h: 1024, full: true, scheme: 'light', ua: MAC_UA },
];
for (const s of shots) {
  const ctx = await browser.newContext({ viewport: { width: s.w, height: s.h }, deviceScaleFactor: s.mobile || s.scale ? 2 : 1, userAgent: s.ua, isMobile: !!s.mobile, hasTouch: !!s.mobile, colorScheme: s.scheme, reducedMotion: s.motion ? 'no-preference' : 'reduce' });
  const page = await ctx.newPage();
  // Headless Chromium sends real (x86) hints that override the UA, so fake them per platform.
  const hintArch = { linux: 'x86', 'linux-arm': 'arm', 'linux-unknown': '' }[platform];
  if (hintArch !== undefined) await page.addInitScript((arch) => {
    Object.defineProperty(navigator, 'userAgentData', { configurable: true, value: arch === null ? undefined : { platform: 'Linux', mobile: false, getHighEntropyValues: async () => (arch ? { architecture: arch } : {}) } });
  }, hintArch);
  if (state === 'error') await page.route('https://api.github.com/**', (r) => r.fulfill({ status: 403, body: '{"message":"API rate limit exceeded"}' }));
  else await page.route('https://api.github.com/**', (r) => r.fulfill({ status: 200, contentType: 'application/json', body }));
  await page.goto(base, { waitUntil: 'networkidle' });
  await page.evaluate(() => document.fonts.ready);
  if (s.full) await page.evaluate(async () => { for (let y = 0; y < document.body.scrollHeight; y += 600) { window.scrollTo(0, y); await new Promise((r) => setTimeout(r, 40)); } window.scrollTo(0, 0); });
  await page.waitForTimeout(s.motion ? 3500 : 500);
  const suffix = (state === 'none' ? '' : `-${state}`) + (LINUX_UAS[platform] ? `-${platform}` : '');
  if (s.section) await page.locator('#download').screenshot({ path: `${out}${s.name}${suffix}.png` });
  else await page.screenshot({ path: `${out}${s.name}${suffix}.png`, fullPage: s.full });
  await ctx.close();
}
await browser.close();
console.log('captured', state, platform);
