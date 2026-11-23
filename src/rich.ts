import { defaultValueCtx, Editor, editorViewCtx, editorViewOptionsCtx, rootCtx, serializerCtx } from '@milkdown/kit/core';
import { clipboard } from '@milkdown/kit/plugin/clipboard';
import { history, redoCommand, undoCommand } from '@milkdown/kit/plugin/history';
import {
  codeBlockSchema, commonmark, createCodeBlockCommand, htmlSchema, imageSchema, insertImageCommand,
  toggleEmphasisCommand, toggleInlineCodeCommand, toggleLinkCommand, toggleStrongCommand, turnIntoTextCommand,
  wrapInBlockquoteCommand, wrapInBulletListCommand, wrapInHeadingCommand, wrapInOrderedListCommand,
} from '@milkdown/kit/preset/commonmark';
import { gfm, insertTableCommand, toggleStrikethroughCommand } from '@milkdown/kit/preset/gfm';
import type { Node as ProseNode } from '@milkdown/kit/prose/model';
import { Plugin } from '@milkdown/kit/prose/state';
import { closeHistory } from '@milkdown/kit/prose/history';
import type { EditorView, NodeView } from '@milkdown/kit/prose/view';
import { $prose, $remark, $view, callCommand, insert, replaceAll } from '@milkdown/kit/utils';
import '@milkdown/kit/prose/view/style/prosemirror.css';
import { expandReferences, parseMarkdown } from './markdown';
import { AssetBudget, diagramData, type Images, safeMarkup } from './preview';
import { fitsUtf8, MAX_DOCUMENT_BYTES, MAX_TREE_DEPTH, MAX_TREE_NODES, validateCodeLanguage, validateMarkdown } from './security';
import { mermaidExample, type Format, type FormatValue } from './source';
import { codeHighlightPlugin } from './rich-code-highlight';
import { createLanguageSelect, updateLanguageSelect } from './code-language-select';

function imageView(node: ProseNode, images: Images, budget: AssetBudget): NodeView {
  const img = document.createElement('img');
  let source: string | undefined;
  let request: AbortController | undefined;
  function release() {
    request?.abort();
    budget.release(img);
  }
  function update(next: ProseNode) {
    if (next.type !== node.type) return false;
    img.alt = next.attrs.alt;
    img.title = next.attrs.title;
    if (source !== next.attrs.src) {
      source = next.attrs.src;
      release();
      request = new AbortController();
      void images.display(img, source ?? '', { signal: request.signal, budget });
    }
    return true;
  }
  update(node);
  return { dom: img, update, ignoreMutation: () => true, destroy: release };
}

let htmlViewSequence = 0;

function htmlView(node: ProseNode, images: Images, budget: AssetBudget): NodeView {
  const span = document.createElement('span');
  span.className = 'rich-html';
  span.contentEditable = 'false';
  const fragment = safeMarkup(node.attrs.value, `shiori-rich-${++htmlViewSequence}-`);
  fragment.querySelectorAll('input').forEach((item) => item.remove());
  const request = new AbortController();
  const pictures = fragment.querySelectorAll('img');
  pictures.forEach((img) => void images.display(img, img.getAttribute('src') ?? '', { signal: request.signal, budget }));
  if (fragment.textContent?.trim() || fragment.firstElementChild?.matches('svg, img, hr, br')
      || fragment.querySelector('img, svg')) {
    span.append(fragment);
  } else {
    span.classList.add('rich-html-source');
    span.textContent = node.attrs.value;
  }
  return {
    dom: span,
    ignoreMutation: () => true,
    destroy() {
      request.abort();
      pictures.forEach((img) => budget.release(img));
    },
  };
}

function codeView(
  node: ProseNode, budget: AssetBudget, view: EditorView, getPos: () => number | undefined,
  onError: (error: unknown) => void,
): NodeView {
  const frame = document.createElement('div');
  frame.className = 'code-frame';
  const caption = document.createElement('div');
  caption.className = 'code-caption';
  caption.contentEditable = 'false';
  const language = createLanguageSelect(String(node.attrs.language ?? ''), (value) => {
    try {
      validateCodeLanguage(value);
      const position = getPos();
      if (position === undefined || !Number.isSafeInteger(position) || position < 0
          || position >= view.state.doc.content.size || !view.editable) return;
      const current = view.state.doc.nodeAt(position);
      if (current !== node || current.attrs.language === value) return;
      const transaction = view.state.tr.setNodeMarkup(position, null, { ...current.attrs, language: value });
      view.dispatch(closeHistory(transaction));
      // Keep the next text edit in its own undo group.
      if (view.state.doc.nodeAt(position)?.attrs.language === value) {
        view.dispatch(closeHistory(view.state.tr));
      }
    } catch (error) {
      onError(error);
    } finally {
      if (language.isConnected) updateLanguageSelect(language, String(node.attrs.language ?? ''));
    }
  });
  language.disabled = !view.editable;
  caption.append(language);
  frame.append(caption);
  const pre = document.createElement('pre');
  pre.dataset.language = node.attrs.language;
  const code = document.createElement('code');
  pre.append(code);
  const inCaption = (event: Event) => event.target instanceof Node && caption.contains(event.target);
  const ignoreMutation: NodeView['ignoreMutation'] = (mutation) =>
    mutation.type !== 'selection' && !code.contains(mutation.target);
  if (String(node.attrs.language).toLowerCase() !== 'mermaid') {
    frame.append(pre);
    return {
      dom: frame,
      contentDOM: code,
      update(next) {
        if (next.type !== node.type || String(next.attrs.language).toLowerCase() === 'mermaid') return false;
        node = next;
        pre.dataset.language = next.attrs.language;
        updateLanguageSelect(language, String(next.attrs.language ?? ''));
        language.disabled = !view.editable;
        return true;
      },
      stopEvent: inCaption,
      ignoreMutation,
    };
  }

  frame.classList.add('diagram-frame', 'rich-diagram');
  const graphic = document.createElement('div');
  graphic.contentEditable = 'false';
  const details = document.createElement('details');
  details.open = true;
  const summary = document.createElement('summary');
  summary.contentEditable = 'false';
  summary.textContent = 'Mermaid ソース';
  details.append(summary, pre);
  frame.append(graphic, details);
  let timer: number | undefined;
  let revision = 0;
  function render(source: string) {
    window.clearTimeout(timer);
    const token = ++revision;
    timer = window.setTimeout(() => {
      void diagramData(source).then((data) => {
        if (token !== revision) return;
        budget.claim(graphic, data.length);
        const image = document.createElement('img');
        image.alt = 'Mermaid 図';
        image.src = data;
        graphic.replaceChildren(image);
      }).catch(() => {
        if (token === revision) {
          budget.release(graphic);
          graphic.textContent = 'Mermaid図を描画できません';
        }
      });
    }, 200);
  }
  render(node.textContent);
  return {
    dom: frame,
    contentDOM: code,
    update(next) {
      if (next.type !== node.type || next.attrs.language !== node.attrs.language) return false;
      if (next.textContent !== node.textContent) render(next.textContent);
      node = next;
      return true;
    },
    stopEvent: (event) => inCaption(event) || summary.contains(event.target as Node),
    ignoreMutation,
    destroy() {
      window.clearTimeout(timer);
      revision += 1;
      budget.release(graphic);
    },
  };
}

function validCodeMetadata(data: string) {
  if (data.length > 8192) return false;
  try {
    const mode: unknown = JSON.parse(data)?.mode;
    return typeof mode === 'string' && /^[a-z0-9_+.-]{1,48}$/i.test(mode);
  } catch {
    return false;
  }
}

function validatePaste(view: EditorView, event: ClipboardEvent, onError: (error: unknown) => void) {
  const data = event.clipboardData;
  if (!data || !view.editable || view.composing) return false;
  const text = data.getData('text/plain') || data.getData('text/uri-list');
  const html = data.getData('text/html');
  try {
    validateMarkdown(text);
    if (!fitsUtf8(html, MAX_DOCUMENT_BYTES)) throw new Error('貼り付けるHTMLは64 MiBまでです。');
    const metadata = data.getData('vscode-editor-data');
    if (!metadata || validCodeMetadata(metadata)) return false;
    event.preventDefault();
    // The original event carries metadata the clipboard plugin cannot parse.
    if (html) view.pasteHTML(html);
    else if (text) view.pasteText(text);
  } catch (error) {
    event.preventDefault();
    onError(error);
  }
  return true;
}

function validateDocumentTree(root: ProseNode) {
  let nodes = 0;
  let length = 0;
  function visit(node: ProseNode, depth: number) {
    if (++nodes > MAX_TREE_NODES || depth > MAX_TREE_DEPTH) {
      throw new Error('Markdownの構造が複雑すぎます。');
    }
    length += node.text?.length ?? 0;
    if (length > MAX_DOCUMENT_BYTES) throw new Error('Markdownファイルは64 MiBまでです。');
    node.forEach((child) => visit(child, depth + 1));
  }
  visit(root, 0);
}

export class RichEditor {
  private editor: Editor;
  private synchronized: string;
  private syncing = true;
  private locked = false;

  private constructor(
    parent: HTMLElement, markdown: string, images: Images,
    changed: (text: string) => void, onError: (error: unknown) => void,
  ) {
    this.synchronized = markdown;
    const budget = new AssetBudget();
    const updates = $prose((ctx) => {
      let prepared: { doc: ProseNode; markdown: string } | undefined;
      const serialize = (doc: ProseNode) => {
        validateDocumentTree(doc);
        const value = ctx.get(serializerCtx)(doc);
        validateMarkdown(value);
        return value;
      };
      return new Plugin({
        filterTransaction: (transaction) => {
          if (this.syncing || !transaction.docChanged) return true;
          try {
            prepared = { doc: transaction.doc, markdown: serialize(transaction.doc) };
            return true;
          } catch (error) {
            onError(error);
            return false;
          }
        },
        view: () => ({
          update: (view, before) => {
            if (this.syncing || before.doc.eq(view.state.doc)) return;
            this.synchronized = prepared?.doc === view.state.doc
              ? prepared.markdown : serialize(view.state.doc);
            prepared = undefined;
            changed(this.synchronized);
          },
        }),
      });
    });
    const references = $remark('shioriReferences', () => expandReferences);
    this.editor = Editor.make().config((ctx) => {
      ctx.set(rootCtx, parent);
      ctx.set(defaultValueCtx, markdown);
      ctx.update(codeBlockSchema.key, (original) => (schemaCtx) => {
        const schema = original(schemaCtx);
        return {
          ...schema,
          attrs: { ...schema.attrs, meta: { default: '', validate: 'string' } },
          parseMarkdown: {
            ...schema.parseMarkdown,
            runner: (state, node, type) => {
              state.openNode(type, { language: node.lang ?? '', meta: node.meta ?? '' });
              if (node.value) state.addText(String(node.value));
              state.closeNode();
            },
          },
          toMarkdown: {
            ...schema.toMarkdown,
            runner: (state, node) => {
              state.addNode('code', undefined, node.textContent, {
                lang: node.attrs.language || (node.attrs.meta ? 'text' : ''),
                meta: node.attrs.meta,
              });
            },
          },
        };
      });
      ctx.update(editorViewOptionsCtx, (options) => ({
        ...options,
        editable: () => !this.locked,
        handleDOMEvents: {
          ...options.handleDOMEvents,
          paste: (view, event) => validatePaste(view, event, onError)
            || options.handleDOMEvents?.paste?.(view, event),
        },
        attributes: {
          class: 'document-content',
          'aria-label': 'WYSIWYG本文',
          'aria-multiline': 'true',
          spellcheck: 'false',
        },
      }));
    }).use(commonmark).use(gfm).use(history).use(clipboard).use(references)
      .use($view(imageSchema.node, () => (node) => imageView(node, images, budget)))
      .use($view(htmlSchema.node, () => (node) => htmlView(node, images, budget)))
      .use($view(codeBlockSchema.node, () => (node, view, getPos) => codeView(node, budget, view, getPos, onError)))
      .use($prose(() => codeHighlightPlugin()))
      .use(updates);
  }

  static async create(
    parent: HTMLElement, markdown: string, images: Images,
    changed: (text: string) => void, onError: (error: unknown) => void,
  ) {
    parseMarkdown(markdown);
    const instance = new RichEditor(parent, markdown, images, changed, onError);
    await instance.editor.create();
    instance.syncing = false;
    return instance;
  }

  get view() { return this.editor.action((ctx) => ctx.get(editorViewCtx)); }

  get selection() {
    const { doc, selection } = this.view.state;
    return doc.textBetween(selection.from, selection.to, '\n');
  }

  replace(markdown: string, force = false) {
    if (!force && markdown === this.synchronized) return;
    parseMarkdown(markdown);
    this.syncing = true;
    try {
      this.editor.action(replaceAll(markdown, true));
      this.synchronized = markdown;
    } finally {
      this.syncing = false;
    }
  }

  lock(locked: boolean) {
    this.locked = locked;
    this.view.setProps({ editable: () => !this.locked });
    for (const select of this.view.dom.querySelectorAll<HTMLSelectElement>('select.code-language')) {
      select.disabled = locked;
    }
  }

  format(format: Format, value?: FormatValue) {
    const commands = {
      paragraph: turnIntoTextCommand,
      bold: toggleStrongCommand,
      italic: toggleEmphasisCommand,
      strike: toggleStrikethroughCommand,
      inlineCode: toggleInlineCodeCommand,
      code: createCodeBlockCommand,
      bullet: wrapInBulletListCommand,
      ordered: wrapInOrderedListCommand,
      quote: wrapInBlockquoteCommand,
      undo: undoCommand,
      redo: redoCommand,
    };
    this.view.focus();
    if (format in commands) {
      this.editor.action(callCommand(commands[format as keyof typeof commands].key));
    } else if (/^h[1-6]$/.test(format)) {
      this.editor.action(callCommand(wrapInHeadingCommand.key, Number(format[1])));
    } else if (format === 'image' && value) {
      this.editor.action(callCommand(insertImageCommand.key, { src: value.source, alt: value.label }));
    } else if (format === 'link' && value) {
      if (this.view.state.selection.empty) {
        const text = this.view.state.schema.text(value.label, [
          this.view.state.schema.marks.link.create({ href: value.source }),
        ]);
        this.view.dispatch(this.view.state.tr.replaceSelectionWith(text, false).scrollIntoView());
      } else {
        this.editor.action(callCommand(toggleLinkCommand.key, { href: value.source }));
      }
    } else if (format === 'table') {
      this.editor.action(callCommand(insertTableCommand.key, { row: 2, col: 2 }));
    } else if (format === 'task' || format === 'mermaid') {
      this.editor.action(insert(format === 'task' ? '- [ ] タスク' : '```mermaid\n' + mermaidExample + '\n```'));
    }
  }
}
