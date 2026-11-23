// Self-checks of scripts/update-feed.mjs: `npm run feed:test` (node's built-in runner, no network).
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { buildFeed, changelogVersion, checkFeed, fetchChecked, newestStable, parseTag } from './update-feed.mjs';

const asset = (name, size = 1, state = 'uploaded') => ({ name, size, state, browser_download_url: `https://example/${name}`, id: 7, uploader: {} });
const rel = (tag, extra = {}) => ({
  tag_name: tag,
  name: tag,
  draft: false,
  prerelease: false,
  published_at: '2026-11-01T10:00:00Z',
  html_url: `https://github.com/zyno-io/siple/releases/tag/${tag}`,
  body: 'notes',
  author: { login: 'x' },
  assets: [asset('SHA256SUMS'), asset('SHA256SUMS.minisig')],
  ...extra,
});

test('tags', () => {
  assert.deepEqual(parseTag('v0.2.4'), [0, 2, 4]);
  assert.deepEqual(parseTag('v0.3.0-rc.1'), [0, 3, 0]);
  for (const bad of ['0.2.4', 'nightly', 'v1.2', 'v1.2.3+x', undefined, 7]) assert.equal(parseTag(bad), null);
});

test('drafts and non-version tags are dropped, newest first, only the app fields', () => {
  const feed = buildFeed([
    rel('v0.2.0', { published_at: '2026-10-01T00:00:00Z' }),
    rel('v0.9.0', { draft: true }),
    rel('nightly', { prerelease: true }),
    rel('v0.3.0-rc.1', { prerelease: true, published_at: '2026-11-10T00:00:00Z' }),
    rel('v0.2.4', { published_at: '2026-11-20T00:00:00Z', assets: [asset('a'), asset('b', 2, 'starter'), asset('c', 'x')] }),
  ]);
  assert.deepEqual(feed.map((r) => r.tag_name), ['v0.2.4', 'v0.3.0-rc.1', 'v0.2.0']);
  assert.deepEqual(Object.keys(feed[0]), ['tag_name', 'name', 'draft', 'prerelease', 'published_at', 'html_url', 'assets']);
  assert.deepEqual(feed[0].assets, [{ name: 'a', size: 1, state: 'uploaded', browser_download_url: 'https://example/a' }]);
  assert.equal(feed[1].prerelease, true);
  assert.ok(feed.every((r) => r.draft === false));
});

test('release notes are not in the feed, so no text is cut (a surrogate pair stays whole)', () => {
  const notes = `${'a'.repeat(9_999)}\u{1F600} and more`;
  const feed = buildFeed([rel('v0.2.0', { body: notes })]);
  assert.ok(!('body' in feed[0]));
  assert.ok(!JSON.stringify(feed).includes('and more'));
  // The file always parses as strict JSON text (no lone surrogate from a cut).
  assert.deepEqual(JSON.parse(JSON.stringify(feed)), feed);
  assert.ok(!/[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/.test(JSON.stringify(feed)));
});

test('at most 20, keeping the newest', () => {
  const many = Array.from({ length: 30 }, (_, i) => rel(`v1.0.${i}`, { published_at: new Date(Date.UTC(2026, 0, 1 + i)).toISOString() }));
  const feed = buildFeed(many);
  assert.equal(feed.length, 20);
  assert.equal(feed[0].tag_name, 'v1.0.29');
  assert.equal(feed[19].tag_name, 'v1.0.10');
});

test('the newest stable ignores pre-releases and list order', () => {
  const feed = buildFeed([rel('v0.3.0-rc.1', { prerelease: true }), rel('v0.2.0'), rel('v0.2.4')]);
  assert.equal(newestStable(feed).tag_name, 'v0.2.4');
});

test('the check fails unless the newest stable is the CHANGELOG version, with its signed sums', () => {
  const feed = buildFeed([rel('v0.2.4'), rel('v0.2.3')]);
  assert.deepEqual(checkFeed(feed, '0.2.4'), []);
  assert.match(checkFeed(feed, '0.2.5')[0], /newest stable release in the feed is v0\.2\.4, but .* is 0\.2\.5/);
  assert.match(checkFeed([], '0.2.4')[0], /no releases/);
  assert.match(checkFeed(buildFeed([rel('v0.3.0-rc.1', { prerelease: true })]), '0.3.0').join(), /no stable release/);
  const noSig = buildFeed([rel('v0.2.4', { assets: [asset('SHA256SUMS')] })]);
  assert.match(checkFeed(noSig, '0.2.4')[0], /no SHA256SUMS\.minisig/);
});

test('the CHANGELOG version', () => {
  assert.equal(changelogVersion('# Changelog\n\n## Unreleased\n\n### New\n\n## 0.2.4 (2026-11-22)\n\n## 0.2.3 (2026-11-01)\n'), '0.2.4');
  assert.throws(() => changelogVersion('## Unreleased\n## 0.2.4\n'));
});

test('a strict build looks again while the release is not in the list yet', async () => {
  const lists = [[rel('v0.2.3')], [rel('v0.2.3')], [rel('v0.2.4'), rel('v0.2.3')]];
  let calls = 0;
  const waits = [];
  const out = await fetchChecked('0.2.4', true, { fetchFn: async () => lists[Math.min(calls++, 2)], wait: async (ms) => waits.push(ms), retries: 5, delayMs: 30_000 });
  assert.deepEqual(out.problems, []);
  assert.equal(calls, 3);
  assert.deepEqual(waits, [30_000, 30_000]);
});

test('a strict build gives up after 5 more tries, a normal build never retries', async () => {
  let calls = 0;
  const waits = [];
  const stale = { fetchFn: async () => (calls++, [rel('v0.2.3')]), wait: async (ms) => waits.push(ms) };
  const out = await fetchChecked('0.2.4', true, stale);
  assert.equal(calls, 6);
  assert.equal(waits.length, 5);
  assert.ok(waits.every((ms) => ms === 30_000));
  assert.match(out.problems[0], /newest stable release in the feed is v0\.2\.3/);
  calls = 0;
  waits.length = 0;
  assert.equal((await fetchChecked('0.2.4', false, stale)).problems.length, 1);
  assert.equal(calls, 1);
  assert.deepEqual(waits, []);
});
