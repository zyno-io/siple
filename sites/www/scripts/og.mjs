/**
 * Renders the social preview image and the icons into public/.
 *
 *   npm run og        (after `npm run shots`; needs `npx playwright install chromium` once)
 *
 * Output (commit it; the build does not run this):
 *   public/og/home.png            1200×630 Open Graph / Twitter card
 *   public/icon-64.png            the header/footer brand mark (2x of 32px)
 *   public/icon-192.png, public/apple-touch-icon.png (180×180)
 *   public/favicon-32.png, public/favicon.ico
 *
 * Icons come from Siple's own app icon (scripts/siple-icon-1024.png, a copy of
 * siple packaging/icons/siple-1024.png), cropped to its tile; the 64 and 32 px
 * ones use the app's simplified small-size mark (scripts/siple-mark-256.png, a
 * copy of siple packaging/icons/siple-mark-256.png: bigger handset, no lamp). The card is drawn with the site's tokens (light theme), Inter
 * from @fontsource, and the app's real screenshot (public/shots/).
 */
import { chromium } from '@playwright/test';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const pub = join(root, 'public');
const fontDir = join(root, 'node_modules/@fontsource/inter/files');
const b64 = (p) => readFileSync(p).toString('base64');

const iconSrc = `data:image/png;base64,${b64(join(root, 'scripts/siple-icon-1024.png'))}`;
const markSrc = `data:image/png;base64,${b64(join(root, 'scripts/siple-mark-256.png'))}`;
const shotSrc = `data:image/webp;base64,${b64(join(pub, 'shots/two_lines-light.webp'))}`;

// The app icon's tile spans x/y 100..924 of 1024 (Apple's icon grid; the drop shadow lies outside it).
const TILE = { x: 100, size: 824 };

const font = (w) =>
  `@font-face{font-family:Inter;font-weight:${w};src:url(data:font/woff2;base64,${b64(join(fontDir, `inter-latin-${w}-normal.woff2`))}) format('woff2')}`;

const css = `
${[400, 500, 600, 700].map(font).join('\n')}
:root{--bg:#f6f7f9;--surface:#fff;--border:#e3e5ea;--border-strong:#cfd3db;--text:#15171c;--text-2:#4f5562;--text-3:#646a78;
  --accent:#17769a;--accent-soft:rgba(23,118,154,.1);--mono:ui-monospace,'SF Mono','JetBrains Mono',Menlo,Consolas,monospace}
*{box-sizing:border-box;margin:0}
html,body{width:1200px;height:630px;overflow:hidden}
body{font-family:Inter,sans-serif;background:var(--bg);color:var(--text);position:relative;isolation:isolate;-webkit-font-smoothing:antialiased}
body::before{content:'';position:absolute;inset:0;z-index:-1;
  background-image:linear-gradient(to right,var(--border) 1px,transparent 1px),linear-gradient(to bottom,var(--border) 1px,transparent 1px);
  background-size:28px 28px;-webkit-mask-image:radial-gradient(ellipse 80% 75% at 25% 0%,#000 25%,transparent 80%);opacity:.75}
body::after{content:'';position:absolute;inset:0;z-index:-1;background:radial-gradient(ellipse 55% 70% at 20% 0%,var(--accent-soft),transparent 70%)}
.copy{position:absolute;left:72px;top:64px;bottom:56px;width:520px;display:flex;flex-direction:column}
.brand{display:flex;align-items:center;gap:14px}
.mark{width:52px;height:52px;background:url(${iconSrc}) no-repeat;background-size:${(1024 / TILE.size) * 52}px;background-position:-${(TILE.x / TILE.size) * 52}px -${(TILE.x / TILE.size) * 52}px}
.name{font-size:32px;font-weight:650;letter-spacing:-.015em}
h1{margin-top:auto;font-size:60px;line-height:1.04;font-weight:700;letter-spacing:-.035em}
.sub{margin-top:22px;font-size:24px;line-height:1.4;color:var(--text-2)}
.foot{margin-top:auto;padding-top:28px;display:flex;align-items:center;gap:14px;font-size:19px;color:var(--text-3);font-weight:500}
.dot{width:4px;height:4px;border-radius:50%;background:var(--border-strong)}
.shot{position:absolute;left:636px;top:72px;width:760px;border:1px solid var(--border-strong);border-radius:14px;overflow:hidden;
  box-shadow:0 30px 60px -16px rgba(16,24,40,.22),0 4px 10px -4px rgba(16,24,40,.08);background:#eceef2}
.shot img{display:block;width:100%}
.term{position:absolute;left:600px;top:400px;width:540px;border:1px solid var(--border-strong);border-radius:12px;background:var(--surface);
  box-shadow:0 24px 48px -12px rgba(16,24,40,.25);overflow:hidden;font-family:var(--mono)}
.bar{height:34px;display:flex;align-items:center;padding:0 16px;border-bottom:1px solid var(--border);background:#f3f4f6;font-size:14px;color:var(--text-3)}
pre{padding:14px 16px 16px;font-family:var(--mono);font-size:15px;line-height:1.7;white-space:pre;overflow:hidden}
.ps{color:#067647}.o{color:var(--text-2)}
`;

const card = `<!doctype html><html><head><meta charset="utf-8"><style>${css}</style></head><body>
<div class="copy">
  <div class="brand"><span class="mark"></span><span class="name">Siple</span></div>
  <h1>The scriptable SIP softphone.</h1>
  <p class="sub">Several lines, busy lamps and transfers for people. Every button a command for agents.</p>
  <div class="foot">macOS<span class="dot"></span>Windows<span class="dot"></span>Linux<span class="dot"></span>source-available</div>
</div>
<div class="shot"><img src="${shotSrc}" alt=""></div>
<div class="term"><div class="bar">siple-cli</div><pre><span class="ps">$ </span>siple-cli ctl call.dial '{"account":"desk-1001",
    "target":"1003"}'
<span class="o">{"call_id":"c1","call":{"state":"dialing",…}}</span>
<span class="ps">$ </span>siple-cli ctl call.mute '{"call_id":"c1"}'</pre></div>
</body></html>`;

/** An ICO file holding one PNG image. */
function pngToIco(png, size) {
  const head = Buffer.alloc(22);
  head.writeUInt16LE(0, 0);
  head.writeUInt16LE(1, 2);
  head.writeUInt16LE(1, 4);
  head.writeUInt8(size >= 256 ? 0 : size, 6);
  head.writeUInt8(size >= 256 ? 0 : size, 7);
  head.writeUInt8(0, 8);
  head.writeUInt8(0, 9);
  head.writeUInt16LE(1, 10);
  head.writeUInt16LE(32, 12);
  head.writeUInt32LE(png.length, 14);
  head.writeUInt32LE(22, 18);
  return Buffer.concat([head, png]);
}

const browser = await chromium.launch();
try {
  const ctx = await browser.newContext({ viewport: { width: 1200, height: 630 }, deviceScaleFactor: 1, colorScheme: 'light' });
  const tab = await ctx.newPage();
  mkdirSync(join(pub, 'og'), { recursive: true });
  await tab.setContent(card, { waitUntil: 'load' });
  await tab.evaluate(() => document.fonts.ready);
  await tab.screenshot({ path: join(pub, 'og', 'home.png'), type: 'png' });
  console.log('wrote public/og/home.png');

  /** The app icon at `size`, cropped to its tile (`crop`) or with the OS padding kept, on `bg`. */
  const icon = async (size, { crop = true, bg = 'transparent', pad = 0, tile = TILE, src = iconSrc } = {}) => {
    await tab.setViewportSize({ width: size, height: size });
    const inner = size - 2 * pad;
    const scale = crop ? 1024 / tile.size : 1;
    const off = crop ? (tile.x / tile.size) * inner : 0;
    await tab.setContent(
      `<html><body style="margin:0;width:${size}px;height:${size}px;background:${bg};display:grid;place-items:center">` +
        `<div style="width:${inner}px;height:${inner}px;background:url(${src}) no-repeat;background-size:${inner * scale}px;background-position:-${off}px -${off}px"></div></body></html>`,
      { waitUntil: 'load' },
    );
    return tab.screenshot({ type: 'png', omitBackground: bg === 'transparent' });
  };
  // The mark is the tile alone (no canvas margin), so it is drawn uncropped.
  writeFileSync(join(pub, 'icon-64.png'), await icon(64, { crop: false, src: markSrc }));
  writeFileSync(join(pub, 'icon-192.png'), await icon(192));
  // iOS masks the touch icon itself and fills transparency with black, so it is
  // cropped inside the tile's rounded corners: the gradient runs edge to edge.
  writeFileSync(join(pub, 'apple-touch-icon.png'), await icon(180, { tile: { x: 157, size: 710 } }));
  const fav32 = await icon(32, { crop: false, src: markSrc });
  writeFileSync(join(pub, 'favicon-32.png'), fav32);
  writeFileSync(join(pub, 'favicon.ico'), pngToIco(fav32, 32));
  console.log('wrote icons');
} finally {
  await browser.close();
}
