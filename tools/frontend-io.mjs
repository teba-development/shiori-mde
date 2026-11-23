import { basename, dirname, extname, isAbsolute, relative, resolve, sep } from 'node:path';
import { closeFilesystem, filesystem, projectRootKey } from './frontend-native.mjs';

const directories = new Map();
const outputKey = path => resolve(path).toLowerCase();
const token = value => typeof value === 'string' && /^[0-9a-f]{32}$/.test(value);
function owned(directory, root) {
  if (typeof directory !== 'string' || !isAbsolute(directory)) throw new Error('Invalid private frontend directory');
  const entry = directories.get(resolve(directory));
  if (!entry || (root !== undefined && projectRootKey(root) !== entry.root)) throw new Error('Unknown private frontend directory');
  return entry;
}
export async function closeFrontendFilesystem(root) {
  const rootKey = projectRootKey(root);
  try { await closeFilesystem(root); }
  finally {
    for (const [path, entry] of directories) if (rootKey === entry.root) directories.delete(path);
  }
}
async function readInput(root, name) {
  const text = await (await filesystem(root)).call({ op: 'read', name });
  if (typeof text !== 'string') throw new Error('Invalid frontend input reply');
  return text;
}
export async function readProjectText(root, name) {
  if (!['index.html', 'tsconfig.json', 'src/main.ts', 'src/document.css'].includes(name)) throw new Error('Unsupported frontend input');
  return readInput(root, name);
}
export async function loadProjectSource(root, path) {
  const absolute = resolve(path);
  const directory = resolve(root);
  const prefix = directory.endsWith(sep) ? directory : directory + sep;
  if (!absolute.startsWith(prefix)) throw new Error('Frontend source is outside the project');
  const name = absolute.slice(prefix.length).split(sep).join('/');
  // Installed package code is a trusted build dependency. Project inputs
  // use the guard even if esbuild has already resolved their names.
  if (name.startsWith('node_modules/')) return;
  const extension = extname(name);
  const loader = { '.ts': 'ts', '.css': 'css', '.png': 'file' }[extension];
  const sourceDirectory = loader === 'file' ? 'assets/' : 'src/';
  if (!loader || !name.startsWith(sourceDirectory) || name.split('/').some(part => !portableName(part))) {
    throw new Error('Unsupported frontend source: ' + name.slice(0, 256));
  }
  const text = await readInput(root, name);
  return { loader, contents: loader === 'file' ? Buffer.from(text, 'base64') : text, watchFiles: [path] };
}
export async function makePrivateDirectory(root, kind) {
  if (!['build', 'dev', 'package'].includes(kind)) throw new Error('Invalid frontend directory kind');
  const guard = await filesystem(root);
  const value = await guard.call({ op: 'make', kind });
  if (!token(value?.id) || typeof value.path !== 'string' || !isAbsolute(value.path)
      || basename(value.path) !== '.frontend-' + kind + '-' + value.id
      || relative(dirname(value.path), root) !== '') {
    throw new Error('Invalid filesystem directory reply');
  }
  directories.set(resolve(value.path), { root: projectRootKey(root), id: value.id, guard, state: 'active' });
  return value.path;
}
export async function assertPrivateDirectory(root, path) {
  const entry = owned(path, root);
  if (entry.state !== 'active') throw new Error('Frontend directory is no longer active');
  await entry.guard.call({ op: 'check', directory: entry.id });
}
export async function removePrivateDirectory(root, path) {
  const entry = owned(path, root);
  if (entry.state === 'removed' || entry.state === 'published') return;
  await entry.guard.call({ op: 'remove', directory: entry.id });
  entry.state = 'removed';
}
function portableName(name) {
  return typeof name === 'string' && name.length > 0 && name.length <= 255
    && name.isWellFormed() && !/[<>:"/\\|?*\u0000-\u001f\u007f-\u009f]/.test(name) && !/[. ]$/.test(name)
    && !/^(?:con|prn|aux|nul|conin\$|conout\$|com[1-9\u00b9\u00b2\u00b3]|lpt[1-9\u00b9\u00b2\u00b3])$/i
      .test(name.split('.', 1)[0].trimEnd());
}
export function outputAsset(root, directory, value) {
  if (typeof value !== 'string' || !value || value.length > 8192 || /[\u0000-\u001f\u007f]/.test(value)) {
    throw new Error('Invalid frontend asset path');
  }
  const path = resolve(root, value);
  const local = relative(directory, path);
  if (dirname(path) !== resolve(directory) || !portableName(local)
      || !/\.(?:js|css|png|map|txt)$/i.test(local)) throw new Error('Frontend asset must be a portable output file');
  return { path, url: '/' + encodeURIComponent(local) };
}
export async function atomicWrite(directory, name, contents) {
  if (!portableName(name)) throw new Error('Invalid frontend output filename');
  const entry = owned(directory);
  if (entry.state !== 'active') throw new Error('Frontend directory is no longer active');
  const bytes = typeof contents === 'string' ? Buffer.from(contents, 'utf8') : contents;
  if (!(bytes instanceof Uint8Array) || bytes.byteLength > 128 * 1024 * 1024) throw new Error('Invalid frontend output bytes');
  const write = await entry.guard.call({ op: 'begin', directory: entry.id, name, size: bytes.byteLength });
  if (!token(write)) throw new Error('Invalid filesystem write reply');
  try {
    for (let offset = 0; offset < bytes.byteLength; offset += 256 * 1024) {
      const data = Buffer.from(bytes.subarray(offset, offset + 256 * 1024)).toString('base64');
      await entry.guard.call({ op: 'append', write, data });
    }
    await entry.guard.call({ op: 'commit', write });
  } catch (error) {
    await entry.guard.call({ op: 'abort', write }).catch(() => {});
    throw error;
  }
}
export function inspectBundle(root, directory, files) {
  if (!Array.isArray(files) || !files.length || files.length > 4096) throw new Error('Invalid frontend asset count');
  const paths = new Set();
  let bytes = 0;
  const assets = files.map(file => {
    const asset = outputAsset(root, directory, file.path);
    const name = outputKey(asset.path);
    if (paths.has(name) || !(file.contents instanceof Uint8Array)) throw new Error('Invalid or duplicate frontend asset');
    paths.add(name);
    bytes += file.contents.byteLength;
    if (bytes > 128 * 1024 * 1024) throw new Error('Frontend output exceeds 128 MiB');
    return { ...asset, contents: file.contents };
  });
  return { paths, assets };
}
export async function writeBundle(root, directory, files) {
  await assertPrivateDirectory(root, directory);
  const { paths, assets } = inspectBundle(root, directory, files);
  for (const asset of assets) await atomicWrite(directory, basename(asset.path), asset.contents);
  return paths;
}
export function renderIndex(template, { css, script, icon }) {
  if (typeof template !== 'string') throw new Error('Invalid frontend HTML template');
  const entry = '<script type="module" src="/src/main.ts"></script>';
  const favicon = '/assets/shiori-bookmark-v1.png';
  for (const marker of [entry, favicon, '</head>']) {
    if (template.split(marker).length !== 2) throw new Error('Frontend HTML must contain each placeholder exactly once');
  }
  const attribute = value => {
    if (typeof value !== 'string' || !value.startsWith('/') || /[?#\u0000-\u0020\u007f]/.test(value)
        || !portableName(decodeURIComponent(value.slice(1)))) throw new Error('Invalid frontend asset URL');
    return value.replaceAll('&', '&amp;').replaceAll('"', '&quot;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
  };
  return template.replace(favicon, () => attribute(icon))
    .replace('</head>', () => '  <link rel="stylesheet" href="' + attribute(css) + '">\n  </head>')
    .replace(entry, () => '<script type="module" src="' + attribute(script) + '"></script>');
}
export async function publishBundle(root, directory) {
  const entry = owned(directory, root);
  if (entry.state !== 'active') throw new Error('Frontend directory is no longer active');
  await entry.guard.call({ op: 'publish', directory: entry.id });
  entry.state = 'published';
}
export async function sealBundle(root, directory, required = 'index.html') {
  const entry = owned(directory, root);
  if (entry.state !== 'active') throw new Error('Frontend directory is no longer active');
  const names = await entry.guard.call({ op: 'seal', directory: entry.id });
  if (!Array.isArray(names) || names.length > 4097 || !names.includes(required)
      || names.some(name => !portableName(name))) throw new Error('Invalid sealed frontend output');
  entry.state = 'sealed';
  return names.map(name => resolve(directory, name));
}
