import { LanguageDescription, type LanguageSupport } from '@codemirror/language';
import { languages } from '@codemirror/language-data';
import { classHighlighter, highlightTree } from '@lezer/highlight';

const MAX_BLOCK_LENGTH = 200_000;
const PARSE_TIME_MS = 40;
export const MAX_HIGHLIGHT_LENGTH = 1_000_000;
export const MAX_HIGHLIGHT_TOKENS = 20_000;

export interface CodeToken {
  from: number;
  to: number;
  classes: string;
}

const aliases: Record<string, string> = {
  shell: 'sh', zsh: 'sh', console: 'sh', pwsh: 'powershell',
  docker: 'dockerfile', text: '', txt: '', plaintext: '', none: '',
};
const loading = new Map<LanguageDescription, Promise<LanguageSupport | null>>();

export function codeLanguage(info: string): LanguageDescription | null {
  const name = /^\s*([a-z0-9_+#.-]{1,48})(?=\s|$)/i.exec(info)?.[1].toLowerCase();
  if (!name || name === 'mermaid') return null;
  const normalized = Object.hasOwn(aliases, name) ? aliases[name] : name;
  if (!normalized) return null;
  return LanguageDescription.matchLanguageName(languages, normalized, false)
    ?? languages.find((language) => language.extensions.includes(normalized))
    ?? null;
}

export async function codeTokens(source: string, info: string): Promise<CodeToken[]> {
  if (!source || source.length > MAX_BLOCK_LENGTH) return [];
  const description = codeLanguage(info);
  if (!description) return [];
  let support = description.support;
  if (!support) {
    let pending = loading.get(description);
    if (!pending) {
      pending = description.load().catch(() => null);
      loading.set(description, pending);
    }
    support = await pending ?? undefined;
  }
  if (!support) return [];

  try {
    const deadline = performance.now() + PARSE_TIME_MS;
    const parse = support.language.parser.startParse(source);
    let tree = parse.advance();
    while (!tree && performance.now() < deadline) tree = parse.advance();
    if (!tree) return [];
    const tokens: CodeToken[] = [];
    highlightTree(tree, classHighlighter, (from, to, classes) => {
      if (tokens.length >= MAX_HIGHLIGHT_TOKENS) throw new Error('Code highlighting limit');
      tokens.push({ from, to, classes });
    });
    return tokens;
  } catch {
    return [];
  }
}

export async function highlightCode(
  code: HTMLElement, info: string, signal?: AbortSignal, remainingTokens = MAX_HIGHLIGHT_TOKENS,
) {
  if (signal?.aborted || !remainingTokens) return 0;
  const source = code.textContent ?? '';
  const tokens = await codeTokens(source, info);
  if (!tokens.length || tokens.length > remainingTokens || signal?.aborted || code.textContent !== source) return 0;
  const fragment = document.createDocumentFragment();
  let offset = 0;
  for (const { from, to, classes } of tokens) {
    fragment.append(document.createTextNode(source.slice(offset, from)));
    const span = document.createElement('span');
    span.className = classes;
    span.textContent = source.slice(from, to);
    fragment.append(span);
    offset = to;
  }
  fragment.append(document.createTextNode(source.slice(offset)));
  code.replaceChildren(fragment);
  return tokens.length;
}
