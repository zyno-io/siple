/**
 * The newest released version, read at build time from the Siple CHANGELOG (repo root), so the site never carries a
 * hand-edited version or date. Sections look like `## 0.2.2 (2026-11-22)`; `## Unreleased` is skipped.
 * Builds run from sites/www (npm run build, CI), so the repo root is two levels up from the working directory.
 */
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

const changelog = resolve(process.cwd(), process.env.SIPLE_CHANGELOG ?? '../../CHANGELOG.md');

function load(): { version: string; date: string; label: string } {
  let text: string;
  try {
    text = readFileSync(changelog, 'utf8');
  } catch (err) {
    throw new Error(`release.ts: cannot read ${changelog} (build from sites/www, or set SIPLE_CHANGELOG): ${(err as Error).message}`);
  }
  // The newest section that is not Unreleased must be the release: a malformed heading fails the build instead of
  // silently falling back to an older version.
  const heading = text.split(/\r?\n/).find((l) => l.startsWith('## ') && !/^## \[?unreleased\]?\s*$/i.test(l));
  if (!heading) throw new Error(`release.ts: no released section found in ${changelog}`);
  const m = /^## (\d+\.\d+\.\d+) \((\d{4})-(\d{2})-(\d{2})\)\s*$/.exec(heading);
  if (!m) throw new Error(`release.ts: the newest CHANGELOG section "${heading}" is not "## X.Y.Z (YYYY-MM-DD)"`);
  const [, version, y, mo, d] = m;
  const months = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
  const month = months[Number(mo) - 1];
  if (!month) throw new Error(`release.ts: bad date in the ${version} CHANGELOG heading`);
  // Release and tag builds in CI set SIPLE_EXPECT_VERSION to the tag: a CHANGELOG that wasn't updated for it would
  // otherwise publish an older version under the new tag.
  const expected = process.env.SIPLE_EXPECT_VERSION?.replace(/^v/, '');
  if (expected && expected !== version) {
    throw new Error(`release.ts: building tag v${expected}, but the newest CHANGELOG section is ${version}`);
  }
  return { version, date: `${y}-${mo}-${d}`, label: `${Number(d)} ${month} ${y}` };
}

/** `date` is ISO (2026-11-22); `label` is the display form (22 Nov 2026). */
export const release = load();
