import { build, context } from 'esbuild';
import { relative, resolve } from 'node:path';
import {
  atomicWrite, closeFrontendFilesystem, inspectBundle, loadProjectSource, makePrivateDirectory, outputAsset,
  publishBundle, readProjectText, renderIndex, writeBundle,
} from './frontend-io.mjs';
import { prepareFrontendSnapshot } from './frontend-snapshot.mjs';

export async function runFrontend(root, development = false, packageAssets) {
  if (typeof development !== 'boolean' || (packageAssets !== undefined && (development || typeof packageAssets !== 'function'))) {
    throw new Error('Invalid frontend mode');
  }
  let failed = false;
  try {
    const entryPoint = resolve(root, 'src/main.ts');
    const templatePath = resolve(root, 'index.html');
    const inlinePath = resolve(root, 'src/document.css');
    const developmentLinks = { script: '/main.js', css: '/main.css', icon: '/shiori-bookmark-v1.png' };
    const tsconfig = JSON.parse(await readProjectText(root, 'tsconfig.json'));
    if (!tsconfig?.compilerOptions || typeof tsconfig.compilerOptions !== 'object'
        || Array.isArray(tsconfig.compilerOptions) || Object.hasOwn(tsconfig, 'extends')) {
      throw new Error('Expected a standalone frontend tsconfig with compilerOptions');
    }
    const outputDirectory = await makePrivateDirectory(root, development ? 'dev' : packageAssets ? 'package' : 'build');
    let snapshot;

    const html = {
      name: 'shiori-html',
      setup(builder) {
        if (development) {
          builder.onResolve({ filter: /^shiori:index\.html$/ }, () => ({ path: 'index.html', namespace: 'shiori-index' }));
          builder.onLoad({ filter: /.*/, namespace: 'shiori-index' }, async () => ({
            contents: Buffer.from(renderIndex(await readProjectText(root, 'index.html'), developmentLinks)),
            loader: 'copy', watchFiles: [templatePath],
          }));
        }
        builder.onResolve({ filter: /\.css\?inline$/ }, args => {
          if (args.path !== './document.css?inline' || args.kind !== 'import-statement'
              || resolve(args.importer) !== resolve(root, 'src/preview.ts')) {
            throw new Error('Only src/preview.ts may import ./document.css?inline');
          }
          return { path: inlinePath, namespace: 'shiori-inline-css' };
        });
        builder.onLoad({ filter: /.*/, namespace: 'shiori-inline-css' }, async args => {
          if (args.path !== inlinePath) throw new Error('Invalid inline stylesheet');
          return { contents: await readProjectText(root, 'src/document.css'), loader: 'text', watchFiles: [inlinePath] };
        });
        builder.onLoad({ filter: /.*/, namespace: 'file' }, args => loadProjectSource(root, args.path));
        builder.onEnd(async result => {
          if (result.errors.length) return;
          const outputs = Object.entries(result.metafile.outputs);
          const entry = outputs.find(([, item]) => item.entryPoint
            && relative(entryPoint, resolve(root, item.entryPoint)) === '');
          const icon = outputs.find(([path, item]) => path.endsWith('.png')
            && Object.hasOwn(item.inputs, 'assets/shiori-bookmark-v1.png'));
          if (!entry || !entry[1].cssBundle || !icon) throw new Error('Missing frontend entry, styles or icon');
          const links = {
            script: outputAsset(root, outputDirectory, entry[0]),
            css: outputAsset(root, outputDirectory, entry[1].cssBundle),
            icon: outputAsset(root, outputDirectory, icon[0]),
          };
          let paths;
          if (development) {
            const indexPath = resolve(outputDirectory, 'index.html');
            const index = result.outputFiles.find(file => relative(indexPath, file.path) === '');
            if (!index || index.contents.byteLength > 128 * 1024
                || Object.entries(links).some(([name, asset]) => asset.url !== developmentLinks[name])) {
              throw new Error('Invalid in-memory frontend index');
            }
            paths = inspectBundle(root, outputDirectory, result.outputFiles.filter(file => file !== index)).paths;
          } else {
            const template = await readProjectText(root, 'index.html');
            const index = renderIndex(template, Object.fromEntries(Object.entries(links).map(([name, asset]) => [name, asset.url])));
            if (packageAssets) {
              const validated = inspectBundle(root, outputDirectory, result.outputFiles);
              paths = validated.paths;
              snapshot = await prepareFrontendSnapshot(root, outputDirectory, validated.assets, index);
            } else {
              paths = await writeBundle(root, outputDirectory, result.outputFiles);
              await atomicWrite(outputDirectory, 'index.html', index);
            }
          }
          if (Object.values(links).some(asset => !paths.has(asset.path.toLowerCase()))) {
            throw new Error('HTML references a missing frontend asset');
          }
        });
      },
    };

    const options = {
      absWorkingDir: root,
      entryPoints: development ? { main: entryPoint, index: 'shiori:index.html' } : [entryPoint],
      outdir: outputDirectory,
      entryNames: development ? '[name]' : '[name]-[hash]',
      chunkNames: 'chunk-[hash]',
      assetNames: development ? '[name]' : '[name]-[hash]',
      publicPath: '/',
      bundle: true,
      splitting: true,
      format: 'esm',
      platform: 'browser',
      target: 'chrome110',
      tsconfigRaw: { compilerOptions: tsconfig.compilerOptions },
      preserveSymlinks: true,
      loader: { '.png': 'file' },
      minify: !development,
      sourcemap: development,
      metafile: true,
      write: false,
      legalComments: 'linked',
      define: { 'process.env.NODE_ENV': JSON.stringify(development ? 'development' : 'production') },
      plugins: [html],
      logLevel: 'warning',
    };

    let compiler;
    try {
      if (development) {
        compiler = await context(options);
        await compiler.watch();
        const server = await compiler.serve({ host: '127.0.0.1', port: 1420 });
        if (server.port !== 1420) throw new Error('Development port 1420 is unavailable');
        console.log('栞: http://127.0.0.1:1420/ (watching source files)');
        await new Promise(resolve => {
          process.once('SIGINT', resolve);
          process.once('SIGTERM', resolve);
        });
      } else {
        const result = await build(options);
        if (packageAssets) {
          if (!snapshot) throw new Error('Missing verified frontend snapshot');
          await packageAssets(snapshot);
        } else {
          await publishBundle(root, outputDirectory);
        }
        console.log('栞: built frontend (' + Object.keys(result.metafile.outputs).length + ' assets)');
      }
    } finally {
      await compiler?.dispose();
    }
  } catch (error) {
    failed = true;
    throw error;
  } finally {
    // The helper releases read handles and removes the directories it still
    // owns on close. Preserve the build error if closing also fails.
    try {
      await closeFrontendFilesystem(root);
    } catch (error) {
      if (!failed) throw error;
      console.warn('Could not close frontend filesystem:', error.message);
    }
  }
}
