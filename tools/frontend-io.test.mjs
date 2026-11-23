import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { once } from 'node:events';
import { link, lstat, mkdir, mkdtemp, readFile, readdir, realpath, rename, rm, symlink, unlink, writeFile } from 'node:fs/promises';
import { basename, dirname, resolve } from 'node:path';
import test from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { crc32 } from 'node:zlib';
import {
  atomicWrite, closeFrontendFilesystem, makePrivateDirectory, outputAsset, publishBundle,
  readProjectText, removePrivateDirectory, renderIndex, sealBundle, writeBundle,
} from './frontend-io.mjs';
import { ensureFilesystemHelper, filesystem, projectRootKey } from './frontend-native.mjs';
import { runFrontend } from './frontend-build.mjs';

const workspace = await realpath(fileURLToPath(new URL('..', import.meta.url)));
const template = await readFile(resolve(workspace, 'index.html'), 'utf8');
async function fixture(t) {
  const base = await mkdtemp(resolve(workspace, '.frontend-tests-'));
  const root = resolve(base, 'project');
  for (const path of ['tools', 'src', 'assets']) await mkdir(resolve(root, path), { recursive: true });
  await writeFile(resolve(root, 'package.json'), '{"type":"module"}');
  await writeFile(resolve(root, 'tsconfig.json'), '{"compilerOptions":{"target":"ES2022"}}');
  await writeFile(resolve(root, 'index.html'), template);
  await writeFile(resolve(root, 'src/main.ts'), 'import image from "../assets/shiori-bookmark-v1.png"; import styles from "./preview"; import "./style.css"; console.log(image, styles);');
  await writeFile(resolve(root, 'src/preview.ts'), 'import styles from "./document.css?inline"; export default styles;');
  await writeFile(resolve(root, 'src/document.css'), '.document {color:green}');
  await writeFile(resolve(root, 'src/style.css'), 'body {color:green}');
  await writeFile(resolve(root, 'assets/shiori-bookmark-v1.png'), Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/U5sAAAAASUVORK5CYII=', 'base64'));
  t.after(async () => {
    await closeFrontendFilesystem(root);
    assert.equal(dirname(await realpath(base)), workspace);
    assert.ok(basename(base).startsWith('.frontend-tests-'));
    await rm(base, { recursive: true, force: false });
  });
  return { root, base };
}
const coreUrl = pathToFileURL(resolve(workspace, 'tools/frontend-build.mjs')).href;
const command = 'const {runFrontend}=await import(' + JSON.stringify(coreUrl) + '); await runFrontend(process.cwd(), false);';
const run = root => spawnSync(process.execPath, ['--input-type=module', '-e', command], { cwd: root, encoding: 'utf8', windowsHide: true, timeout: 20000 });

test('release output stays locked for a build descendant after the caller is terminated', { timeout: 45000 }, async t => {
  const { base } = await fixture(t);
  const helper = await ensureFilesystemHelper();
  const output = resolve(base, 'target');
  const worker = resolve(base, 'worker.mjs');
  const descendant = resolve(base, 'descendant.mjs');
  const allowExit = resolve(base, 'allow-exit');
  const ready = resolve(base, 'descendant-ready');
  const next = resolve(base, 'next-build-ready');
  await writeFile(descendant, `
    import { existsSync, writeFileSync } from 'node:fs';
    import { setTimeout } from 'node:timers/promises';
    const [ready, allowExit] = process.argv.slice(2);
    writeFileSync(ready, 'ready');
    const deadline = Date.now() + 20000;
    while (!existsSync(allowExit) && Date.now() < deadline) await setTimeout(50);
  `);
  await writeFile(worker, `
    import { spawn } from 'node:child_process';
    import { once } from 'node:events';
    import { writeFileSync } from 'node:fs';
    for await (const gate of process.stdin) if (gate.length !== 1 || gate[0] !== 1) throw new Error('Invalid gate');
    const [marker, descendant, allowExit] = process.argv.slice(2);
    if (descendant) {
      // Leave the Node worker's own subprocess group while remaining in the native build job.
      const child = spawn(process.execPath, [descendant, marker, allowExit], { detached: true, stdio: 'inherit', windowsHide: true });
      await once(child, 'spawn');
      child.unref();
    } else writeFileSync(marker, 'ready');
  `);
  const launcher = `
    const {spawn} = require('node:child_process');
    const [helper, output, worker, ...args] = process.argv.slice(1);
    const supervisor = spawn(helper, ['--release', String(process.pid), output, process.execPath, worker, '[]', ...args],
      { detached: true, stdio: 'inherit', windowsHide: true });
    supervisor.on('exit', code => console.error('Test supervisor exited:', code));
    setInterval(() => {}, 1000);
  `;
  const waitForExit = child => new Promise((resolveExit, reject) => {
    child.once('error', reject);
    child.once('exit', resolveExit);
  });
  const owner = spawn(process.execPath, ['-e', launcher, helper, output, worker, ready, descendant, allowExit], { windowsHide: true });
  let diagnostics = '';
  for (const stream of [owner.stdout, owner.stderr]) stream.on('data', chunk => { diagnostics += chunk; });
  const ownerExit = waitForExit(owner);
  let nextExit;
  try {
    for (let attempt = 0; attempt < 200; attempt++) {
      if (await lstat(ready).catch(() => null)) break;
      await delay(50);
    }
    assert.equal(await readFile(ready, 'utf8').catch(() => null), 'ready', diagnostics);
    owner.kill();
    await ownerExit;
    nextExit = waitForExit(spawn(helper, ['--release', String(process.pid), output, process.execPath, worker, '[]', next], {
      windowsHide: true, stdio: ['ignore', 'inherit', 'inherit'],
    }));
    await delay(800);
    await assert.rejects(lstat(next), { code: 'ENOENT' });
  } finally {
    await writeFile(allowExit, 'exit');
    owner.kill();
    await ownerExit;
    if (nextExit) assert.equal(await nextExit, 0);
  }
  assert.equal(await readFile(next, 'utf8'), 'ready');
});

test('a successor does not use the workspace of a terminated supervisor', { timeout: 45000 }, async t => {
  const { base } = await fixture(t);
  const helper = await ensureFilesystemHelper();
  const output = resolve(base, 'target');
  const worker = resolve(base, 'workspace.mjs');
  const first = resolve(base, 'first-workspace');
  const second = resolve(base, 'second-workspace');
  await writeFile(worker, `
    import { writeFileSync } from 'node:fs';
    import { setTimeout } from 'node:timers/promises';
    for await (const gate of process.stdin) if (gate.length !== 1 || gate[0] !== 1) throw new Error('Invalid gate');
    const [marker, wait] = process.argv.slice(2);
    writeFileSync(marker, process.env.CARGO_TARGET_DIR);
    if (wait) await setTimeout(20000);
  `);
  const start = (...args) => {
    const child = spawn(helper, ['--release', String(process.pid), output, process.execPath, worker, '[]', ...args], {
      stdio: ['ignore', 'ignore', 'inherit'], windowsHide: true,
    });
    const exit = new Promise((resolveExit, reject) => {
      child.once('error', reject);
      child.once('exit', resolveExit);
    });
    return { child, exit };
  };
  const owner = start(first, 'wait');
  try {
    for (let attempt = 0; attempt < 200; attempt++) {
      if (await lstat(first).catch(() => null)) break;
      await delay(50);
    }
    assert.equal(dirname(await readFile(first, 'utf8')), output);
    assert.ok(owner.child.kill());
    assert.notEqual(await owner.exit, 0);
    assert.equal(await start(second).exit, 0);
    assert.equal(dirname(await readFile(second, 'utf8')), output);
    assert.notEqual(await readFile(first, 'utf8'), await readFile(second, 'utf8'));
  } finally {
    owner.child.kill();
    await owner.exit;
  }
});

test('a completed workspace is reused after publication fails', { timeout: 45000 }, async t => {
  const { base } = await fixture(t);
  const helper = await ensureFilesystemHelper();
  const output = resolve(base, 'target');
  const worker = resolve(base, 'publication.mjs');
  const first = resolve(base, 'first-workspace');
  const second = resolve(base, 'second-workspace');
  const image = await readFile(helper);
  const machine = image.readUInt16LE(image.readUInt32LE(0x3c) + 4);
  assert.ok(machine === 0xaa64 || machine === 0x8664);
  const target = machine === 0xaa64 ? 'aarch64-pc-windows-msvc' : 'x86_64-pc-windows-msvc';
  const artifact = [target, 'release', 'test.exe'];
  await writeFile(worker, `
    import { copyFileSync, mkdirSync, writeFileSync } from 'node:fs';
    import { dirname, resolve } from 'node:path';
    for await (const gate of process.stdin) if (gate.length !== 1 || gate[0] !== 1) throw new Error('Invalid gate');
    const [marker, source] = process.argv.slice(2);
    const root = process.env.CARGO_TARGET_DIR;
    const artifact = resolve(root, ...${JSON.stringify(artifact)});
    mkdirSync(dirname(artifact), { recursive: true });
    if (source) copyFileSync(source, artifact);
    else writeFileSync(artifact, 'MZ');
    writeFileSync(marker, root);
  `);
  const build = (marker, source) => new Promise((done, fail) => {
    const child = spawn(helper, [
      '--release', String(process.pid), output, process.execPath, worker, JSON.stringify([artifact]), marker, source,
    ], { stdio: ['ignore', 'ignore', 'pipe'], windowsHide: true });
    child.stderr.resume();
    child.once('error', fail);
    child.once('close', done);
  });
  assert.equal(await build(first, ''), 1);
  await assert.rejects(lstat(resolve(output, ...artifact)), { code: 'ENOENT' });
  assert.equal(await build(second, helper), 0);
  assert.equal(await readFile(first, 'utf8'), await readFile(second, 'utf8'));
  assert.deepEqual(await readFile(resolve(output, ...artifact)), image);
});

async function installerFixture(t) {
  const { base: directory } = await fixture(t);
  const base = resolve(directory, '配布 検証');
  await mkdir(base);
  const helper = await ensureFilesystemHelper();
  const output = resolve(base, 'target');
  const image = await readFile(helper);
  // Build a valid PE/NSIS envelope without relying on a previous release.
  const pe = image.readUInt32LE(0x3c);
  const machine = image.readUInt16LE(pe + 4);
  assert.ok(machine === 0xaa64 || machine === 0x8664);
  const target = machine === 0xaa64 ? 'aarch64-pc-windows-msvc' : 'x86_64-pc-windows-msvc';
  const sections = image.readUInt16LE(pe + 6);
  const sectionStart = pe + 24 + image.readUInt16LE(pe + 20);
  let imageEnd = image.readUInt32LE(pe + 24 + 60);
  for (let index = 0; index < sections; index++) {
    const offset = sectionStart + index * 40;
    imageEnd = Math.max(imageEnd, image.readUInt32LE(offset + 16) + image.readUInt32LE(offset + 20));
  }
  const aligned = Math.ceil(imageEnd / 512) * 512;
  const setup = Buffer.alloc(aligned + 33);
  image.copy(setup, 0, 0, imageEnd);
  setup.writeUInt32LE(0xdeadbeef, aligned + 4);
  setup.write('NullsoftInst', aligned + 8, 'ascii');
  setup.writeUInt32LE(1, aligned + 20);
  setup.writeUInt32LE(33, aligned + 24);
  setup.writeUInt32LE(crc32(setup.subarray(512, setup.length - 4)), setup.length - 4);
  const appSource = resolve(base, 'application.exe');
  const setupSource = resolve(base, 'fixture-setup.exe');
  await writeFile(setupSource, setup);
  const extractor = resolve(base, '7z.exe');
  const compiled = spawnSync('rustc', [resolve(workspace, 'tools/fixtures/extractor.rs'), '--target', target, '-o', extractor], {
    encoding: 'utf8', windowsHide: true, timeout: 20000,
  });
  assert.equal(compiled.status, 0, compiled.error?.message ?? compiled.stderr);
  await writeFile(appSource, await readFile(extractor));
  await writeFile(resolve(base, '7z.dll'), 'TEST FIXTURE');
  const appParts = [target, 'release', 'shiori-mde.exe'];
  const setupParts = [target, 'release', 'bundle', 'nsis', 'fixture-setup.exe'];
  const files = [{ source: appSource, path: appParts }, { source: setupSource, path: setupParts }];
  const worker = resolve(base, 'installer.mjs');
  await writeFile(worker, `
    import { copyFileSync, mkdirSync, writeFileSync } from 'node:fs';
    import { dirname, resolve } from 'node:path';
    for await (const gate of process.stdin) if (gate.length !== 1 || gate[0] !== 1) throw new Error('Invalid gate');
    for (const item of JSON.parse(process.argv[2])) {
      const path = resolve(process.env.CARGO_TARGET_DIR, ...item.path);
      mkdirSync(dirname(path), { recursive: true });
      copyFileSync(item.source, path);
    }
    writeFileSync(process.argv[3], process.env.CARGO_TARGET_DIR);
  `);
  const start = (inputs, marker, payload = '') => {
    const child = spawn(helper, ['--release', String(process.pid), output, process.execPath, worker,
      JSON.stringify(inputs.map(item => item.path)), JSON.stringify(inputs), marker], {
      windowsHide: true, stdio: ['ignore', 'ignore', 'pipe'],
      env: { ...process.env, SHIORI_7ZIP: extractor, SHIORI_TEST_EXTRACTOR_PAYLOAD: payload },
    });
    let errors = '';
    child.stderr.setEncoding('utf8').on('data', data => { errors += data; });
    return { child, exit: once(child, 'close'), errors: () => errors };
  };
  return { base, output, files, start, appParts, appSource, setupParts, setupSource };
}

test('interrupting installer verification stops the extractor and isolates the workspace', { timeout: 45000 }, async t => {
  const { base, output, files, start, setupParts } = await installerFixture(t);
  const firstMarker = resolve(base, 'first-workspace');
  const secondMarker = resolve(base, 'next-workspace');
  const owner = start(files, firstMarker);
  let extractorPid;
  let successor;
  try {
    for (let attempt = 0; attempt < 200; attempt++) {
      extractorPid = Number(await readFile(resolve(base, 'extractor-pid'), 'utf8').catch(() => ''));
      if (Number.isInteger(extractorPid) && extractorPid > 0) break;
      await delay(50);
    }
    assert.ok(Number.isInteger(extractorPid) && extractorPid > 0, owner.errors());
    const firstWorkspace = await readFile(firstMarker, 'utf8');
    await assert.rejects(lstat(resolve(output, '.shiori-release-ready')), { code: 'ENOENT' });
    assert.ok(owner.child.kill());
    await owner.exit;
    const alive = () => { try { process.kill(extractorPid, 0); return true; } catch { return false; } };
    for (let attempt = 0; attempt < 100 && alive(); attempt++) await delay(50);
    assert.equal(alive(), false, 'The extractor must not outlive its supervisor');
    successor = start([], secondMarker);
    assert.equal((await successor.exit)[0], 0, successor.errors());
    assert.notEqual(await readFile(secondMarker, 'utf8'), firstWorkspace);
    await writeFile(resolve(firstWorkspace, ...setupParts), 'next build data');
  } finally {
    await writeFile(resolve(base, 'stop'), 'stop');
    for (const process of [owner, successor].filter(Boolean)) {
      if (process.child.exitCode === null && process.child.signalCode === null) process.child.kill();
      await process.exit;
    }
    if (extractorPid) { try { process.kill(extractorPid); } catch {} }
  }
});

test('publication requires the installer payload to match the application', { timeout: 45000 }, async t => {
  const { base, output, files, start, appParts, appSource, setupParts, setupSource } = await installerFixture(t);
  const appOutput = resolve(output, ...appParts);
  const setupOutput = resolve(output, ...setupParts);
  const mismatched = start(files, resolve(base, 'mismatched-workspace'), setupSource);
  assert.equal((await mismatched.exit)[0], 1, mismatched.errors());
  assert.match(mismatched.errors(), /does not contain the application prepared for this build/);
  await assert.rejects(lstat(appOutput), { code: 'ENOENT' });
  await assert.rejects(lstat(setupOutput), { code: 'ENOENT' });

  const verified = start(files, resolve(base, 'verified-workspace'), appSource);
  assert.equal((await verified.exit)[0], 0, verified.errors());
  assert.deepEqual(await readFile(appOutput), await readFile(appSource));
  assert.deepEqual(await readFile(setupOutput), await readFile(setupSource));
});

test('the release callback receives one verified snapshot and its digest', async t => {
  const { root } = await fixture(t);
  let stage;
  await runFrontend(root, false, async ({ id, digest }) => {
    assert.match(id, /^[0-9a-f]{32}$/);
    stage = resolve(root, '.frontend-package-' + id);
    const file = resolve(stage, 'frontend.bundle');
    assert.deepEqual(await readdir(stage), ['frontend.bundle']);
    const contents = await readFile(file);
    assert.equal(contents.subarray(0, 8).toString('latin1'), 'SHIORI\0\x01');
    assert.equal(createHash('sha256').update(contents).digest('hex'), digest);
    await assert.rejects(writeFile(file, 'incomplete output'));
    await assert.rejects(rename(stage, stage + '-moved'));
  });
  assert.ok(stage);
  await assert.rejects(lstat(stage), { code: 'ENOENT' });
});

test('inputs are limited by path, byte count, encoding and file type', async t => {
  const { root, base } = await fixture(t);
  assert.equal(await readProjectText(root, 'index.html'), template);
  for (const name of ['../outside.css', '__proto__', 'C:\\Windows\\win.ini', null]) {
    await assert.rejects(readProjectText(root, name), /Unsupported/);
  }
  for (const bytes of [Buffer.from([0xff]), Buffer.from('x\0y'), Buffer.alloc(65537, 65)]) {
    await writeFile(resolve(root, 'index.html'), bytes);
    await assert.rejects(readProjectText(root, 'index.html'));
  }
  const outside = resolve(base, 'outside.css');
  await writeFile(outside, 'PRIVATE CANARY');
  await unlink(resolve(root, 'src/document.css'));
  await link(outside, resolve(root, 'src/document.css'));
  await assert.rejects(readProjectText(root, 'src/document.css'), /regular file/);
  const source = resolve(root, 'src');
  const original = resolve(root, 'src-original');
  await rename(source, original);
  await symlink(base, source, 'junction');
  await assert.rejects(readProjectText(root, 'src/main.ts'), /regular directory/);
});

test('output paths reject traversal, devices, alternate streams and invalid names', async t => {
  const { root } = await fixture(t);
  const directory = await makePrivateDirectory(root, 'build');
  const asset = outputAsset(root, directory, resolve(directory, 'a $& 日本語.js'));
  assert.equal(asset.url, '/a%20%24%26%20%E6%97%A5%E6%9C%AC%E8%AA%9E.js');
  for (const path of ['../outside.js', 'x.js:stream', 'CON.js', 'COM\u00b9.js', 'CON .js', 'NUL', 'a.js ', 'a.js.', 'sub/x.js', 'a.js\0', 'a".js', '' + 'a'.repeat(256) + '.js']) {
    assert.throws(() => outputAsset(root, directory, resolve(directory, path)), path);
  }
  for (const value of [null, undefined, 42, {}, [], '\\?\\C:\\outside.js']) assert.throws(() => outputAsset(root, directory, value));
  await assert.rejects(makePrivateDirectory(root, '../../other'));
  await assert.rejects(removePrivateDirectory(root, root), /private frontend/);
  for (const path of ['\\\\.\\pipe\\project', '\\\\localhost\\pipe\\project', '\\\\?\\GLOBALROOT\\Device']) {
    assert.throws(() => projectRootKey(path), /project volume/);
  }
});

test('HTML replacements escape attributes and require unique placeholders', () => {
  const urls = { css: '/main.css', script: '/main.js', icon: '/$&.png' };
  const html = renderIndex(template, urls);
  assert.ok(html.includes('/$&amp;.png'));
  assert.ok(!html.includes('/src/main.ts'));
  assert.throws(() => renderIndex(template + '<script type="module" src="/src/main.ts"></script>', urls));
  assert.throws(() => renderIndex(template.replace('</head>', ''), urls));
  for (const css of ['/a"><img/src=x>.css', '/%2e%2e%2fa.css', '/a.js#fragment']) {
    assert.throws(() => renderIndex(template, { ...urls, css }));
  }
  assert.throws(() => renderIndex(template, { ...urls, script: 'https://example.invalid/a.js' }));
});

test('owned outputs reject link injection and preserve the outside file', async t => {
  const { root, base } = await fixture(t);
  const directory = await makePrivateDirectory(root, 'build');
  const outside = resolve(base, 'outside.html');
  await writeFile(outside, 'PRIVATE CANARY');
  await assert.rejects(link(outside, resolve(directory, 'index.html')));
  await atomicWrite(directory, 'index.html', 'NEW INDEX');
  assert.equal(await readFile(outside, 'utf8'), 'PRIVATE CANARY');
  assert.equal(await readFile(resolve(directory, 'index.html'), 'utf8'), 'NEW INDEX');
  for (const name of ['../outside', 'index.html:stream', 'CON', 'x\0y']) await assert.rejects(atomicWrite(directory, name, 'bad'));
  await symlink(base, resolve(directory, 'assets'), 'junction');
  await assert.rejects(writeBundle(root, directory, [{ path: resolve(directory, 'assets/a.js'), contents: Buffer.from('bad') }]), /portable output/);
});

test('publish preserves linked folders and refuses an occupied publish lock', async t => {
  const { root, base } = await fixture(t);
  const previous = resolve(root, 'dist');
  await mkdir(previous);
  const outside = resolve(base, 'outside');
  await mkdir(outside);
  await writeFile(resolve(outside, 'canary'), 'PRIVATE CANARY');
  await link(resolve(outside, 'canary'), resolve(previous, 'hard-link'));
  await symlink(outside, resolve(previous, 'linked-folder'), 'junction');
  const directory = await makePrivateDirectory(root, 'build');
  await atomicWrite(directory, 'index.html', 'NEW INDEX');
  const lockPath = resolve(root, '.frontend-publish.lock');
  await writeFile(lockPath, 'EXISTING LOCK');
  await assert.rejects(publishBundle(root, directory), /publish lock/);
  assert.equal(await readFile(lockPath, 'utf8'), 'EXISTING LOCK');
  await unlink(lockPath);
  await publishBundle(root, directory);
  assert.equal(await readFile(resolve(previous, 'index.html'), 'utf8'), 'NEW INDEX');
  assert.equal(await readFile(resolve(outside, 'canary'), 'utf8'), 'PRIVATE CANARY');
  await assert.rejects(lstat(lockPath), { code: 'ENOENT' });
});

test('parallel publishers expose one complete output', async t => {
  const { root } = await fixture(t);
  const builds = await Promise.all(['ONE', 'TWO'].map(async text => {
    const directory = await makePrivateDirectory(root, 'build');
    await atomicWrite(directory, 'index.html', text);
    return directory;
  }));
  const results = await Promise.allSettled(builds.map(directory => publishBundle(root, directory)));
  assert.ok(results.some(result => result.status === 'fulfilled'));
  assert.ok(['ONE', 'TWO'].includes(await readFile(resolve(root, 'dist/index.html'), 'utf8')));
});

test('failed builds and unsupported CSS imports preserve the previous output', async t => {
  const { root, base } = await fixture(t);
  let result = run(root);
  assert.equal(result.status, 0, result.stderr);
  const original = await readFile(resolve(root, 'dist/index.html'), 'utf8');
  result = run(root);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(await readFile(resolve(root, 'dist/index.html'), 'utf8'), original, 'Private staging names do not change output URLs');
  await writeFile(resolve(root, 'dist/keep.txt'), 'KEEP');
  await writeFile(resolve(root, 'src/main.ts'), 'invalid {{{');
  result = run(root);
  assert.notEqual(result.status, 0);
  assert.equal(await readFile(resolve(root, 'dist/keep.txt'), 'utf8'), 'KEEP');
  assert.equal(await readFile(resolve(root, 'dist/index.html'), 'utf8'), original);
  await writeFile(resolve(base, 'outside.css'), '/* PRIVATE CANARY */');
  await writeFile(resolve(root, 'src/main.ts'), 'import "./preview";');
  await writeFile(resolve(root, 'src/preview.ts'), 'import value from "../../outside.css?inline"; console.log(value);');
  result = run(root);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /Only src\/preview.ts/);
  assert.equal(await readFile(resolve(root, 'dist/keep.txt'), 'utf8'), 'KEEP');
});

test('the bundler rejects linked content at ordinary project inputs', async t => {
  const { root, base } = await fixture(t);
  const outside = resolve(base, 'outside.css');
  await writeFile(outside, '/* outside the project */');
  await unlink(resolve(root, 'src/style.css'));
  await link(outside, resolve(root, 'src/style.css'));
  const result = run(root);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /regular file without links/);
  assert.equal(await readFile(outside, 'utf8'), '/* outside the project */');
  await assert.rejects(lstat(resolve(root, 'dist')), { code: 'ENOENT' });
});

test('an invalid write chunk cancels the write before it can be committed', async t => {
  const { root } = await fixture(t);
  const guard = await filesystem(root);
  const directory = await makePrivateDirectory(root, 'build');
  const id = basename(directory).slice('.frontend-build-'.length);
  const write = await guard.call({ op: 'begin', directory: id, name: 'main.js', size: 2 });
  await guard.call({ op: 'append', write, data: 'YQ==' });
  await assert.rejects(guard.call({ op: 'append', write, data: 'invalid' }));
  await assert.rejects(guard.call({ op: 'commit', write }));
  await assert.rejects(lstat(resolve(directory, 'main.js')), { code: 'ENOENT' });
  await atomicWrite(directory, 'main.js', 'complete');
});

test('untracked or altered outputs preserve dist and sealed files stay readable', async t => {
  const { root } = await fixture(t);
  const directory = await makePrivateDirectory(root, 'build');
  const dist = resolve(root, 'dist');
  const extra = resolve(directory, 'extra.txt');
  const script = resolve(directory, 'main.js');
  await mkdir(dist);
  await writeFile(resolve(dist, 'index.html'), 'PREVIOUS');
  await atomicWrite(directory, 'index.html', 'INDEX');
  await atomicWrite(directory, 'main.js', 'SCRIPT');
  await writeFile(extra, 'UNTRACKED');
  await assert.rejects(publishBundle(root, directory), /files changed/);
  await unlink(extra);
  await writeFile(script, 'EDITED');
  await assert.rejects(sealBundle(root, directory), /files changed/);
  assert.equal(await readFile(resolve(dist, 'index.html'), 'utf8'), 'PREVIOUS');
  await writeFile(script, 'SCRIPT');
  assert.deepEqual(await sealBundle(root, directory), [resolve(directory, 'index.html'), script]);
  assert.equal(await readFile(script, 'utf8'), 'SCRIPT');
  await assert.rejects(writeFile(script, 'EDITED'));
  await assert.rejects(rename(script, resolve(directory, 'renamed.js')));
  await removePrivateDirectory(root, directory);
  await assert.rejects(lstat(directory), { code: 'ENOENT' });
});

test('a junction at dist is refused without touching its target', async t => {
  const { root, base } = await fixture(t);
  const outside = resolve(base, 'outside');
  await mkdir(outside);
  await writeFile(resolve(outside, 'canary'), 'PRIVATE CANARY');
  await symlink(outside, resolve(root, 'dist'), 'junction');
  const result = run(root);
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /regular directory/);
  assert.equal(await readFile(resolve(outside, 'canary'), 'utf8'), 'PRIVATE CANARY');
});

test('owned directory handles block the 0.1.9 parent replacement attack', async t => {
  const { root, base } = await fixture(t);
  const directory = await makePrivateDirectory(root, 'build');
  const outside = resolve(base, 'outside');
  await mkdir(outside);
  await writeFile(resolve(outside, 'index.html'), 'OUTSIDE CANARY');
  await assert.rejects(rename(directory, resolve(root, 'moved-stage')));
  await assert.rejects(symlink(outside, directory, 'junction'));
  await atomicWrite(directory, 'index.html', 'APPROVED OUTPUT');
  assert.equal(await readFile(resolve(outside, 'index.html'), 'utf8'), 'OUTSIDE CANARY');
  assert.equal(await readFile(resolve(directory, 'index.html'), 'utf8'), 'APPROVED OUTPUT');
  await removePrivateDirectory(root, directory);
  await mkdir(directory);
  await writeFile(resolve(directory, 'keep'), 'NEW OWNER');
  await removePrivateDirectory(root, directory);
  assert.equal(await readFile(resolve(directory, 'keep'), 'utf8'), 'NEW OWNER');
});
