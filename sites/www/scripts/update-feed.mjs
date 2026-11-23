#!/usr/bin/env node
/**
 * Writes public/updates/v1/releases.json, the update feed of the direct-download builds of the app
 * (crates/siple-core/src/update/site.rs asks https://siple.app/updates/v1/releases.json about every hour and falls
 * back to the GitHub releases API when the site gives no usable answer).
 *
 * The feed is a JSON array in the shape of the GitHub releases API, cut down to what the app's parser reads
 * (crates/siple-core/src/update/releases.rs): published releases only (no drafts), newest first, at most 20,
 * pre-releases included (the app's channel setting filters them). The file is unsigned and only a hint: the app
 * installs from the minisign-signed SHA256SUMS of the version it found, never from anything in here.
 *
 * Runs before `astro build` (npm run build). Generated, not committed.
 *   GITHUB_TOKEN / GH_TOKEN  used when set (CI passes the workflow token: no rate limit); otherwise the request is
 *                            anonymous (60 per hour per IP, plenty for a local build).
 *   SIPLE_FEED=skip          local builds only: write no feed (offline work). Refused in GitHub Actions.
 *   SIPLE_EXPECT_VERSION     release and tag builds (set by www.yml): the build fails unless the newest stable
 *                            release in the feed is this version. Other builds (staging from main, pull requests)
 *                            only warn, since main's CHANGELOG may name a version that is not published yet.
 *   SIPLE_CHANGELOG          the CHANGELOG to read, as in src/data/release.ts.
 *   SIPLE_RELEASES_JSON      a file to read instead of calling GitHub (tests, offline checks).
 * `node --test scripts/update-feed.test.mjs` runs the self-checks.
 */
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

export const REPO = 'zyno-io/siple';
export const MAX_RELEASES = 20;
const MAX_BODY_CHARS = 10_000;
const TAG = /^v(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?$/;

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const out = resolve(root, 'public/updates/v1/releases.json');

/** `[major, minor, patch]` of a `vX.Y.Z[-pre]` tag, or null when it is not a version tag. */
export function parseTag(tag) {
  const m = typeof tag === 'string' ? TAG.exec(tag) : null;
  return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null;
}

function compare(a, b) {
  for (let i = 0; i < 3; i++) if (a[i] !== b[i]) return a[i] - b[i];
  return 0;
}

const stable = (r) => !r.prerelease && !r.tag_name.includes('-');

/**
 * Cuts the API's list down to the feed: published releases with a version tag, newest first (by publication time,
 * then version), at most `MAX_RELEASES`, only the fields the app reads.
 */
export function buildFeed(releases) {
  if (!Array.isArray(releases)) throw new Error('the releases answer is not a list');
  return releases
    .filter((r) => r && typeof r === 'object' && r.draft !== true && parseTag(r.tag_name))
    .sort((a, b) => {
      const t = Date.parse(b.published_at ?? 0) - Date.parse(a.published_at ?? 0);
      return t || compare(parseTag(b.tag_name), parseTag(a.tag_name));
    })
    .slice(0, MAX_RELEASES)
    .map((r) => ({
      tag_name: r.tag_name,
      name: r.name ?? r.tag_name,
      draft: false,
      prerelease: Boolean(r.prerelease) || r.tag_name.includes('-'),
      published_at: r.published_at ?? null,
      html_url: r.html_url,
      ...(typeof r.body === 'string' && r.body ? { body: r.body.slice(0, MAX_BODY_CHARS) } : {}),
      assets: (Array.isArray(r.assets) ? r.assets : [])
        // The app only takes assets that finished uploading.
        .filter((a) => a && a.state === 'uploaded' && typeof a.name === 'string' && Number.isSafeInteger(a.size))
        .map((a) => ({
          name: a.name,
          size: a.size,
          state: 'uploaded',
          browser_download_url: a.browser_download_url,
        })),
    }));
}

/** The newest stable release of a feed (version order, not list order), or undefined. */
export function newestStable(feed) {
  return feed
    .filter(stable)
    .sort((a, b) => compare(parseTag(b.tag_name), parseTag(a.tag_name)))[0];
}

/**
 * Checks the feed against the version the CHANGELOG calls released. Returns the problems found; the caller decides
 * whether they fail the build.
 */
export function checkFeed(feed, version) {
  const problems = [];
  if (feed.length === 0) problems.push('the feed has no releases');
  const newest = newestStable(feed);
  if (!newest) problems.push('the feed has no stable release');
  else if (newest.tag_name !== `v${version}`) {
    problems.push(`the newest stable release in the feed is ${newest.tag_name}, but the newest CHANGELOG section is ${version}`);
  } else {
    for (const need of ['SHA256SUMS', 'SHA256SUMS.minisig']) {
      if (!newest.assets.some((a) => a.name === need)) problems.push(`${newest.tag_name} has no ${need} asset yet`);
    }
  }
  return problems;
}

/** The released version, from the newest `## X.Y.Z (date)` section of the CHANGELOG (as src/data/release.ts). */
export function changelogVersion(text) {
  const heading = text.split(/\r?\n/).find((l) => l.startsWith('## ') && !/^## \[?unreleased\]?\s*$/i.test(l));
  const m = heading && /^## (\d+\.\d+\.\d+) \(\d{4}-\d{2}-\d{2}\)\s*$/.exec(heading);
  if (!m) throw new Error(`the newest CHANGELOG section ${JSON.stringify(heading)} is not "## X.Y.Z (YYYY-MM-DD)"`);
  return m[1];
}

async function fetchReleases() {
  if (process.env.SIPLE_RELEASES_JSON) return JSON.parse(readFileSync(process.env.SIPLE_RELEASES_JSON, 'utf8'));
  const token = process.env.GITHUB_TOKEN || process.env.GH_TOKEN;
  const headers = {
    Accept: 'application/vnd.github+json',
    'X-GitHub-Api-Version': '2022-11-28',
    'User-Agent': 'siple-www-build',
    ...(token ? { Authorization: `Bearer ${token}` } : {}),
  };
  const all = [];
  // Version tags only count towards the 20, so look past non-version releases (a few pages at most).
  for (let page = 1; page <= 3; page++) {
    const url = `https://api.github.com/repos/${REPO}/releases?per_page=100&page=${page}`;
    let res;
    for (let attempt = 1; ; attempt++) {
      try {
        res = await fetch(url, { headers, signal: AbortSignal.timeout(30_000) });
        if (res.ok || res.status < 500 || attempt === 3) break;
      } catch (err) {
        if (attempt === 3) throw err;
      }
      await new Promise((r) => setTimeout(r, 2000 * attempt));
    }
    if (!res.ok) {
      const hint = res.status === 403 || res.status === 429 ? ' (rate limited? set GITHUB_TOKEN)' : '';
      throw new Error(`GET ${url} answered HTTP ${res.status}${hint}`);
    }
    const list = await res.json();
    if (!Array.isArray(list)) throw new Error(`GET ${url} did not answer a list`);
    all.push(...list);
    if (list.length < 100 || buildFeed(all).length >= MAX_RELEASES) break;
  }
  return all;
}

async function main() {
  if (process.env.SIPLE_FEED === 'skip') {
    if (process.env.GITHUB_ACTIONS) throw new Error('SIPLE_FEED=skip is not allowed in GitHub Actions: the deploy must carry the feed');
    console.warn('update-feed: SIPLE_FEED=skip, writing no feed (the site build has no /updates/v1/releases.json)');
    return;
  }
  const changelog = resolve(process.cwd(), process.env.SIPLE_CHANGELOG ?? '../../CHANGELOG.md');
  const version = changelogVersion(readFileSync(changelog, 'utf8'));
  const feed = buildFeed(await fetchReleases());
  const problems = checkFeed(feed, version);
  const strict = Boolean(process.env.SIPLE_EXPECT_VERSION);
  if (problems.length && strict) {
    throw new Error(`the update feed does not carry the release being published:\n  - ${problems.join('\n  - ')}`);
  }
  for (const p of problems) console.warn(`update-feed: warning: ${p} (not a release build, so the build goes on)`);
  mkdirSync(dirname(out), { recursive: true });
  writeFileSync(out, `${JSON.stringify(feed, null, 2)}\n`);
  console.log(`update-feed: wrote ${feed.length} releases (newest stable ${newestStable(feed)?.tag_name ?? 'none'}) to public/updates/v1/releases.json`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((err) => {
    console.error(`update-feed: ${err.message}`);
    process.exit(1);
  });
}
