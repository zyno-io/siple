// Dev-only: captures review screenshots into .impeccable/review/ from a running
// `npm run preview` (http://127.0.0.1:4321). Not part of the build or CI.
//   node scripts/review-capture.mjs [platform]
//     platform: mac (default) | linux | linux-arm | linux-unknown. The Linux platforms capture the
//               #download section only, at desktop and mobile widths. Client Hints are faked
//               (x86 / arm / none); linux-unknown also sends a UA with no CPU in it.
import { chromium } from '@playwright/test';
const out = new URL('../.impeccable/review/', import.meta.url).pathname;
const base = 'http://127.0.0.1:4321/';
const platform = process.argv[2] || 'mac';
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
  await page.goto(base, { waitUntil: 'networkidle' });
  await page.evaluate(() => document.fonts.ready);
  if (s.full) await page.evaluate(async () => { for (let y = 0; y < document.body.scrollHeight; y += 600) { window.scrollTo(0, y); await new Promise((r) => setTimeout(r, 40)); } window.scrollTo(0, 0); });
  await page.waitForTimeout(s.motion ? 3500 : 500);
  const suffix = LINUX_UAS[platform] ? `-${platform}` : '';
  if (s.section) await page.locator('#download').screenshot({ path: `${out}${s.name}${suffix}.png` });
  else await page.screenshot({ path: `${out}${s.name}${suffix}.png`, fullPage: s.full });
  await ctx.close();
}
await browser.close();
console.log('captured', platform);
