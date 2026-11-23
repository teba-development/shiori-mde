import { toHtml } from 'hast-util-to-html';
import type { Root as HtmlRoot, RootContent as HtmlNode } from 'hast';
import type { Code, Definition, Heading as MdHeading, Image, ImageReference, Root, RootContent } from 'mdast';
import { parseFragment, type DefaultTreeAdapterTypes } from 'parse5';
import rehypeRaw from 'rehype-raw';
import remarkGfm from 'remark-gfm';
import remarkParse from 'remark-parse';
import remarkRehype from 'remark-rehype';
import { unified } from 'unified';
import { escapeHtml } from './platform';
import { MAX_DOCUMENT_BYTES, MAX_TREE_DEPTH, MAX_TREE_NODES, validateCodeLanguage, validateMarkdown } from './security';

const parser = unified().use(remarkParse).use(remarkGfm);
const renderer = unified().use(remarkRehype, { allowDangerousHtml: true }).use(rehypeRaw);

export interface Heading {
  line: number;
  level: number;
  label: string;
  id: string;
}

export function walk(root: Root | RootContent, visit: (node: Root | RootContent) => void) {
  const stack = [{ node: root, depth: 0 }];
  let visited = 0;
  while (stack.length) {
    const { node, depth } = stack.pop()!;
    if (++visited > MAX_TREE_NODES || depth > MAX_TREE_DEPTH) {
      throw new Error('Markdownの構造が複雑すぎます。');
    }
    visit(node);
    if (!('children' in node)) continue;
    if (visited + stack.length + node.children.length > MAX_TREE_NODES) {
      throw new Error('Markdownの構造が複雑すぎます。');
    }
    for (let index = node.children.length - 1; index >= 0; index -= 1) {
      stack.push({ node: node.children[index], depth: depth + 1 });
    }
  }
}

export function definitionsOf(root: Root) {
  const definitions = new Map<string, Definition>();
  walk(root, (node) => {
    if (node.type === 'definition' && !definitions.has(node.identifier)) {
      definitions.set(node.identifier, node);
    }
  });
  return definitions;
}

export function parseMarkdown(markdown: string) {
  validateMarkdown(markdown);
  const tree = parser.parse(markdown);
  walk(tree, () => undefined);
  return tree;
}

export function codeLanguageChange(markdown: string, block: Code, language: string) {
  validateCodeLanguage(language);
  const from = block.position?.start.offset;
  const end = block.position?.end.offset;
  if (from === undefined || end === undefined || !Number.isSafeInteger(from) || !Number.isSafeInteger(end)
      || from < 0 || end <= from || end > markdown.length) {
    throw new Error('コードブロックの位置を確認できませんでした。');
  }
  const lineEnd = markdown.indexOf('\n', from);
  const to = lineEnd < 0 ? markdown.length : lineEnd;
  // A fenced node starts at its marker; an indented node starts at whitespace.
  // Treating a tab as one space would overwrite literal fences in indented code.
  const opening = /^(`{3,}|~{3,})([^\r\n]*)$/.exec(markdown.slice(from, to));
  if (opening) {
    const info = /^([ \t]*)([^ \t]*)([\s\S]*)$/.exec(opening[2])!;
    const metadata = block.meta ? info[3] : '';
    return {
      from: from + opening[1].length,
      to,
      insert: (info[1] || '') + (language || (block.meta ? 'text' : '')) + metadata,
    };
  }

  // Indented code becomes a fenced block inside its original list or quote.
  const lineStart = markdown.lastIndexOf('\n', from - 1) + 1;
  const prefix = markdown.slice(lineStart, from)
    .replace(/(?:[-+*]|\d+[.)])([ \t]+)/g, (marker) => ' '.repeat(marker.length));
  let backticks = 3;
  let tildes = 3;
  for (const [run] of block.value.matchAll(/`+|~+/g)) {
    if (run[0] === '`') backticks = Math.max(backticks, run.length + 1);
    else tildes = Math.max(tildes, run.length + 1);
  }
  const fenceLength = Math.min(backticks, tildes);
  let lines = 1;
  for (const _line of block.value.matchAll(/\n/g)) lines += 1;
  const length = markdown.length - (end - from) + 2 * fenceLength + language.length + 2
    + block.value.length + prefix.length * (lines + 1);
  if (length > MAX_DOCUMENT_BYTES) throw new Error('Markdownファイルは64 MiBまでです。');
  const fence = (backticks <= tildes ? '`' : '~').repeat(fenceLength);
  const body = block.value.split('\n').map((line) => prefix + line).join('\n');
  return { from, to: end, insert: `${fence}${language}\n${body}\n${prefix}${fence}` };
}

function textOf(node: MdHeading | RootContent): string {
  if (node.type === 'image' || node.type === 'imageReference') return node.alt ?? '';
  if ('children' in node) return node.children.map(textOf).join('');
  if (node.type === 'text' || node.type === 'inlineCode') return node.value;
  return '';
}

export function outlineOf(root: Root) {
  const headings: Heading[] = [];
  const ids = new Set<string>();
  const nextSuffix = new Map<string, number>();
  walk(root, (node) => {
    if (node.type !== 'heading' || !node.position || headings.length >= 5000) return;
    const label = textOf(node).trim() || '無題の見出し';
    const stem = label.slice(0, 512).toLowerCase().replace(/[^\p{L}\p{N}\p{M}\s_-]/gu, '').replace(/\s/g, '-') || 'heading';
    let id = stem;
    let suffix = nextSuffix.get(stem) ?? 1;
    while (ids.has(id)) id = `${stem}-${suffix++}`;
    nextSuffix.set(stem, suffix);
    ids.add(id);
    headings.push({ label, id, line: node.position.start.line, level: node.depth });
  });
  return headings;
}

export function renderHtml(root: Root, headings: Heading[]) {
  const documentTree = renderer.runSync(root) as HtmlRoot;
  const byLine = new Map(headings.map((heading) => [heading.line, heading]));
  const blocks = new Set([
    'p', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'pre', 'li', 'blockquote', 'table', 'tr',
    'img', 'hr', 'div', 'section', 'svg',
  ]);
  const stack: { node: HtmlRoot | HtmlNode; depth: number }[] = [{ node: documentTree, depth: 0 }];
  let visited = 0;
  while (stack.length) {
    const { node, depth } = stack.pop()!;
    if (++visited > MAX_TREE_NODES || depth > MAX_TREE_DEPTH) {
      throw new Error('プレビューの構造が複雑すぎます。');
    }
    if (node.type === 'element') delete node.properties.dataSourceLine;
    if (node.type === 'element' && node.position) {
      const line = node.position.start.line;
      if (blocks.has(node.tagName)) node.properties.dataSourceLine = line;
      const heading = /^h[1-6]$/.test(node.tagName) && byLine.get(line);
      if (heading) node.properties.id = `shiori-heading-${heading.id}`;
    }
    if (!('children' in node)) continue;
    if (visited + stack.length + node.children.length > MAX_TREE_NODES) {
      throw new Error('プレビューの構造が複雑すぎます。');
    }
    for (const child of node.children) stack.push({ node: child, depth: depth + 1 });
  }
  return toHtml(documentTree);
}

export function relativeImage(source: string) {
  return !!source && !/^(?:[a-z][a-z0-9+.-]*:|[\\/#])/i.test(source);
}

export function imageMarkdown(alt: string, source: string, title?: string | null) {
  const label = alt.replace(/[\\[\]]/g, '\\$&').replace(/[\r\n]+/g, ' ');
  const destination = source.replace(/[<>\r\n\\]/g, (character) => encodeURIComponent(character));
  const tooltip = title ? ` "${title.replace(/[\\"]/g, '\\$&')}"` : '';
  return `![${label}](<${destination}>${tooltip})`;
}

export function svgData(svg: string) {
  if (svg.length > 4 * 1024 * 1024) throw new Error('Mermaid図のSVG出力は4 MiBまでです。');
  const bytes = new TextEncoder().encode(svg);
  if (bytes.byteLength > 4 * 1024 * 1024) throw new Error('Mermaid図のSVG出力は4 MiBまでです。');
  let binary = '';
  for (let offset = 0; offset < bytes.length; offset += 8192) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 8192));
  }
  return `data:image/svg+xml;base64,${btoa(binary)}`;
}

interface Replacement {
  start: number;
  end: number;
  value: () => Promise<string>;
}

function htmlImages(html: string) {
  const results: { source: string; start: number; end: number }[] = [];
  const stack: (DefaultTreeAdapterTypes.ChildNode | DefaultTreeAdapterTypes.DocumentFragment)[] = [
    parseFragment(html, { sourceCodeLocationInfo: true }),
  ];
  let visited = 0;
  while (stack.length) {
    const node = stack.pop()!;
    if (++visited > MAX_TREE_NODES) throw new Error('HTMLの構造が複雑すぎます。');
    if ('tagName' in node && node.tagName === 'img') {
      const source = node.attrs.find(({ name }) => name === 'src')?.value;
      const location = node.sourceCodeLocation?.attrs?.src;
      if (source && location) {
        results.push({ source, start: location.startOffset, end: location.endOffset });
      }
    }
    if (!('childNodes' in node)) continue;
    if (visited + stack.length + node.childNodes.length > MAX_TREE_NODES) {
      throw new Error('HTMLの構造が複雑すぎます。');
    }
    for (const child of node.childNodes) stack.push(child);
  }
  return results;
}

export async function prepareMarkdown(
  markdown: string,
  image: ((source: string) => Promise<string>) | undefined,
  diagram: ((source: string) => Promise<string>) | undefined,
) {
  validateMarkdown(markdown);
  if (!image && !diagram) return markdown;
  const tree = parseMarkdown(markdown);
  const definitions = definitionsOf(tree);
  const replacements: Replacement[] = [];
  let diagrams = 0;
  walk(tree, (node) => {
    const start = node.position?.start.offset;
    const end = node.position?.end.offset;
    if (start === undefined || end === undefined) return;
    if (diagram && node.type === 'code' && isMermaid(node)) {
      if (++diagrams > 64) throw new Error('一度に変換できるMermaid図は64件までです。');
      replacements.push({ start, end, value: async () => imageMarkdown('Mermaid 図', await diagram(node.value)) });
    }
    if (!image) return;
    if (node.type === 'image' || node.type === 'imageReference') {
      const reference = node.type === 'image' ? node : definitions.get(node.identifier);
      const source = reference?.url;
      if (source && relativeImage(source)) {
        replacements.push({
          start, end,
          value: () => image(source).then((url) => source === url
            ? markdown.slice(start, end)
            : imageMarkdown(node.alt ?? '', url, reference.title)),
        });
      }
    } else if (node.type === 'html') {
      // Blockquotes and lists may strip indentation from the parsed HTML value.
      // Parse the original slice so attribute offsets still refer to the file.
      for (const item of htmlImages(markdown.slice(start, end))) {
        if (!relativeImage(item.source)) continue;
        const from = start + item.start;
        const to = start + item.end;
        replacements.push({
          start: from, end: to,
          value: () => image(item.source).then((url) => url === item.source
            ? markdown.slice(from, to)
            : `src="${escapeHtml(url)}"`),
        });
      }
    }
  });

  const ordered = replacements.sort((a, b) => a.start - b.start);
  const chunks: string[] = [];
  let offset = 0;
  let length = markdown.length;
  for (const change of ordered) {
    if (change.start < offset || change.end < change.start || change.end > markdown.length) {
      throw new Error('Markdownの置換位置を確認できませんでした。');
    }
    const replacement = await change.value();
    length += replacement.length - (change.end - change.start);
    if (length > MAX_DOCUMENT_BYTES) throw new Error('埋め込み後のMarkdownは64 MiBまでです。');
    chunks.push(markdown.slice(offset, change.start), replacement);
    offset = change.end;
  }
  chunks.push(markdown.slice(offset));
  const result = chunks.join('');
  validateMarkdown(result);
  return result;
}

export function isMermaid(node: Code) {
  return node.lang?.toLowerCase() === 'mermaid';
}

export function expandReferences() {
  return (tree: Root) => {
    const definitions = definitionsOf(tree);
    const used = new Set<string>();
    walk(tree, (parent) => {
      if (!('children' in parent)) return;
      const children = parent.children as RootContent[];
      children.forEach((child, index) => {
        if (child.type !== 'imageReference' && child.type !== 'linkReference') return;
        const definition = definitions.get(child.identifier);
        if (!definition) return;
        used.add(child.identifier);
        const attributes = { url: definition.url, title: definition.title };
        children[index] = child.type === 'imageReference'
          ? { ...attributes, type: 'image', alt: (child as ImageReference).alt } satisfies Image
          : { ...attributes, type: 'link', children: child.children };
      });
    });
    walk(tree, (parent) => {
      if (!('children' in parent)) return;
      const children = parent.children as RootContent[];
      for (let index = children.length - 1; index >= 0; index -= 1) {
        const child = children[index];
        if (child.type === 'definition') {
          if (used.has(child.identifier)) children.splice(index, 1);
          else children[index] = {
            type: 'html',
            value: `[${child.label ?? child.identifier}]: <${child.url}>${child.title ? ` "${child.title}"` : ''}`,
          };
        }
      }
    });
  };
}
