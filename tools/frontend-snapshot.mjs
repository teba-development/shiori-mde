import { createHash } from 'node:crypto';
import { basename, resolve } from 'node:path';
import { atomicWrite, sealBundle } from './frontend-io.mjs';

const name = 'frontend.bundle';
const magic = Buffer.from('SHIORI\0\x01', 'latin1');
const maxBytes = 128 * 1024 * 1024;

export async function prepareFrontendSnapshot(root, directory, assets, index) {
  const id = /^\.frontend-package-([0-9a-f]{32})$/.exec(basename(directory))?.[1];
  if (!id || !Array.isArray(assets) || !assets.length || assets.length > 4096 || typeof index !== 'string') {
    throw new Error('Invalid frontend snapshot');
  }
  const indexBytes = Buffer.from(index, 'utf8');
  if (!indexBytes.length || indexBytes.length > 128 * 1024) {
    throw new Error('Invalid frontend snapshot index');
  }
  const files = [...assets.map(asset => [basename(asset.path), asset.contents]), ['index.html', indexBytes]];
  const count = Buffer.alloc(4);
  count.writeUInt32LE(files.length);
  const parts = [magic, count];
  const names = new Set();
  let size = magic.length + count.length;
  for (const [file, bytes] of files) {
    const key = file.toLowerCase();
    if (file.length > 255 || !/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(file)
        || (file !== 'index.html' && !/\.(?:js|css|png|txt)$/.test(file))
        || names.has(key) || !(bytes instanceof Uint8Array)) {
      throw new Error('Invalid or duplicate frontend snapshot filename');
    }
    names.add(key);
    size += 6 + file.length + bytes.byteLength;
    if (size > maxBytes) throw new Error('Frontend snapshot exceeds 128 MiB');
    const header = Buffer.alloc(6);
    header.writeUInt16LE(file.length);
    header.writeUInt32LE(bytes.byteLength, 2);
    parts.push(header, Buffer.from(file, 'ascii'), bytes);
  }
  const contents = Buffer.concat(parts, size);
  const digest = createHash('sha256').update(contents).digest('hex');
  await atomicWrite(directory, name, contents);
  const sealed = await sealBundle(root, directory, name);
  if (sealed.length !== 1 || sealed[0] !== resolve(directory, name)) {
    throw new Error('Unexpected frontend snapshot files');
  }
  return { id, digest };
}
