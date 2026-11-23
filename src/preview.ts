import DOMPurify from 'dompurify';
import { Check, Copy, createElement } from 'lucide';
import type { Code, Root } from 'mdast';
import { highlightCode, MAX_HIGHLIGHT_LENGTH, MAX_HIGHLIGHT_TOKENS } from './code-highlight';
import { createLanguageSelect } from './code-language-select';
import { outlineOf, relativeImage, renderHtml, svgData, walk } from './markdown';
import { desktop, escapeHtml, messageOf } from './platform';
import { fitsUtf8, linkHref, MAX_DOCUMENT_BYTES } from './security';
import documentStyles from './document.css?inline';

const imageDataPattern = /^data:image\/(?:png|jpeg|gif|webp|svg\+xml|bmp|avif|x-icon|vnd\.microsoft\.icon)(?:;charset=utf-8)?(?:;base64)?,/i;
const MAX_IMAGE_DATA_LENGTH = 32 * 1024 * 1024 + 256;
const IMAGE_CACHE_LENGTH = 40 * 1024 * 1024;
const MAX_PREVIEW_DATA_LENGTH = 64 * 1024 * 1024;
const MAX_PREVIEW_IMAGES = 256;
const MAX_DIAGRAMS = 64;
const ASSET_LIMIT = '画像とMermaid図の合計は64 MiBまでです。';
const safeClass = /^(?:task-list-item|contains-task-list|footnotes|language-[a-z0-9_+#.-]{1,48})$/i;
const HTML_NAMESPACE = 'http://www.w3.org/1999/xhtml';
const SVG_NAMESPACE = 'http://www.w3.org/2000/svg';
const svgReferenceAttributes = [
  'fill', 'stroke', 'filter', 'clip-path', 'mask', 'marker-start', 'marker-mid', 'marker-end', 'color-profile',
];

// DOMPurify prefixes document IDs when SANITIZE_NAMED_PROPS is enabled.
export const contentId = (id: string) => `user-content-${id}`;

export function fragmentId(href: string): string | null {
  if (!href.startsWith('#')) return null;
  try {
    return decodeURIComponent(href.slice(1));
  } catch {
    return null;
  }
}

export function safeMarkup(html: string, idPrefix = '') {
  const fragment = DOMPurify.sanitize(html, {
    USE_PROFILES: { html: true, svg: true },
    FORBID_TAGS: [
      'style', 'form', 'button', 'textarea', 'select', 'iframe', 'object', 'embed',
      'foreignobject', 'animate', 'animatetransform', 'set', 'use', 'video', 'audio', 'source',
      'area', 'map', 'base', 'meta', 'link', 'dialog', 'template', 'image', 'feimage',
    ],
    FORBID_ATTR: [
      'style', 'srcset', 'name', 'form', 'target', 'loading', 'autofocus', 'contenteditable',
      'tabindex', 'draggable', 'popover', 'popovertarget', 'popovertargetaction', 'srcdoc',
      'ping', 'download', 'xlink:href', 'background', 'poster', 'action', 'formaction',
    ],
    SANITIZE_NAMED_PROPS: true,
    ALLOW_DATA_ATTR: false,
    ADD_ATTR: ['data-source-line'],
    RETURN_DOM_FRAGMENT: true,
  });
  for (const element of fragment.querySelectorAll('[class]')) {
    const classes = [...element.classList].filter((name) => safeClass.test(name));
    if (classes.length) element.setAttribute('class', classes.join(' '));
    else element.removeAttribute('class');
  }
  const ids = new Map<string, Element>();
  const svgIds = new Map<Element, Map<string, Element>>();
  let duplicate = 0;
  for (const element of fragment.querySelectorAll('[id]')) {
    const id = element.id;
    const svg = element.namespaceURI === SVG_NAMESPACE ? element.closest('svg') : null;
    const local = svg && (svgIds.get(svg) ?? new Map<string, Element>());
    if (local ? local.has(id) : ids.has(id)) {
      element.removeAttribute('id');
      continue;
    }
    if (local) {
      local.set(id, element);
      svgIds.set(svg!, local);
    }
    element.id = idPrefix + (ids.has(id) ? `shiori-duplicate-${++duplicate}-${id}` : id);
    if (!ids.has(id)) ids.set(id, element);
  }
  for (const input of fragment.querySelectorAll('input')) {
    if (input.type !== 'checkbox') input.remove();
    else input.disabled = true;
  }
  for (const element of fragment.querySelectorAll('[src]')) {
    if (element.namespaceURI !== HTML_NAMESPACE || element.localName !== 'img') element.removeAttribute('src');
  }
  for (const link of fragment.querySelectorAll('a[href]')) {
    const href = linkHref(link.getAttribute('href') ?? '');
    if (!href || link.namespaceURI !== HTML_NAMESPACE) {
      link.removeAttribute('href');
      continue;
    }
    const id = fragmentId(href);
    const target = id && ids.get(contentId(id));
    link.setAttribute('href', target ? `#${encodeURIComponent(target.id)}` : href);
  }
  for (const svg of fragment.querySelectorAll('svg, svg *')) {
    const owner = svg.closest('svg');
    const local = owner && svgIds.get(owner);
    const localTarget = (id: string | null) => {
      if (!id || !/^[\p{L}\p{M}\p{N}_.:-]+$/u.test(id)) return undefined;
      return local?.get(contentId(id))?.id;
    };
    const href = svg.getAttribute('href');
    if (href !== null) {
      const target = localTarget(fragmentId(href));
      if (target) svg.setAttribute('href', `#${target}`);
      else svg.removeAttribute('href');
    }
    for (const name of svgReferenceAttributes) {
      const value = svg.getAttribute(name);
      if (!value) continue;
      // A CSS escape or comment can hide the spelling of a URL function.
      if (value.includes('\\') || value.includes('/*')) {
        svg.removeAttribute(name);
        continue;
      }
      if (!/url\s*\(/i.test(value)) continue;
      const reference = /^url\(\s*(?:"#([^"]+)"|'#([^']+)'|#([^\s"'()]+))\s*\)$/i.exec(value);
      const target = localTarget(reference?.[1] ?? reference?.[2] ?? reference?.[3] ?? null);
      if (target) svg.setAttribute(name, `url(#${target})`);
      else svg.removeAttribute(name);
    }
  }
  return fragment;
}

export class Images {
  private cache = new Map<string, string>();
  private cacheLength = 0;
  private pending = new Map<string, Promise<string>>();
  private reads = Promise.resolve();
  private requested = new WeakMap<HTMLImageElement, symbol>();
  private generation = 0;
  private revision: number | null = null;

  reset(revision: number | null) {
    this.revision = revision;
    this.generation += 1;
    this.cache.clear();
    this.cacheLength = 0;
    this.pending.clear();
    this.reads = Promise.resolve();
  }

  local(source: string) {
    const cached = this.cache.get(source);
    if (cached) return Promise.resolve(cached);
    const queued = this.pending.get(source);
    if (queued) return queued;
    const { revision, generation } = this;
    if (revision === null) return Promise.reject(new Error('相対画像を表示するには文書の保存先が必要です。'));
    if (this.pending.size >= 128) return Promise.reject(new Error('画像の読み込みが混み合っています。'));
    const pending = this.reads.then(async () => {
      try {
        if (this.generation !== generation) throw new Error('表示中の文書が切り替わりました。');
        const data = await desktop.readImage(source, revision);
        if (this.generation !== generation) throw new Error('表示中の文書が切り替わりました。');
        if (data.length > MAX_IMAGE_DATA_LENGTH || !imageDataPattern.test(data)) {
          throw new Error('埋め込み画像の形式またはサイズが無効です。');
        }
        while (this.cacheLength + data.length > IMAGE_CACHE_LENGTH) {
          const oldest = this.cache.keys().next().value!;
          this.cacheLength -= this.cache.get(oldest)!.length;
          this.cache.delete(oldest);
        }
        this.cache.set(source, data);
        this.cacheLength += data.length;
        return data;
      } finally {
        if (this.generation === generation) this.pending.delete(source);
      }
    });
    this.reads = pending.then(() => undefined, () => undefined);
    this.pending.set(source, pending);
    return pending;
  }

  async display(
    img: HTMLImageElement,
    source: string,
    options: { strict?: boolean; remaining?: number; signal?: AbortSignal; budget?: AssetBudget } = {},
  ) {
    const request = Symbol();
    this.requested.set(img, request);
    img.removeAttribute('src');
    img.classList.remove('image-unavailable');
    img.referrerPolicy = 'no-referrer';
    img.decoding = 'async';
    const active = () => !options.signal?.aborted && this.requested.get(img) === request;
    if (!source || !active()) return 0;
    if (/^(?:https?:)?\/\//i.test(source)) {
      img.classList.add('image-unavailable');
      img.title = '外部画像';
      if (!img.alt) img.alt = '外部画像';
      return 0;
    }
    try {
      const data = imageDataPattern.test(source) ? source
        : relativeImage(source) ? await this.local(source) : null;
      if (!data || data.length > MAX_IMAGE_DATA_LENGTH) throw new Error('画像の形式またはサイズが無効です。');
      if (data.length > (options.remaining ?? MAX_PREVIEW_DATA_LENGTH)) throw new Error(ASSET_LIMIT);
      if (!active()) return 0;
      options.budget?.claim(img, data.length);
      img.src = data;
      return data.length;
    } catch (error) {
      if (active()) {
        img.classList.add('image-unavailable');
        img.title = messageOf(error);
      }
      if (options.strict && active()) throw error;
      return 0;
    }
  }
}

export class AssetBudget {
  private sizes = new WeakMap<Element, number>();
  private length = 0;

  claim(element: Element, length: number) {
    const previous = this.sizes.get(element) ?? 0;
    const total = this.length - previous + length;
    if (total > MAX_PREVIEW_DATA_LENGTH) throw new Error(ASSET_LIMIT);
    this.sizes.set(element, length);
    this.length = total;
  }

  release(element: Element) {
    this.length -= this.sizes.get(element) ?? 0;
    this.sizes.delete(element);
  }
}

let mermaid: Promise<typeof import('mermaid').default> | undefined;
let diagramSequence = 0;
let pendingDiagrams = 0;
const diagrams = new Map<string, Promise<string>>();

export function diagramData(code: string) {
  const cached = diagrams.get(code);
  if (cached) return cached;
  if (code.length > 50_000) return Promise.reject(new Error('Mermaid図は5万文字までです。'));
  if (pendingDiagrams >= 16) return Promise.reject(new Error('Mermaid図の描画が混み合っています。'));
  if (diagrams.size >= 8) diagrams.delete(diagrams.keys().next().value!);
  pendingDiagrams += 1;
  mermaid ??= import('mermaid').then(({ default: engine }) => {
    engine.initialize({
      startOnLoad: false,
      securityLevel: 'strict',
      htmlLabels: false,
      suppressErrorRendering: true,
      fontFamily: 'Segoe UI, Yu Gothic UI, sans-serif',
      maxTextSize: 50_000,
      maxEdges: 500,
      secure: [...new Set([
        ...(engine.mermaidAPI.getConfig().secure ?? []),
        'secure', 'securityLevel', 'htmlLabels', 'maxTextSize', 'maxEdges', 'themeCSS', 'themeVariables',
      ])],
    });
    return engine;
  });
  const stage = document.getElementById('diagram-stage')!;
  const container = document.createElement('div');
  stage.append(container);
  const pending = mermaid.then(async (engine) => {
    await document.fonts.ready;
    const { svg } = await engine.render(`shiori-diagram-${++diagramSequence}`, code, container);
    return svgData(svg);
  }).catch((error: unknown) => {
    if (diagrams.get(code) === pending) diagrams.delete(code);
    throw new Error(`Mermaid図の構文を確認してください。\n${messageOf(error)}`);
  }).finally(() => {
    pendingDiagrams -= 1;
    container.remove();
  });
  diagrams.set(code, pending);
  return pending;
}

function decorateCode(
  pre: HTMLElement, code: HTMLElement, diagram: boolean, interactive: boolean,
  changeLanguage?: (language: string) => void,
  language = languageOf(code) ?? '',
) {
  const block = document.createElement('div');
  block.className = diagram ? 'code-frame diagram-frame' : 'code-frame';
  if (pre.dataset.sourceLine) block.dataset.sourceLine = pre.dataset.sourceLine;
  pre.removeAttribute('data-source-line');
  pre.replaceWith(block);
  if (interactive) {
    const caption = document.createElement('div');
    caption.className = 'code-caption';
    const lang = changeLanguage
      ? createLanguageSelect(language, changeLanguage)
      : document.createElement('span');
    if (!changeLanguage) lang.textContent = diagram ? 'Mermaid' : language || 'コード';
    const copy = document.createElement('button');
    copy.type = 'button';
    copy.className = 'code-copy';
    copy.setAttribute('aria-label', 'コードをコピー');
    copy.title = 'コードをコピー';
    copy.append(createElement(Copy));
    let reset: number | undefined;
    copy.addEventListener('click', () => {
      void navigator.clipboard.writeText((code.textContent ?? '').replace(/\n$/, '')).then(() => {
        copy.replaceChildren(createElement(Check));
        copy.title = 'コピーしました';
        copy.setAttribute('aria-label', copy.title);
      }).catch(() => {
        copy.title = 'コピーできませんでした';
        copy.setAttribute('aria-label', copy.title);
      }).finally(() => {
        window.clearTimeout(reset);
        reset = window.setTimeout(() => {
          copy.replaceChildren(createElement(Copy));
          copy.title = 'コードをコピー';
          copy.setAttribute('aria-label', copy.title);
        }, 1800);
      });
    });
    caption.append(lang, copy);
    block.append(caption);
  }
  block.append(pre);
  return block;
}

function languageOf(code: HTMLElement) {
  return [...code.classList].find((name) => name.startsWith('language-'))?.slice(9);
}

interface PreviewOptions {
  interactive: boolean;
  strictImages?: boolean;
  signal?: AbortSignal;
  changeCodeLanguage?: (block: Code, language: string) => void;
}

export function createPreview(root: Root, images: Images, options: PreviewOptions) {
  const outline = outlineOf(root);
  const article = document.createElement('article');
  article.className = 'document-content';
  const idPrefix = options.interactive ? '' : 'shiori-export-';
  const markup = safeMarkup(renderHtml(root, outline), idPrefix);
  const pictures = Array.from(markup.querySelectorAll('img'), (img) => {
    const source = img.getAttribute('src') ?? '';
    img.removeAttribute('src');
    return { img, source };
  });
  article.append(markup);
  const headings = new Set(outline.map(({ id }) => idPrefix + contentId(`shiori-heading-${id}`)));
  article.querySelectorAll<HTMLAnchorElement>('a[href^="#"]').forEach((link) => {
    const id = fragmentId(link.getAttribute('href') ?? '');
    if (id === null) return;
    const target = idPrefix + contentId(`shiori-heading-${id}`);
    if (headings.has(target)) link.setAttribute('href', `#${encodeURIComponent(target)}`);
  });
  const pending: (() => Promise<void>)[] = [];
  let remaining = MAX_PREVIEW_DATA_LENGTH;
  for (const { img, source } of pictures) {
    if (!source) continue;
    if (pending.length >= MAX_PREVIEW_IMAGES) {
      if (options.strictImages) throw new Error('一度に出力できる画像は256件までです。');
      img.classList.add('image-unavailable');
      img.title = 'プレビューできる画像は256件までです。';
      continue;
    }
    pending.push(async () => {
      remaining -= await images.display(img, source, { strict: options.strictImages, remaining, signal: options.signal });
    });
  }
  let diagrams = 0;
  const codeBlocks = new Map<number, Code>();
  walk(root, (node) => {
    if (node.type === 'code' && node.position) codeBlocks.set(node.position.start.line, node);
  });
  let highlightRemaining = MAX_HIGHLIGHT_LENGTH;
  let highlightTokensRemaining = MAX_HIGHLIGHT_TOKENS;
  for (const pre of article.querySelectorAll('pre')) {
    const code = pre.querySelector('code');
    if (!code) continue;
    const language = languageOf(code) ?? '';
    const diagram = language.toLowerCase() === 'mermaid';
    if (!diagram && (code.textContent?.length ?? 0) <= highlightRemaining) {
      highlightRemaining -= code.textContent?.length ?? 0;
      pending.push(async () => {
        highlightTokensRemaining -= await highlightCode(code, language, options.signal, highlightTokensRemaining);
      });
    }
    if (!diagram && !options.interactive) continue;
    const sourceBlock = codeBlocks.get(Number(pre.dataset.sourceLine));
    const changeLanguage = options.changeCodeLanguage && sourceBlock
      ? (language: string) => options.changeCodeLanguage!(sourceBlock, language) : undefined;
    const block = decorateCode(pre, code, diagram, options.interactive, changeLanguage, sourceBlock?.lang ?? language);
    if (!diagram) continue;
    if (++diagrams > MAX_DIAGRAMS) {
      if (!options.interactive) throw new Error('一度に出力できるMermaid図は64件までです。');
      continue;
    }
    pending.push(async () => {
      try {
        const data = await diagramData(code.textContent ?? '');
        if (options.signal?.aborted) return;
        if (data.length > remaining) throw new Error(ASSET_LIMIT);
        remaining -= data.length;
        const img = document.createElement('img');
        img.alt = 'Mermaid 図';
        img.src = data;
        pre.replaceWith(img);
      } catch (error) {
        if (!options.interactive) throw error;
        const details = document.createElement('p');
        details.className = 'diagram-error';
        details.textContent = 'Mermaid図を描画できません';
        details.title = messageOf(error);
        block.insertBefore(details, pre);
      }
    });
  }
  const ready = (async () => {
    for (const load of pending) {
      if (options.signal?.aborted) break;
      await load();
    }
  })();
  return { article, outline, ready };
}

export async function waitForImages(container: HTMLElement) {
  await document.fonts.ready;
  let timer: number | undefined;
  try {
    await Promise.race([
      Promise.all([...container.querySelectorAll<HTMLImageElement>('img[src]')].map(async (img) => {
        await img.decode().catch(() => { throw new Error(`画像を表示できません。 ${img.alt.slice(0, 120)}`); });
      })),
      new Promise<never>((_, reject) => {
        timer = window.setTimeout(() => reject(new Error('画像の準備がタイムアウトしました。')), 10_000);
      }),
    ]);
  } finally {
    window.clearTimeout(timer);
  }
}

export function standaloneHtml(article: HTMLElement, title: string) {
  for (const item of article.querySelectorAll('[data-source-line]')) item.removeAttribute('data-source-line');
  const html = `<!doctype html>
<html lang="ja">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; form-action 'none'">
<title>${escapeHtml(title)}</title>
<style>${documentStyles}
body { margin: 0; background: #f8f6f2; }
main { margin: 0 auto; max-width: 840px; padding: clamp(24px, 6vw, 64px); background: white; }
@media print { body { background: white; } main { padding: 0; } }
</style>
<main class="document-content">
${article.innerHTML}
</main>
</html>
`;
  if (!fitsUtf8(html, MAX_DOCUMENT_BYTES)) throw new Error('HTMLの出力サイズは64 MiBまでです。');
  return html;
}
