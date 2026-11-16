import { readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { resolve, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const REPOSITORY = 'siarheisinelnikau/disk-migrator';
export const sha256 = value => createHash('sha256').update(value).digest('hex');
export function description(version) {
  return `Disk Migrator ${version} is an experimental Windows x64 preview. This is the first published binary release.\n\nAdded\n- A six-language image migration wizard with light/dark themes, reduced motion and saved preferences.\n- Strict MBR/GPT planning, durable checkpoints, safe image resume and full destination byte verification, including relocated backup GPT metadata.\n- Windows SDK VSS requester code with System, Registry and ASR writer checks, cancellation and snapshot cleanup.\n- Explicit advanced commands for VSS-to-image capture, image-to-SSD copying and live Windows-to-SSD copying. Physical targets require their exact serial confirmation, stable hardware identity and pre-existing offline/writable state.\n- In-app update checking and user-triggered verified portable installation with rollback.\n\nValidation\n- Rust formatting, Clippy, generated-image tests and fake-adapter failure/recovery tests run before packaging.\n- The Windows x64 executable is inspected for its embedded icons, unelevated manifest, static CRT and CPU/GDI presentation imports. The GUI is not launched by the release job.\n\nKnown limitations\n- Real physical SSD writes, native VSS behavior, GUI idle/hidden performance, in-app update installation and Windows bootability have not been validated on hardware or in a VM. This preview must not be treated as a verified boot migration tool.\n- Live migration requires basic UEFI/GPT, NTFS data/recovery, healthy VSS writers and an exclusively lockable FAT32 EFI partition. It refuses unsupported or changing sources. BitLocker live capture, ReFS, dynamic disks, filesystem resizing, sector-size conversion and boot repair are unsupported.\n- Live snapshots are ephemeral. Export an offline image for resume across application restarts. Copied disk/partition identifiers are preserved, and the destination remains offline.\n\nDownload DiskMigrator-v${version}.exe. SHA256SUMS verifies that executable. MIT application source and third-party license notices are included. Windows VSS uses built-in OS components; no driver or service is installed by Disk Migrator.`;
}
export function validateContext(context) {
  if (context.actions !== 'true' || context.repository !== REPOSITORY || !/^[a-f0-9]{40}$/.test(context.sha ?? '')) {
    throw new Error('Publishing is restricted to the canonical GitHub Actions runner and an exact source commit.');
  }
}
export function assetNames(version) { return [`DiskMigrator-v${version}.exe`, 'SHA256SUMS', 'LICENSE', 'THIRD-PARTY-NOTICES.txt']; }
export function validateBundle(version, files) {
  const names = assetNames(version);
  for (const name of names) if (!Buffer.isBuffer(files.get(name)) || !files.get(name).length) throw new Error(`Missing release artifact: ${name}`);
  const executable = files.get(names[0]);
  const header = executable.length >= 64 ? executable.readUInt32LE(60) : executable.length;
  if (executable.subarray(0, 2).toString() !== 'MZ' || header + 24 > executable.length || executable.subarray(header, header + 4).toString() !== 'PE\0\0' || executable.readUInt16LE(header + 4) !== 0x8664) throw new Error('Release executable must be a Windows x64 PE.');
  const sum = files.get('SHA256SUMS').toString('utf8').trim();
  if (sum !== `${sha256(executable)}  ${names[0]}`) throw new Error('Packaged executable checksum does not match SHA256SUMS.');
}
export async function publishRelease({ api, context, version, prerelease, files }) {
  validateContext(context);
  if (!/^0\.2\.0$/.test(version)) throw new Error('This reviewed release request is scoped to v0.2.0.');
  validateBundle(version, files);
  const tag = `v${version}`, body = description(version);
  let release = await api.getRelease(tag);
  if (release && !release.draft) throw new Error('This tag is already published; existing releases are never rewritten.');
  if (release && (release.target_commitish !== context.sha || release.body !== body)) throw new Error('An unrelated draft already owns this tag.');
  if (!release) release = await api.createRelease({ tag_name: tag, target_commitish: context.sha, name: `Disk Migrator ${version} — experimental preview`, body, draft: true, prerelease: Boolean(prerelease), make_latest: 'false' });
  const expected = new Map();
  for (const name of assetNames(version)) {
    const data = files.get(name), digest = `sha256:${sha256(data)}`;
    const existing = release.assets?.find(asset => asset.name === name);
    const asset = existing ?? await api.upload(release.upload_url, name, data);
    if (asset.size !== data.length || asset.digest !== digest || asset.state !== 'uploaded') throw new Error(`Uploaded asset failed server digest/size verification: ${name}. The release stays a draft.`);
    expected.set(name, digest);
  }
  const checked = await api.getReleaseById(release.id);
  for (const [name, digest] of expected) {
    const asset = checked.assets.find(a => a.name === name);
    if (!asset || asset.digest !== digest || asset.state !== 'uploaded') throw new Error(`Final release asset verification failed: ${name}`);
  }
  return api.publish(release.id, { draft: false, prerelease: Boolean(prerelease), make_latest: 'false' });
}
export function uploadUrl(value, name) {
  const url = new URL(value.split('{', 1)[0]);
  if (url.protocol !== 'https:' || url.hostname !== 'uploads.github.com') throw new Error('Unexpected GitHub upload origin.');
  url.searchParams.set('name', name);
  return url;
}
function githubApi(token) {
  const base = `https://api.github.com/repos/${REPOSITORY}`;
  async function request(method, path, body, binary = false) {
    const response = await fetch(path, { method, redirect: 'error', headers: { Authorization: `Bearer ${token}`, Accept: 'application/vnd.github+json', 'X-GitHub-Api-Version': '2022-11-28', 'Content-Type': binary ? 'application/octet-stream' : 'application/json' }, body: body === undefined ? undefined : binary ? body : JSON.stringify(body), signal: AbortSignal.timeout(180000) });
    if (response.status === 404 && method === 'GET') return null;
    if (!response.ok) throw new Error(`GitHub ${method} failed with HTTP ${response.status}: ${(await response.text()).slice(0, 1000)}`);
    return response.json();
  }
  return {
    getRelease: tag => request('GET', `${base}/releases/tags/${encodeURIComponent(tag)}`),
    getReleaseById: id => request('GET', `${base}/releases/${id}`),
    createRelease: data => request('POST', `${base}/releases`, data),
    upload: (url, name, data) => request('POST', uploadUrl(url, name), data, true),
    publish: (id, data) => request('PATCH', `${base}/releases/${id}`, data),
  };
}
async function main() {
  const context = { actions: process.env.GITHUB_ACTIONS, repository: process.env.GITHUB_REPOSITORY, sha: process.env.GITHUB_SHA };
  validateContext(context);
  const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const request = JSON.parse(await readFile(resolve(root, '.github/release-request.json'), 'utf8'));
  if (!request.publish) throw new Error('Release publication is not enabled in the reviewed request.');
  const manifest = await readFile(resolve(root, 'Cargo.toml'), 'utf8');
  if (manifest.match(/^version = "([^"]+)"/m)?.[1] !== request.version) throw new Error('Package version and release request differ.');
  if (!process.env.GITHUB_TOKEN) throw new Error('The GitHub Actions token is missing. No local authentication fallback is used.');
  const files = new Map(await Promise.all(assetNames(request.version).map(async name => [name, await readFile(resolve(root, 'dist', name))])));
  const release = await publishRelease({ api: githubApi(process.env.GITHUB_TOKEN), context, version: request.version, prerelease: request.prerelease, files });
  process.stdout.write(`Published ${release.html_url}\n`);
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) main().catch(error => { process.stderr.write(`${error.message}\n`); process.exitCode = 1; });
