// The native supervisor assigns this process to the build job before opening the gate.
let started = false;
for await (const chunk of process.stdin) {
  if (started || chunk.length !== 1 || chunk[0] !== 1) throw new Error('Invalid build gate');
  started = true;
}
if (!started) throw new Error('Packaging must run under the release supervisor');

const { default: cli } = await import('@tauri-apps/cli');
await cli.run(process.argv.slice(2));
