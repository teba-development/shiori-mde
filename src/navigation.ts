import type { SourceEditor, TextMatch } from './source';
import { MAX_SEARCH_MATCHES, MAX_SEARCH_QUERY } from './security';

interface Anchor { line: number; top: number }

function interpolate(anchors: Anchor[], value: number, from: keyof Anchor, to: keyof Anchor) {
  let left = 0;
  let right = anchors.length - 1;
  while (right - left > 1) {
    const middle = (left + right) >> 1;
    if (anchors[middle][from] <= value) left = middle;
    else right = middle;
  }
  const first = anchors[left];
  const last = anchors[right];
  const fraction = Math.max(0, Math.min(1, (value - first[from]) / Math.max(0.01, last[from] - first[from])));
  return first[to] + (last[to] - first[to]) * fraction;
}

export class ScrollSync {
  private anchors: Anchor[] | undefined;
  private sourceEcho: number | undefined;
  private previewEcho: number | undefined;

  constructor(
    private readonly source: SourceEditor,
    private readonly pane: HTMLElement,
    private readonly split: () => boolean,
    private readonly onLine: (line: number) => void,
  ) {
    source.scroll.addEventListener('scroll', () => this.fromSource(), { passive: true });
    pane.addEventListener('scroll', () => this.fromPreview(), { passive: true });
    new ResizeObserver(() => this.invalidate()).observe(pane);
  }

  invalidate() { this.anchors = undefined; }

  private positions() {
    if (this.anchors) return this.anchors;
    const anchors: Anchor[] = [{ line: 1, top: 0 }];
    const origin = this.pane.getBoundingClientRect().top - this.pane.scrollTop;
    for (const element of this.pane.querySelectorAll<HTMLElement>('[data-source-line]')) {
      const line = Number(element.dataset.sourceLine);
      if (!Number.isSafeInteger(line) || line < 1) continue;
      const top = element.getBoundingClientRect().top - origin - 26;
      const last = anchors.at(-1)!;
      if (line > last.line && top > last.top + 1) anchors.push({ line, top });
    }
    anchors.push({
      line: Math.max(this.source.view.state.doc.lines + 1, anchors.at(-1)!.line + 1),
      top: Math.max(this.pane.scrollHeight, anchors.at(-1)!.top + 1),
    });
    this.anchors = anchors;
    return anchors;
  }

  previewLine() { return interpolate(this.positions(), this.pane.scrollTop, 'top', 'line'); }

  goToPreview(line: number) {
    this.pane.scrollTop = interpolate(this.positions(), line, 'line', 'top');
    this.previewEcho = this.pane.scrollTop;
  }

  fromSource(force = false) {
    if (!this.split()) return;
    const scroll = this.source.scroll;
    const echo = !force && Math.abs((this.sourceEcho ?? -2) - scroll.scrollTop) < 1;
    this.sourceEcho = undefined;
    if (echo) return;
    const line = this.source.topLine();
    if (scroll.scrollTop > 0 && scroll.scrollHeight - scroll.clientHeight - scroll.scrollTop <= 1) {
      this.pane.scrollTop = this.pane.scrollHeight;
      this.previewEcho = this.pane.scrollTop;
    } else this.goToPreview(line);
    this.onLine(line);
  }

  private fromPreview() {
    const top = this.pane.scrollTop;
    const echo = Math.abs((this.previewEcho ?? -2) - top) < 1;
    this.previewEcho = undefined;
    if (echo) return;
    const line = this.previewLine();
    if (this.split()) {
      if (top > 0 && this.pane.scrollHeight - this.pane.clientHeight - top <= 1) {
        this.source.scroll.scrollTop = this.source.scroll.scrollHeight;
      } else this.source.scrollToLine(line);
      this.sourceEcho = this.source.scroll.scrollTop;
    }
    this.onLine(line);
  }
}

export function textMatches(text: string, query: string): TextMatch[] {
  if (!query) return [];
  const literal = query.slice(0, MAX_SEARCH_QUERY).replace(/[.*+?^$(){}|[\]\\]/g, '\\$&');
  const matches: TextMatch[] = [];
  for (const match of text.matchAll(new RegExp(literal, 'giu'))) {
    matches.push({ from: match.index, to: match.index + match[0].length });
    if (matches.length >= MAX_SEARCH_MATCHES) break;
  }
  return matches;
}

export function domMatches(root: HTMLElement, query: string) {
  if (!query) return [];
  const pieces: { node: Text; start: number; end: number }[] = [];
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let content = '';
  let previous: Element | null = null;
  for (let node = walker.nextNode() as Text | null; node; node = walker.nextNode() as Text | null) {
    const parent = node.parentElement;
    if (!parent || !node.length || parent.closest('.code-caption, .diagram-error, summary, [hidden], .ProseMirror-separator')) continue;
    const block = parent.closest('p, li, h1, h2, h3, h4, h5, h6, blockquote, pre, td, th, div');
    if (previous && previous !== block) content += '\n';
    const start = content.length;
    content += node.data;
    pieces.push({ node, start, end: content.length });
    previous = block;
  }
  let start = 0;
  return textMatches(content, query).flatMap(({ from, to }) => {
    while (pieces[start] && pieces[start].end <= from) start += 1;
    let end = start;
    while (pieces[end] && pieces[end].end < to) end += 1;
    if (!pieces[start] || !pieces[end]) return [];
    const range = document.createRange();
    range.setStart(pieces[start].node, Math.max(0, from - pieces[start].start));
    range.setEnd(pieces[end].node, to - pieces[end].start);
    return [range];
  });
}

export function highlightRanges(ranges: Range[], active: number) {
  if (!CSS.highlights) return;
  const all = new Highlight();
  for (const range of ranges) all.add(range);
  CSS.highlights.set('shiori-find', all);
  CSS.highlights.set('shiori-current', ranges[active] ? new Highlight(ranges[active]) : new Highlight());
}

export function revealRange(pane: HTMLElement, range: Range) {
  const target = range.getBoundingClientRect();
  const viewport = pane.getBoundingClientRect();
  if (target.top >= viewport.top + 22 && target.bottom < viewport.bottom - 22) return;
  pane.scrollTop += target.top - viewport.top - (pane.clientHeight - target.height) / 2;
}
