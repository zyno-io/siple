// Self-checks of scripts/update-feed.mjs: `npm run feed:test` (node's built-in runner, no network).
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { buildFeed, changelogVersion, checkFeed, newestStable, parseTag } from './update-feed.mjs';

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
  assert.deepEqual(Object.keys(feed[0]), ['tag_name', 'name', 'draft', 'prerelease', 'published_at', 'html_url', 'body', 'assets']);
  assert.deepEqual(feed[0].assets, [{ name: 'a', size: 1, state: 'uploaded', browser_download_url: 'https://example/a' }]);
  assert.equal(feed[1].prerelease, true);
  assert.ok(feed.every((r) => r.draft === false));
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
