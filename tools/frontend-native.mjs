import { execFile, spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { constants, unlinkSync } from 'node:fs';
import { copyFile, lstat, stat } from 'node:fs/promises';
import { delimiter, isAbsolute, parse, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const toolRoot = fileURLToPath(new URL('..', import.meta.url));
const sessions = new Map();
const MAX_FRAME = 1024 * 1024;
const MAX_REPLY = 16 * MAX_FRAME;
const utf8 = new TextDecoder('utf-8', { fatal: true });
const exec = promisify(execFile);
let helperBuild;

export function projectRootKey(root) {
  if (typeof root !== 'string' || !isAbsolute(root) || root.length > 32767 || /[\u0000-\u001f\u007f-\u009f]/.test(root)) {
    throw new Error('Invalid project root');
  }
  const path = resolve(root);
  const volume = parse(path).root.replace(/^\\\\\?\\UNC\\/i, '\\\\').replace(/^\\\\\?\\/, '');
  if (!/^[a-z]:\\$/i.test(volume)) {
    const share = /^\\\\([^\\]+)\\([^\\]+)\\?$/.exec(volume);
    if (!share || share.slice(1).some(part => /[<>:"/|?*]/.test(part) || /[. ]$/.test(part))
        || /^(?:pipe|mailslot)$/i.test(share[2])) throw new Error('Unsupported project volume');
  }
  // Windows directories can be case-sensitive. Keep distinct roots separate.
  return path;
}

async function cargoPath() {
  for (const entry of (process.env.Path ?? '').split(delimiter)) {
    const directory = entry.replace(/^"(.*)"$/, '$1');
    if (!isAbsolute(directory)) continue;
    const file = resolve(directory, 'cargo.exe');
    try {
      const metadata = await stat(file);
      if (metadata.isFile()) return file;
    } catch (error) {
      if (error.code !== 'ENOENT' && error.code !== 'ENOTDIR') throw error;
    }
  }
  throw new Error('cargo.exe was not found in an absolute PATH directory');
}

export function ensureFilesystemHelper() {
  if (process.platform !== 'win32' || !['x64', 'arm64'].includes(process.arch)) {
    throw new Error('Frontend filesystem operations require Windows x64 or ARM64');
  }
  helperBuild ??= (async () => {
    const target = process.arch === 'arm64' ? 'aarch64-pc-windows-msvc' : 'x86_64-pc-windows-msvc';
    const targetDirectory = resolve(toolRoot, '.build-tools/frontend-fs-target');
    const child = spawn(await cargoPath(), ['build', '--locked', '--manifest-path',
      resolve(toolRoot, 'tools/frontend-fs/Cargo.toml'), '--target', target], {
      cwd: toolRoot, shell: false, windowsHide: true, stdio: ['ignore', 'ignore', 'pipe'],
      env: { ...process.env, CARGO_TARGET_DIR: targetDirectory, CARGO_INCREMENTAL: '0' },
    });
    let errors = '';
    child.stderr.on('data', data => { errors = (errors + data).slice(-65536); });
    await new Promise((done, fail) => {
      child.once('error', fail);
      child.once('close', code => code === 0 ? done() : fail(new Error('Could not build filesystem guard: ' + errors)));
    });
    // Windows keeps running images open. Give this caller its own copy so
    // Cargo can replace the compiled executable even during a release job.
    const compiled = resolve(targetDirectory, target, 'debug/shiori-frontend-fs.exe');
    const runnable = resolve(targetDirectory, target, 'debug', `.shiori-frontend-fs-${randomUUID()}.exe`);
    await copyFile(compiled, runnable, constants.COPYFILE_EXCL);
    process.once('exit', () => {
      try { unlinkSync(runnable); } catch { /* A detached supervisor may still be using it. */ }
    });
    return runnable;
  })().catch(error => {
    helperBuild = undefined;
    throw error;
  });
  return helperBuild;
}

class Session {
  constructor(child) {
    this.child = child;
    this.queue = [];
    this.parts = [];
    this.replyBytes = 0;
    this.errors = '';
    this.closed = false;
    this.closing = undefined;
    this.ready = this.response('initialize');
    this.done = new Promise(resolve => {
      child.once('close', (code, signal) => {
        this.fail(new Error('Filesystem guard exited (' + (code ?? signal) + '): ' + this.errors));
        resolve(code);
      });
    });
    child.stderr.on('data', data => { this.errors = (this.errors + data).slice(-65536); });
    child.stdout.on('data', data => {
      try { this.readReplies(data); }
      catch (error) { this.terminate(error); }
    });
    child.on('error', error => this.fail(error));
    child.stdin.on('error', error => this.terminate(error));
    child.stdout.on('error', error => this.terminate(error));
  }
  readReplies(data) {
    while (data.length && !this.closed) {
      const end = data.indexOf(10);
      const part = end < 0 ? data : data.subarray(0, end);
      this.replyBytes += part.length;
      if (this.replyBytes > MAX_REPLY) throw new Error('Filesystem reply exceeds limit');
      this.parts.push(part);
      if (end < 0) return;
      const response = JSON.parse(utf8.decode(Buffer.concat(this.parts, this.replyBytes)));
      this.parts = [];
      this.replyBytes = 0;
      const pending = this.queue[0];
      if (!pending || !response || typeof response !== 'object'
          || !(response.ok === true && Object.hasOwn(response, 'value')
            || response.ok === false && typeof response.error === 'string')) {
        throw new Error('Invalid filesystem reply');
      }
      this.queue.shift();
      if (response.ok) pending.resolve(response.value);
      else pending.reject(new Error('Filesystem ' + pending.operation + ': ' + response.error.slice(0, 8192)));
      data = data.subarray(end + 1);
    }
  }
  fail(error) {
    this.closed = true;
    this.parts = [];
    for (const pending of this.queue.splice(0)) pending.reject(error);
  }
  terminate(error) {
    this.fail(error);
    this.child.kill();
  }
  response(operation) {
    return new Promise((resolve, reject) => this.queue.push({ resolve, reject, operation }));
  }
  async call(request) {
    if (this.closed || this.closing) throw new Error('Filesystem guard is closed');
    if (this.queue.length >= 64) throw new Error('Too many pending filesystem requests');
    const message = JSON.stringify(request) + '\n';
    if (Buffer.byteLength(message) > MAX_FRAME) throw new Error('Filesystem request exceeds limit');
    const result = this.response(request.op);
    this.child.stdin.write(message, error => { if (error) this.terminate(error); });
    return result;
  }
  close() {
    this.closing ??= (async () => {
      if (this.closed) {
        await this.done;
        return;
      }
      let code;
      try {
        await this.call({ op: 'close' });
        this.child.stdin.end();
      } catch (error) {
        this.terminate(error);
        throw error;
      } finally {
        code = await this.done;
      }
      if (code !== 0) throw new Error('Filesystem guard failed to close: ' + this.errors);
      if (this.errors) console.warn(this.errors.trimEnd());
    })();
    return this.closing;
  }
}
export async function filesystem(root) {
  const sessionKey = projectRootKey(root);
  let pending = sessions.get(sessionKey);
  if (!pending) {
    pending = (async () => {
      // Capture the requested directory before the potentially slow Cargo build.
      const identity = await lstat(sessionKey, { bigint: true });
      if (!identity.isDirectory() || identity.isSymbolicLink()) throw new Error('Expected a regular project directory');
      const binary = await ensureFilesystemHelper();
      const child = spawn(binary, [sessionKey, String(identity.dev), String(identity.ino), String(process.pid)], {
        shell: false, windowsHide: true, stdio: ['pipe', 'pipe', 'pipe'],
      });
      const session = new Session(child);
      try {
        const ready = await session.ready;
        if (ready?.version !== 2 || typeof ready.root !== 'string' || relative(sessionKey, ready.root) !== '') {
          throw new Error('Filesystem guard project identity mismatch');
        }
        return session;
      } catch (error) {
        session.terminate(error);
        await session.done;
        throw error;
      }
    })();
    sessions.set(sessionKey, pending);
  }
  return pending;
}
export async function closeFilesystem(root) {
  const sessionKey = projectRootKey(root);
  const pending = sessions.get(sessionKey);
  if (!pending) return;
  try {
    const session = await pending.catch(() => null);
    await session?.close();
  } finally {
    sessions.delete(sessionKey);
  }
}

export async function runReleaseBuild(root, args, { id, digest }, artifacts) {
  // Cargo resolves the configured target directory relative to this crate.
  const { stdout } = await exec(await cargoPath(), [
    'metadata', '--no-deps', '--locked', '--format-version', '1',
  ], { cwd: resolve(root, 'src-tauri'), maxBuffer: 2 * MAX_FRAME, windowsHide: true });
  const output = projectRootKey(JSON.parse(stdout).target_directory);
  console.log('栞: waiting for release output lock (' + output + ')');
  const helper = await ensureFilesystemHelper();
  const worker = resolve(root, 'tools/package-windows-worker.mjs');
  const child = spawn(helper, ['--release', String(process.pid), output, process.execPath, worker, JSON.stringify(artifacts), ...args], {
    cwd: root, detached: true, shell: false, windowsHide: true, stdio: 'inherit',
    env: { ...process.env, SHIORI_FRONTEND_SNAPSHOT: id, SHIORI_FRONTEND_SHA256: digest },
  });
  await new Promise((done, fail) => {
    child.once('error', fail);
    child.once('close', (code, signal) => code === 0 ? done() : fail(new Error('Packaging failed (' + (code ?? signal) + ')')));
  });
}
