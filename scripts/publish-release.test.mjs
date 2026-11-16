import test from 'node:test';
import assert from 'node:assert/strict';
import { assetNames, description, publishRelease, sha256, uploadUrl } from './publish-release.mjs';

const context = { actions: 'true', repository: 'siarheisinelnikau/disk-migrator', sha: '1'.repeat(40) };
function bundle() {
  const exe = Buffer.alloc(256); exe.write('MZ'); exe.writeUInt32LE(64, 60); exe.write('PE\0\0', 64); exe.writeUInt16LE(0x8664, 68);
  return new Map([['DiskMigrator-v0.2.0.exe', exe], ['SHA256SUMS', Buffer.from(`${sha256(exe)}  DiskMigrator-v0.2.0.exe\n`)], ['LICENSE', Buffer.from('MIT fixture')], ['THIRD-PARTY-NOTICES.txt', Buffer.from('Upstream license fixture')]]);
}
function fakeApi({ published = false, badDigest = false } = {}) {
  const calls = []; let draft = published ? { id: 1, draft: false } : null;
  return { calls, getRelease: async () => draft, createRelease: async value => { calls.push('create-draft'); draft = { ...value, id: 1, assets: [], upload_url: 'https://uploads.github.com/repos/test/releases/1/assets{?name,label}' }; return draft; },
    upload: async (_, name, data) => { calls.push(`upload:${name}`); const asset = { name, size: data.length, digest: badDigest ? 'sha256:wrong' : `sha256:${sha256(data)}`, state: 'uploaded' }; draft.assets.push(asset); return asset; },
    getReleaseById: async () => draft, publish: async () => { calls.push('publish'); draft.draft = false; return { ...draft, html_url: 'https://github.com/siarheisinelnikau/disk-migrator/releases/tag/v0.2.0' }; } };
}
test('creates a draft and publishes only after all four server digests match', async () => {
  const api = fakeApi(); const release = await publishRelease({ api, context, version: '0.2.0', prerelease: true, files: bundle() });
  assert.equal(release.draft, false); assert.equal(release.prerelease, true); assert.equal(release.target_commitish, context.sha); assert.equal(api.calls.at(-1), 'publish'); assert.equal(api.calls.filter(s => s.startsWith('upload:')).length, assetNames('0.2.0').length); assert.ok(description('0.2.0').includes('have not been validated'));
});
test('never authenticates locally or publishes from another repository', async () => {
  for (const changed of [{ ...context, actions: 'false' }, { ...context, repository: 'another/repo' }, { ...context, sha: 'main' }]) {
    const api = fakeApi(); await assert.rejects(publishRelease({ api, context: changed, version: '0.2.0', prerelease: true, files: bundle() })); assert.deepEqual(api.calls, []);
  }
});
test('a failed upload digest leaves the release as an unpublished draft', async () => {
  const api = fakeApi({ badDigest: true }); await assert.rejects(publishRelease({ api, context, version: '0.2.0', prerelease: true, files: bundle() }), /digest/); assert.ok(!api.calls.includes('publish'));
});
test('published releases are never rewritten', async () => {
  const api = fakeApi({ published: true }); await assert.rejects(publishRelease({ api, context, version: '0.2.0', prerelease: true, files: bundle() }), /never rewritten/); assert.deepEqual(api.calls, []);
});
test('broken checksums fail before any release API call', async () => {
  const files = bundle(); files.set('SHA256SUMS', Buffer.from('corrupt')); const api = fakeApi(); await assert.rejects(publishRelease({ api, context, version: '0.2.0', prerelease: true, files }), /checksum/); assert.deepEqual(api.calls, []);
});
test('upload credentials are restricted to the official HTTPS upload origin', () => {
  assert.throws(() => uploadUrl('https://untrusted.test/assets', 'app.exe')); assert.throws(() => uploadUrl('http://uploads.github.com/assets', 'app.exe')); assert.equal(uploadUrl('https://uploads.github.com/assets{?name}', 'app.exe').searchParams.get('name'), 'app.exe');
});
