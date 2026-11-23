import { defaultKeymap, history, historyKeymap, isolateHistory } from '@codemirror/commands';
import { markdown } from '@codemirror/lang-markdown';
import { defaultHighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { Compartment, EditorState, StateEffect, StateField } from '@codemirror/state';
import { Decoration, type DecorationSet, EditorView, highlightActiveLine, highlightActiveLineGutter, keymap, lineNumbers, placeholder } from '@codemirror/view';
import type { Code } from 'mdast';
import { codeLanguageChange, imageMarkdown } from './markdown';
import { codeLanguage } from './code-highlight';
import { fitsUtf8, MAX_DOCUMENT_BYTES, MAX_DOCUMENT_LINES } from './security';

export type Format = 'paragraph' | 'h1' | 'h2' | 'h3' | 'h4' | 'h5' | 'h6'
  | 'bold' | 'italic' | 'strike' | 'inlineCode' | 'code' | 'bullet' | 'ordered' | 'task'
  | 'quote' | 'link' | 'image' | 'mermaid' | 'table' | 'undo' | 'redo';

export interface FormatValue { source: string; label: string }
export interface TextMatch { from: number; to: number }

export const mermaidExample = 'flowchart LR\n    A[開始] --> B[完了]';
const mermaidSnippet = '```mermaid\n' + mermaidExample + '\n```';
const tableSnippet = '| 項目 | 内容 |\n| --- | --- |\n|  |  |';

const markMatches = StateEffect.define<{ matches: TextMatch[]; active: number }>();
const findMarks = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(marks, transaction) {
    for (const effect of transaction.effects) {
      if (effect.is(markMatches)) {
        const { matches, active } = effect.value;
        return Decoration.set(matches.map((match, index) => Decoration.mark({
          class: index === active ? 'cm-find-current' : 'cm-find-match',
        }).range(match.from, match.to)));
      }
    }
    return marks.map(transaction.changes);
  },
  provide: (field) => EditorView.decorations.from(field),
});

export class SourceEditor {
  readonly view: EditorView;
  private readonly editable = new Compartment();
  private syncing = false;
  private locked = false;

  constructor(
    parent: HTMLElement,
    private readonly changed: (markdown: string) => void,
    private readonly onError: (error: unknown) => void,
  ) {
    this.view = new EditorView({ parent, state: this.state('') });
  }

  private state(doc: string) {
    return EditorState.create({ doc, extensions: [
      lineNumbers(), highlightActiveLine(), highlightActiveLineGutter(), history(),
      markdown({ codeLanguages: codeLanguage }),
      syntaxHighlighting(defaultHighlightStyle, { fallback: true }), EditorView.lineWrapping,
      keymap.of([...historyKeymap, ...defaultKeymap]), findMarks, placeholder('ここにMarkdownを入力…'),
      this.editable.of(EditorView.editable.of(!this.locked)),
      EditorView.contentAttributes.of({
        'aria-label': 'Markdown本文', 'aria-multiline': 'true', spellcheck: 'false',
      }),
      EditorState.transactionFilter.of((transaction) => {
        if (!transaction.docChanged || this.syncing) return transaction;
        try {
          const next = transaction.newDoc;
          if (next.lines > MAX_DOCUMENT_LINES) throw new Error('Markdownファイルは20万行までです。');
          if (next.length > MAX_DOCUMENT_BYTES || (next.length * 3 > MAX_DOCUMENT_BYTES
              && !fitsUtf8(next.toString(), MAX_DOCUMENT_BYTES))) {
            throw new Error('Markdownファイルは64 MiBまでです。');
          }
          transaction.changes.iterChanges((_from, _to, _newFrom, _newTo, inserted) => {
            for (const line of inserted.iterLines()) {
              if (line.includes('\0')) throw new Error('MarkdownにNUL文字が含まれています。');
            }
          });
          return transaction;
        } catch (error) {
          this.onError(error);
          return [];
        }
      }),
      EditorView.updateListener.of((update) => {
        if (update.docChanged && !this.syncing) this.changed(update.state.doc.toString());
      }),
    ] });
  }

  get text() { return this.view.state.doc.toString(); }
  get scroll() { return this.view.scrollDOM; }
  get selection() {
    const { from, to } = this.view.state.selection.main;
    return this.view.state.sliceDoc(from, to);
  }

  reset(text: string) { this.view.setState(this.state(text)); }

  replace(text: string) {
    if (text === this.text) return;
    this.syncing = true;
    try {
      this.view.dispatch({ changes: { from: 0, to: this.view.state.doc.length, insert: text } });
    } finally {
      this.syncing = false;
    }
  }

  lock(locked: boolean) {
    this.locked = locked;
    this.view.dispatch({ effects: this.editable.reconfigure(EditorView.editable.of(!locked)) });
  }

  find(matches: TextMatch[], active: number, reveal: boolean) {
    const effects: StateEffect<unknown>[] = [markMatches.of({ matches, active })];
    const match = matches[active];
    if (reveal && match) effects.push(EditorView.scrollIntoView(match.from, { y: 'center' }));
    this.view.dispatch({ effects });
  }

  topLine() {
    const height = Math.max(0, this.scroll.getBoundingClientRect().top - this.view.documentTop);
    const block = this.view.lineBlockAtHeight(height);
    return this.view.state.doc.lineAt(block.from).number + Math.max(0, height - block.top) / Math.max(1, block.height);
  }

  scrollToLine(line: number) {
    const number = Math.max(1, Math.min(Math.floor(line), this.view.state.doc.lines));
    const block = this.view.lineBlockAt(this.view.state.doc.line(number).from);
    const origin = this.scroll.scrollTop + this.view.documentTop - this.scroll.getBoundingClientRect().top;
    this.scroll.scrollTop = origin + block.top + Math.max(0, line - number) * block.height;
  }

  jump(line: number) {
    const anchor = this.view.state.doc.line(line).from;
    this.view.dispatch({ selection: { anchor }, effects: EditorView.scrollIntoView(anchor, { y: 'start', yMargin: 18 }) });
  }

  setCodeLanguage(block: Code, language: string, expectedMarkdown: string) {
    if (this.locked) return;
    if (this.text !== expectedMarkdown) throw new Error('本文の変更を反映してから、言語を選び直してください。');
    this.view.dispatch({
      changes: codeLanguageChange(expectedMarkdown, block, language),
      annotations: isolateHistory.of('full'),
      userEvent: 'input',
    });
  }

  format(format: Format, value?: FormatValue) {
    const { state } = this.view;
    const { from, to } = state.selection.main;
    const selected = state.sliceDoc(from, to);
    const inline = {
      bold: ['**', '太字'], italic: ['*', '斜体'], strike: ['~~', '取り消し線'],
      inlineCode: ['`', 'コード'],
    } as const;

    const apply = (start: number, end: number, insert: string, anchor?: number, head?: number) => {
      this.view.dispatch({
        changes: { from: start, to: end, insert },
        selection: { anchor: anchor ?? start + insert.length, head },
        scrollIntoView: true,
        userEvent: 'input',
      });
      this.view.focus();
    };

    if (format === 'bold' || format === 'italic' || format === 'strike' || format === 'inlineCode') {
      const [marker, label] = inline[format];
      if (format === 'inlineCode' && selected.includes('`')) {
        let longest = 0;
        for (const run of selected.matchAll(/`+/g)) longest = Math.max(longest, run[0].length);
        const fence = '`'.repeat(longest + 1);
        const padding = /^`|`$|^ .* $/.test(selected) ? ' ' : '';
        apply(from, to, fence + padding + selected + padding + fence, from + fence.length + padding.length, to + fence.length + padding.length);
        return;
      }
      if (from >= marker.length && selected && state.sliceDoc(from - marker.length, from) === marker
          && state.sliceDoc(to, to + marker.length) === marker) {
        apply(from - marker.length, to + marker.length, selected, from - marker.length, to - marker.length);
      } else {
        const content = selected || label;
        apply(from, to, marker + content + marker, from + marker.length, from + marker.length + content.length);
      }
      return;
    }

    if (format === 'link' && value) {
      const label = value.label.replace(/[\\[\]]/g, '\\$&');
      const url = value.source.replace(/[<>\r\n]/g, (character) => encodeURIComponent(character));
      apply(from, to, `[${label}](<${url}>)`);
    } else if (format === 'image' && value) {
      apply(from, to, imageMarkdown(value.label, value.source));
    } else if (format === 'code' || format === 'mermaid' || format === 'table') {
      let longest = 2;
      if (format === 'code') {
        for (const run of selected.matchAll(/^`+/gm)) longest = Math.max(longest, run[0].length);
      }
      const fence = '`'.repeat(longest + 1);
      const snippet = format === 'code' ? `${fence}\n${selected || 'コード'}\n${fence}`
        : format === 'mermaid' ? mermaidSnippet : tableSnippet;
      const prefix = from === 0 || state.sliceDoc(Math.max(0, from - 2), from) === '\n\n' ? '' : '\n\n';
      const suffix = to === state.doc.length ? '\n' : '\n\n';
      if (format === 'code') {
        const body = from + prefix.length + fence.length + 1;
        apply(from, to, prefix + snippet + suffix, body, body + (selected || 'コード').length);
      } else {
        apply(from, to, prefix + snippet + suffix);
      }
    } else {
      const first = state.doc.lineAt(from);
      const last = state.doc.lineAt(to > from && state.doc.lineAt(to).from === to ? to - 1 : to);
      const lines = state.sliceDoc(first.from, last.to).split('\n');
      let prefix: (index: number) => string;
      let strip: RegExp;
      if (/^h[1-6]$/.test(format) || format === 'paragraph') {
        prefix = () => format === 'paragraph' ? '' : '#'.repeat(Number(format[1])) + ' ';
        strip = /^ {0,3}#{1,6}(?:\s+|$)/;
      } else if (format === 'quote') {
        prefix = () => '> ';
        strip = /^> ?/;
      } else {
        prefix = (index) => format === 'ordered' ? `${index + 1}. ` : format === 'task' ? '- [ ] ' : '- ';
        strip = /^(?:[-*+] |\d+[.)] )(?:\[[ xX]\] )?/;
      }
      const remove = format !== 'paragraph' && lines.every((line, index) => line.startsWith(prefix(index)));
      const formatted = lines.map((line, index) => (remove ? '' : prefix(index)) + line.replace(strip, '')).join('\n');
      const cursor = from === to ? first.from + Math.max(0, Math.min(formatted.length, from - first.from + formatted.length - lines[0].length)) : first.from;
      apply(first.from, last.to, formatted, cursor, from === to ? cursor : first.from + formatted.length);
    }
  }
}
