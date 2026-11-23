export const MAX_DOCUMENT_BYTES = 64 * 1024 * 1024;
export const MAX_DOCUMENT_LINES = 200_000;
export const MAX_TREE_NODES = 200_000;
export const MAX_TREE_DEPTH = 128;
export const MAX_SEARCH_QUERY = 1024;
export const MAX_SEARCH_MATCHES = 10_000;

export function validateCodeLanguage(value: unknown): asserts value is string {
  if (typeof value !== 'string' || value.length > 48 || /[^a-z0-9_+#.-]/i.test(value)) {
    throw new Error('コードブロックの言語が無効です。');
  }
}

const encoder = new TextEncoder();
const encodingBuffer = new Uint8Array(64 * 1024);

export function fitsUtf8(text: string, limit: number) {
  if (text.length > limit) return false;
  if (text.length * 3 <= limit) return true;
  for (let offset = 0; offset < text.length;) {
    const { read, written } = encoder.encodeInto(text.slice(offset), encodingBuffer);
    limit -= written;
    if (limit < 0) return false;
    offset += read;
  }
  return true;
}

export function validateMarkdown(text: string) {
  if (!fitsUtf8(text, MAX_DOCUMENT_BYTES)) {
    throw new Error('Markdownファイルは64 MiBまでです。');
  }
  if (text.includes('\0')) throw new Error('MarkdownにNUL文字が含まれています。');
  let lines = 1;
  for (const _line of text.matchAll(/\r\n|[\r\n]/g)) {
    if (++lines > MAX_DOCUMENT_LINES) throw new Error('Markdownファイルは20万行までです。');
  }
}

export function linkHref(value: string): string | null {
  const href = value.trim();
  if (!href || href.length > 8192 || /[\u0000-\u001f\u007f-\u009f\\]/.test(value) || href.startsWith('//')) {
    return null;
  }
  if (!/^[^/?#]*:/.test(href)) return href;
  if (!/^(?:https?:\/\/|mailto:)/i.test(href)) return null;
  try {
    const url = new URL(href);
    if (url.protocol === 'mailto:') return url.pathname ? href : null;
    return url.hostname && !url.username && !url.password ? href : null;
  } catch {
    return null;
  }
}
