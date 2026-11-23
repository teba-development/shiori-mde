import { fileURLToPath } from 'node:url';
import { runFrontend } from './frontend-build.mjs';
const args = process.argv.slice(2);
if (args.length > 1 || (args.length === 1 && args[0] !== '--serve')) {
  throw new Error('Usage: node tools/frontend.mjs [--serve]');
}
const root = fileURLToPath(new URL('..', import.meta.url));
await runFrontend(root, args[0] === '--serve');
