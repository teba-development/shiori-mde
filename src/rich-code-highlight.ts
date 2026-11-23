import type { Node as ProseNode } from '@milkdown/kit/prose/model';
import { Plugin, PluginKey } from '@milkdown/kit/prose/state';
import { Decoration, DecorationSet } from '@milkdown/kit/prose/view';
import { codeTokens, MAX_HIGHLIGHT_LENGTH, MAX_HIGHLIGHT_TOKENS } from './code-highlight';

const key = new PluginKey<DecorationSet>('shiori-code-highlight');
async function decorationsFor(doc: ProseNode, active: () => boolean) {
  const blocks: { source: string; language: string; position: number }[] = [];
  let remaining = MAX_HIGHLIGHT_LENGTH;
  doc.descendants((node, position) => {
    if (node.type.name !== 'code_block') return;
    const source = node.textContent;
    if (source.length <= remaining) {
      remaining -= source.length;
      blocks.push({ source, language: String(node.attrs.language ?? ''), position });
    }
    return false;
  });
  const decorations: Decoration[] = [];
  for (const { source, language, position } of blocks) {
    if (!active()) return null;
    const tokens = await codeTokens(source, language);
    if (decorations.length + tokens.length > MAX_HIGHLIGHT_TOKENS) break;
    for (const { from, to, classes } of tokens) {
      decorations.push(Decoration.inline(position + 1 + from, position + 1 + to, { class: classes }));
    }
  }
  return active() ? DecorationSet.create(doc, decorations) : null;
}

export function codeHighlightPlugin() {
  return new Plugin<DecorationSet>({
    key,
    state: {
      init: () => DecorationSet.empty,
      apply: (transaction, previous) => transaction.getMeta(key)
        ?? previous.map(transaction.mapping, transaction.doc),
    },
    props: { decorations: (state) => key.getState(state) },
    view(view) {
      let timer: number | undefined;
      let revision = 0;
      function schedule() {
        window.clearTimeout(timer);
        const token = ++revision;
        const doc = view.state.doc;
        const active = () => token === revision && doc === view.state.doc;
        timer = window.setTimeout(() => {
          void decorationsFor(doc, active).then((decorations) => {
            if (decorations && active()) view.dispatch(view.state.tr.setMeta(key, decorations));
          });
        }, 80);
      }
      schedule();
      return {
        update: (_view, before) => { if (before.doc !== view.state.doc) schedule(); },
        destroy() {
          revision += 1;
          window.clearTimeout(timer);
        },
      };
    },
  });
}
