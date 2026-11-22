/**
 * Composes the Mac App Store and Microsoft Store screenshot sets into store/ (not served).
 *
 *   npm run store-shots     (needs `npx playwright install chromium` once, and ImageMagick `magick`)
 *
 * Input:  store/src/<scene>_<theme>.png, the real app GUI snapshots at 2x (2480x1520), rendered by
 *         `cargo test -p siple --release store_screenshots -- --ignored` in the siple repo
 *         (it writes target/store-shots/). Copy them into store/src/ to refresh.
 * Output: store/mac/01..05-*.png      2880x1800, store/windows/01..05-*.png  3840x2160
 *         sRGB, no alpha. Each frame is a headline, a subhead and the unretouched app render in a
 *         window frame (macOS-style for the Mac set, Windows 11-style for the Windows set).
 * Style:  the site's light-theme tokens, Inter from @fontsource, scripts/siple-icon-1024.png and
 *         scripts/siple-mark-256.png (the small-size mark).
 */
import { chromium } from '@playwright/test';
import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const fontDir = join(root, 'node_modules/@fontsource/inter/files');
const b64 = (p) => readFileSync(p).toString('base64');
const iconSrc = `data:image/png;base64,${b64(join(root, 'scripts/siple-icon-1024.png'))}`;
const markSrc = `data:image/png;base64,${b64(join(root, 'scripts/siple-mark-256.png'))}`;
const TILE = { x: 100, size: 824 }; // the icon tile inside the 1024 canvas (see og.mjs)

const shots = [
  { n: '01-two-lines', scene: 'two_lines', theme: 'light', terminal: true,
    h: 'The scriptable SIP softphone.', s: 'Several lines, hold, mute and encrypted calls in one window.' },
  { n: '02-incoming-call', scene: 'incoming', theme: 'light',
    h: 'Never miss a call.', s: 'Answer, reject or silence an incoming call straight from its line.' },
  { n: '03-attended-transfer', scene: 'transfer_consult', theme: 'light',
    h: 'Attended transfer, right in the call.', s: 'Talk to the target first, then complete the transfer or cancel and go back.' },
  { n: '04-busy-lamps', scene: 'blf_list', theme: 'light',
    h: 'See who’s busy at a glance.', s: 'Busy lamps for every extension, grouped and sortable, with a ringing one ready to pick up.' },
  { n: '05-dark-theme', scene: 'two_lines', theme: 'dark',
    h: 'Light or dark, like your desktop.', s: 'The same window in a dark theme as well as a light one.' },
];

// Per platform: canvas size in CSS px (rendered at 2x), app window width, copy size.
const sets = {
  mac: { w: 1440, h: 900, win: 960, h1: 64, sub: 26, top: 50 },
  windows: { w: 1920, h: 1080, win: 1240, h1: 68, sub: 28, top: 52 },
};

const font = (w) =>
  `@font-face{font-family:Inter;font-weight:${w};src:url(data:font/woff2;base64,${b64(join(fontDir, `inter-latin-${w}-normal.woff2`))}) format('woff2')}`;

/** The app icon's tile at `px`; small sizes use the app's simplified mark (bigger handset, no lamp). */
const mark = (px) => {
  if (px <= 48) return `width:${px}px;height:${px}px;background:url(${markSrc}) no-repeat;background-size:${px}px`;
  const k = 1024 / TILE.size;
  return `width:${px}px;height:${px}px;background:url(${iconSrc}) no-repeat;background-size:${px * k}px;background-position:-${(TILE.x / TILE.size) * px}px -${(TILE.x / TILE.size) * px}px`;
};

const macBar = (dark) => `
<div class="bar mac ${dark ? 'dk' : ''}">
  <span class="tl"><i style="background:#ff5f57"></i><i style="background:#febc2e"></i><i style="background:#28c840"></i></span>
  <span class="ttl">Siple</span>
</div>`;

const winBar = (dark) => {
  const c = dark ? '#fff' : '#1b1b1b';
  return `
<div class="bar win ${dark ? 'dk' : ''}">
  <span class="ico" style="${mark(16)}"></span><span class="ttl">Siple</span>
  <span class="ctl">
    <b><svg width="10" height="10" viewBox="0 0 10 10"><path d="M0 5h10" stroke="${c}" stroke-width="1"/></svg></b>
    <b><svg width="10" height="10" viewBox="0 0 10 10"><rect x=".5" y=".5" width="9" height="9" fill="none" stroke="${c}" stroke-width="1"/></svg></b>
    <b><svg width="10" height="10" viewBox="0 0 10 10"><path d="M0 0l10 10M10 0L0 10" stroke="${c}" stroke-width="1"/></svg></b>
  </span>
</div>`;
};

const css = (set, kind) => `
${[400, 500, 600, 700].map(font).join('\n')}
:root{--bg:#f6f7f9;--surface:#fff;--border:#e3e5ea;--border-strong:#cfd3db;--text:#15171c;--text-2:#4f5562;--text-3:#646a78;
  --accent:#17769a;--accent-soft:rgba(23,118,154,.1);--mono:ui-monospace,'SF Mono','JetBrains Mono',Menlo,Consolas,monospace}
*{box-sizing:border-box;margin:0}
html,body{width:${set.w}px;height:${set.h}px;overflow:hidden}
body{font-family:Inter,sans-serif;background:var(--bg);color:var(--text);position:relative;isolation:isolate;-webkit-font-smoothing:antialiased}
body::before{content:'';position:absolute;inset:0;z-index:-1;
  background-image:linear-gradient(to right,var(--border) 1px,transparent 1px),linear-gradient(to bottom,var(--border) 1px,transparent 1px);
  background-size:32px 32px;-webkit-mask-image:radial-gradient(ellipse 70% 60% at 50% 0%,#000 20%,transparent 80%);opacity:.75}
body::after{content:'';position:absolute;inset:0;z-index:-1;background:radial-gradient(ellipse 60% 55% at 50% 0%,var(--accent-soft),transparent 70%)}
.head{position:absolute;left:0;right:0;top:${set.top}px;text-align:center;padding:0 48px}
h1{font-size:${set.h1}px;line-height:1.05;font-weight:700;letter-spacing:-.035em}
.sub{margin-top:14px;font-size:${set.sub}px;line-height:1.35;color:var(--text-2);font-weight:500}
.frame{position:absolute;left:${(set.w - set.win) / 2}px;bottom:${kind === 'mac' ? 78 : 40}px;width:${set.win}px;overflow:hidden;background:#eceef2;
  border-radius:${kind === 'mac' ? 12 : 8}px;
  box-shadow:0 0 0 1px rgba(16,24,40,.22),0 40px 80px -20px rgba(16,24,40,.35),0 6px 16px -6px rgba(16,24,40,.14)}
.frame img{display:block;width:100%}
.bar{display:flex;align-items:center;position:relative;color:#1b1b1b}
.bar.mac{height:30px;background:linear-gradient(#ebebeb,#dcdcdc);border-bottom:1px solid #c4c4c4;font-size:13px;font-weight:600;color:#4a4a4a}
.bar.mac.dk{background:linear-gradient(#3b3b3e,#2f2f32);border-bottom-color:#1c1c1e;color:#d6d6d8}
.tl{display:flex;gap:8px;padding-left:12px}.tl i{width:12px;height:12px;border-radius:50%;box-shadow:inset 0 0 0 .5px rgba(0,0,0,.22)}
.bar.mac .ttl{position:absolute;left:0;right:0;text-align:center}
.bar.win{height:34px;background:#f3f3f3;font-family:'Segoe UI',Inter,sans-serif;font-size:12px;border-bottom:1px solid #e0e0e0}
.bar.win.dk{background:#202020;color:#fff;border-bottom-color:#2d2d2d}
.bar.win .ico{margin-left:12px}.bar.win .ttl{margin-left:10px}
.ctl{margin-left:auto;display:flex;height:100%}.ctl b{width:46px;display:grid;place-items:center}
.term{position:absolute;left:${set.win * 0.045}px;bottom:${set.win * 0.045}px;width:${set.win * 0.44}px;border:1px solid var(--border-strong);border-radius:10px;
  background:var(--surface);box-shadow:0 24px 48px -12px rgba(16,24,40,.3);overflow:hidden;font-family:var(--mono)}
.tbar{height:30px;display:flex;align-items:center;padding:0 14px;border-bottom:1px solid var(--border);background:#f3f4f6;font-size:${set.w > 1500 ? 14 : 12}px;color:var(--text-3)}
pre{padding:12px 14px 14px;font-family:var(--mono);font-size:${set.w > 1500 ? 15 : 13}px;line-height:1.7;white-space:pre;overflow:hidden;color:var(--text)}
.ps{color:#067647}.o{color:var(--text-2)}
`;

const term = `<div class="term"><div class="tbar">siple-cli</div><pre><span class="ps">$ </span>siple-cli ctl call.dial '{"account":"desk-1001",
    "target":"1003"}'
<span class="o">{"call_id":"c1","call":{"state":"dialing",…}}</span>
<span class="ps">$ </span>siple-cli ctl call.mute '{"call_id":"c1"}'</pre></div>`;

const page = (kind, set, shot) => {
  const dark = shot.theme === 'dark';
  const img = `data:image/png;base64,${b64(join(root, 'store/src', `${shot.scene}_${shot.theme}.png`))}`;
  return `<!doctype html><html><head><meta charset="utf-8"><style>${css(set, kind)}</style></head><body>
<div class="head"><h1>${shot.h}</h1><p class="sub">${shot.s}</p></div>
<div class="frame">${kind === 'mac' ? macBar(dark) : winBar(dark)}<img src="${img}" alt="">${shot.terminal ? term : ''}</div>
</body></html>`;
};

const PROMPT = (scene) =>
  `Not generated by an image model: rendered by scripts/store-shots.mjs with Playwright from the site's tokens, Inter, the Siple app icon and the real ${scene} GUI snapshot (2x).`;

const browser = await chromium.launch();
try {
  for (const [kind, set] of Object.entries(sets)) {
    mkdirSync(join(root, 'store', kind), { recursive: true });
    const ctx = await browser.newContext({ viewport: { width: set.w, height: set.h }, deviceScaleFactor: 2, colorScheme: 'light' });
    const tab = await ctx.newPage();
    for (const shot of shots) {
      await tab.setContent(page(kind, set, shot), { waitUntil: 'load' });
      await tab.evaluate(() => document.fonts.ready);
      const out = join(root, 'store', kind, `${shot.n}.png`);
      const tmp = `${out}.tmp.png`;
      await tab.screenshot({ path: tmp, type: 'png' });
      // Flatten: sRGB, 8-bit, no alpha channel.
      execFileSync('magick', [tmp, '-background', 'white', '-alpha', 'remove', '-alpha', 'off', '-colorspace', 'sRGB',
        '-strip', '-define', 'png:color-type=2', '-depth', '8', out]);
      execFileSync('rm', [tmp]);
      const embed = join(process.env.HOME, '.claude/skills/impeccable/scripts/impeccable');
      try {
        execFileSync(embed, ['embed-prompt', out, '--prompt', PROMPT(`${shot.scene} ${shot.theme}`)], { stdio: 'ignore' });
      } catch {
        /* provenance tool not installed (optional) */
      }
      console.log(`wrote store/${kind}/${shot.n}.png`);
    }
    await ctx.close();
  }
} finally {
  await browser.close();
}
