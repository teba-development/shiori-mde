import { fileURLToPath } from 'node:url';
import tauriConfig from '../src-tauri/tauri.conf.json' with { type: 'json' };
import { runFrontend } from './frontend-build.mjs';
import { runReleaseBuild } from './frontend-native.mjs';

const [arch, kind, ...flags] = process.argv.slice(2);
if (!arch) {
  throw new Error('Use npm run build:x64, build:arm64, build:portable:x64 or build:portable:arm64 to prepare the verified frontend');
}
if (!['x64', 'arm64'].includes(arch) || !['setup', 'portable'].includes(kind)
    || flags.some(flag => !['--ci', '--verbose', '--debug'].includes(flag))) {
  throw new Error('Usage: node tools/package-windows.mjs <x64|arm64> <setup|portable> [--ci] [--verbose] [--debug]');
}
const root = fileURLToPath(new URL('..', import.meta.url));
process.chdir(root);
const target = arch === 'arm64' ? 'aarch64-pc-windows-msvc' : 'x86_64-pc-windows-msvc';
const output = [target, flags.includes('--debug') ? 'debug' : 'release'];
const artifacts = kind === 'portable'
  ? [[...output, 'shiori-mde-portable.exe']]
  : [[...output, 'shiori-mde.exe'],
    [...output, 'bundle', 'nsis', `${tauriConfig.productName}_${tauriConfig.version}_${arch}-setup.exe`]];

await runFrontend(root, false, async (snapshot) => {
  const config = JSON.stringify({ build: { beforeBuildCommand: null } });
  const args = ['build', '--target', target, '--features', 'custom-protocol', ...flags];
  if (kind === 'portable') args.push('--config', 'src-tauri/windows/portable.json');
  // Rust verifies the snapshot and embeds the bytes it checked directly.
  await runReleaseBuild(root, [...args, '--config', config], snapshot, artifacts);
});
