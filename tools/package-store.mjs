import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync } from 'node:fs';
import { copyFile, mkdir, mkdtemp, open, readFile, realpath, rename, rm, writeFile } from 'node:fs/promises';
import { basename, dirname, isAbsolute, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const manifestDirectory = resolve(root, 'src-tauri/windows/store');
const logoFiles = ['Square44x44Logo.png', 'Square150x150Logo.png', 'StoreLogo.png'];
const machines = { x64: 0x8664, arm64: 0xaa64 };
const appFile = 'shiori-mde.exe';

function findMakeAppx() {
  const configured = process.env.SHIORI_MAKEAPPX;
  if (configured) {
    if (!isAbsolute(configured) || basename(configured).toLowerCase() !== 'makeappx.exe' || !existsSync(configured)) {
      throw new Error('SHIORI_MAKEAPPX must point to a Windows SDK MakeAppx.exe');
    }
    return configured;
  }
  const programFiles = process.env['ProgramFiles(x86)'] ?? process.env.ProgramFiles;
  const sdk = programFiles && resolve(programFiles, 'Windows Kits/10/bin');
  if (sdk && existsSync(sdk)) {
    const versions = readdirSync(sdk, { withFileTypes: true })
      .filter(entry => entry.isDirectory() && /^\d+(?:\.\d+){3}$/.test(entry.name))
      .map(entry => entry.name)
      .sort((first, second) => second.localeCompare(first, 'en', { numeric: true }));
    for (const version of versions) {
      for (const arch of new Set([process.arch, 'x64', 'x86'])) {
        const path = resolve(sdk, version, arch, 'MakeAppx.exe');
        if (existsSync(path)) return path;
      }
    }
  }
  throw new Error('Install the Windows SDK or set SHIORI_MAKEAPPX to the full path of MakeAppx.exe');
}

async function readIdentity(file) {
  const identity = JSON.parse((await readFile(file, 'utf8')).replace(/^\uFEFF/, ''));
  for (const key of ['name', 'publisher', 'publisherDisplayName', 'displayName', 'version']) {
    if (typeof identity?.[key] !== 'string' || !identity[key].trim() || /[\x00-\x1F]/.test(identity[key])) {
      throw new Error(`Set ${key} in the Store identity JSON`);
    }
  }
  if (!/^[a-z0-9.-]{3,50}$/i.test(identity.name)) throw new Error('Invalid Store package identity name');
  if (!/^[1-9]\d{0,4}(?:\.(?:0|[1-9]\d{0,4})){2}\.0$/.test(identity.version)
      || identity.version.split('.').some(part => Number(part) > 65535)) {
    throw new Error('Store package version must have a nonzero major version and a fourth part of 0');
  }
  return identity;
}

function xml(value) {
  const escapes = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&apos;' };
  return value.replace(/[&<>"']/g, character => escapes[character]);
}

async function verifyMachine(file, architecture) {
  const handle = await open(file, 'r');
  try {
    const dos = Buffer.alloc(64);
    const pe = Buffer.alloc(6);
    const dosRead = await handle.read(dos, 0, dos.length, 0);
    if (dosRead.bytesRead === dos.length && dos.readUInt16LE(0) === 0x5a4d) {
      const peRead = await handle.read(pe, 0, pe.length, dos.readUInt32LE(0x3c));
      if (peRead.bytesRead === pe.length && pe.readUInt32LE(0) === 0x4550
          && pe.readUInt16LE(4) === machines[architecture]) return;
    }
    throw new Error(`Expected a ${architecture} application executable`);
  } finally {
    await handle.close();
  }
}

const [identityFile, x64Binary, arm64Binary, ...extra] = process.argv.slice(2);
if (process.platform !== 'win32' || !identityFile || !x64Binary || !arm64Binary || extra.length) {
  throw new Error('Usage on Windows: npm run package:store -- <identity.json> <x64 shiori-mde.exe> <ARM64 shiori-mde.exe>');
}
const identity = await readIdentity(identityFile);
const makeAppx = findMakeAppx();
const template = await readFile(resolve(manifestDirectory, 'AppxManifest.xml'), 'utf8');
const outputPath = resolve(root, 'src-tauri/target/store');
await mkdir(outputPath, { recursive: true });
const output = await realpath(outputPath);
const outputRelative = relative(await realpath(root), output);
if (!outputRelative || outputRelative === '..' || outputRelative.startsWith('..' + sep) || isAbsolute(outputRelative)) {
  throw new Error('Store output must be inside the project');
}
const stage = await mkdtemp(resolve(output, '.store-build-'));
const packages = resolve(stage, 'packages');
const bundleName = `${identity.name}_${identity.version}.msixbundle`;

try {
  await mkdir(packages);
  for (const [architecture, binary] of Object.entries({ x64: x64Binary, arm64: arm64Binary })) {
    if (basename(binary).toLowerCase() !== appFile) {
      throw new Error(`Expected ${appFile} from the ${architecture} installer build`);
    }
    const payload = resolve(stage, architecture);
    const assets = resolve(payload, 'Assets');
    await mkdir(assets, { recursive: true });
    await copyFile(binary, resolve(payload, appFile));
    await verifyMachine(resolve(payload, appFile), architecture);
    for (const logo of logoFiles) {
      await copyFile(resolve(manifestDirectory, 'Assets', logo), resolve(assets, logo));
    }
    const values = { ...identity, architecture };
    const manifest = template.replace(/\{\{(\w+)\}\}/g, (_, key) => xml(values[key]));
    await writeFile(resolve(payload, 'AppxManifest.xml'), manifest, 'utf8');
    execFileSync(makeAppx, ['pack', '/d', payload, '/p', resolve(packages, `${architecture}.msix`), '/no'], {
      stdio: 'inherit', windowsHide: true,
    });
  }
  const bundle = resolve(stage, bundleName);
  execFileSync(makeAppx, ['bundle', '/d', packages, '/p', bundle, '/bv', identity.version, '/no'], {
    stdio: 'inherit', windowsHide: true,
  });
  const destination = resolve(output, bundleName);
  await rename(bundle, destination);
  console.log(`栞: ${destination}`);
} finally {
  const directory = await realpath(stage);
  if (dirname(directory) !== output || !basename(directory).startsWith('.store-build-')) {
    throw new Error('Unexpected Store staging path');
  }
  await rm(directory, { recursive: true, force: true });
}
