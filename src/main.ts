import { redo, undo } from '@codemirror/commands';
import { getCurrentWindow } from '@tauri-apps/api/window';
import {
  AlignLeft, Bold, BookOpen, Braces, ChevronDown, ChevronUp, CodeXml, Columns2, createElement,
  Download, Eye, FileCode2, FilePlus2, FileText, FolderCheck, FolderOpen, Heading as HeadingIcon, ImagePlus, Italic, Link2, List,
  ListChecks, ListOrdered, ListTree, PanelLeft, PenLine, Quote, Redo2, Save, SaveAll, Search,
  Strikethrough, Table2, Undo2, Workflow, X,
} from 'lucide';
import bookmark from '../assets/shiori-bookmark-v1.png';
import { outlineOf, parseMarkdown, prepareMarkdown, type Heading } from './markdown';
import { domMatches, highlightRanges, revealRange, ScrollSync, textMatches } from './navigation';
import { basename, desktop, escapeHtml, messageOf, native, type OpenDocument } from './platform';
import { contentId, createPreview, diagramData, fragmentId, Images, standaloneHtml, waitForImages } from './preview';
import type { RichEditor } from './rich';
import { linkHref, MAX_SEARCH_MATCHES, MAX_SEARCH_QUERY } from './security';
import { SourceEditor, type Format, type FormatValue } from './source';
import './document.css';
import './style.css';

type Mode = 'view' | 'split' | 'rich';
type Icon = Parameters<typeof createElement>[0];
const svg = (icon: Icon) => createElement(icon, { 'aria-hidden': 'true' }).outerHTML;
const get = <T extends HTMLElement = HTMLElement>(selector: string) => document.querySelector<T>(selector)!;

function button(icon: Icon, label: string, attributes: string, shortcut = '') {
  const title = escapeHtml(shortcut ? `${label} (${shortcut})` : label);
  return `<button class="icon-button" type="button" title="${title}" aria-label="${label}" ${attributes}>${svg(icon)}</button>`;
}

const formatButton = (icon: Icon, label: string, format: Format) => button(icon, label, `data-format="${format}"`);
get('#app').innerHTML = `
  <header class="topbar">
    <div class="brand">
      <img src="${bookmark}" alt="">
      <span>栞</span>
      <small>MARKDOWN EDITOR</small>
    </div>
    <div class="file-tools" role="toolbar" aria-label="ファイル操作">
      ${button(FilePlus2, '新規作成', 'id="file-new"', 'Ctrl+N')}
      ${button(FolderOpen, 'ファイルを開く', 'id="file-open"', 'Ctrl+O')}
      ${button(Save, '上書き保存', 'id="file-save"', 'Ctrl+S')}
      ${button(SaveAll, '名前を付けて保存', 'id="file-save-as"', 'Ctrl+Shift+S')}
      ${button(FolderCheck, '画像の参照フォルダーを許可', 'id="approve-image-folder" hidden')}
    </div>
    <div class="mode-selector" role="group" aria-label="表示切り替え">
      <button type="button" data-mode="view" title="表示 (Ctrl+1)">${svg(Eye)}<span>表示</span></button>
      <button type="button" data-mode="split" title="Markdown編集＋プレビュー (Ctrl+2)">${svg(Columns2)}<span>Markdown<span class="mode-extra">＋プレビュー</span></span></button>
      <button type="button" data-mode="rich" title="WYSIWYG (Ctrl+3)">${svg(PenLine)}<span>WYSIWYG</span></button>
    </div>
    <div class="utilities">
      ${button(PanelLeft, '見出しペインを切り替え', 'id="toggle-outline" aria-controls="sidebar" aria-expanded="true"')}
      ${button(Search, '文書内検索', 'id="open-search" aria-controls="search-bar" aria-expanded="false"', 'Ctrl+F')}
      <button type="button" class="export-button" title="エクスポート" aria-label="エクスポート" popovertarget="export-menu">
        ${svg(Download)}<span>エクスポート</span>${svg(ChevronDown)}
      </button>
      <div id="export-menu" class="popover" popover="auto">
        <button type="button" id="export-pdf">${svg(FileText)}<span>PDFにエクスポート</span></button>
        <button type="button" id="export-html">${svg(FileCode2)}<span>HTMLにエクスポート</span></button>
      </div>
    </div>
  </header>

  <div class="window-body">
    <aside id="sidebar" class="sidebar" aria-label="文書の見出し">
      <div class="outline-header">
        ${svg(ListTree)} <h2>見出し</h2><span id="heading-count">0</span>
        ${button(X, '見出しペインを隠す', 'id="hide-outline"')}
      </div>
      <nav id="outline" aria-label="見出しへ移動"></nav>
      <p id="outline-empty">本文に見出しを追加すると<br>ここに表示されます。</p>
    </aside>

    <main class="editor-area">
      <div class="document-header">
        <div class="document-glyph">${svg(FileText)}</div>
        <div class="document-name">
          <div class="eyebrow">MARKDOWN DOCUMENT</div>
          <h1 id="file-name">無題.md</h1>
        </div>
        <span id="dirty-indicator" class="dirty-indicator" title="未保存の変更" hidden></span>
      </div>
      <div id="formatbar" class="formatbar" role="toolbar" aria-label="書式設定">
        ${formatButton(Undo2, '元に戻す', 'undo')}${formatButton(Redo2, 'やり直す', 'redo')}
        <span class="tool-divider"></span>
        <button type="button" id="heading-format" class="heading-format" title="段落・見出し" aria-label="段落・見出し" popovertarget="heading-menu">
          ${svg(HeadingIcon)}${svg(ChevronDown)}
        </button>
        <div id="heading-menu" class="popover" popover="auto">
          <button type="button" data-format="paragraph">${svg(AlignLeft)}<span>段落</span></button>
          ${[1, 2, 3, 4, 5, 6].map((level) => `<button type="button" data-format="h${level}"><span class="heading-icon">H${level}</span><span>見出し ${level}</span></button>`).join('')}
        </div>
        ${formatButton(Bold, '太字', 'bold')}${formatButton(Italic, '斜体', 'italic')}
        ${formatButton(Strikethrough, '取り消し線', 'strike')}
        <span class="tool-divider"></span>
        ${formatButton(List, '箇条書き', 'bullet')}${formatButton(ListOrdered, '番号付きリスト', 'ordered')}
        ${formatButton(ListChecks, 'チェックリスト', 'task')}${formatButton(Quote, '引用', 'quote')}
        <span class="tool-divider"></span>
        ${formatButton(CodeXml, 'インラインコード', 'inlineCode')}${formatButton(Braces, 'コードブロック', 'code')}
        ${formatButton(Link2, 'リンクを挿入', 'link')}${formatButton(ImagePlus, '画像を挿入', 'image')}
        ${formatButton(Table2, '表を挿入', 'table')}${formatButton(Workflow, 'Mermaid図を挿入', 'mermaid')}
      </div>
      <div id="search-bar" class="search-bar" hidden>
        ${svg(Search)}
        <input id="search-input" type="search" maxlength="${MAX_SEARCH_QUERY}" aria-label="文書を検索" placeholder="文書を検索…" autocomplete="off">
        <span id="search-count" aria-live="polite"></span>
        ${button(ChevronUp, '前の一致', 'id="search-prev"')}${button(ChevronDown, '次の一致', 'id="search-next"')}
        ${button(X, '検索を閉じる', 'id="search-close"')}
      </div>

      <div id="workbench" class="workbench" data-mode="split">
        <section class="source-pane" aria-label="Markdown編集">
          <header class="pane-label"><i></i>MARKDOWN<span>ソース</span></header>
          <div id="source-editor" class="source-editor"></div>
        </section>
        <section class="reading-pane" aria-label="文書プレビュー">
          <header class="pane-label"><i></i><span id="reading-label">PREVIEW</span><span id="reading-caption">プレビュー</span></header>
          <div id="preview-scroll" class="reading-scroll" tabindex="0" aria-label="プレビュー本文">
            <div id="preview-empty" class="preview-empty">
              <div>${svg(BookOpen)}</div><h2>ここにプレビューを表示</h2>
              <p id="empty-caption">左のエディタに Markdown を入力すると、ここに表示されます。</p>
            </div>
            <div id="preview-article" class="article-wrap"></div>
          </div>
        </section>
        <section class="rich-pane" aria-label="WYSIWYG編集">
          <header class="pane-label"><i></i>WYSIWYG<span>ビジュアル編集</span></header>
          <div id="rich-scroll" class="reading-scroll">
            <div id="rich-editor" class="article-wrap"></div>
          </div>
        </section>
      </div>
    </main>
  </div>
  <footer class="statusbar">
    <span class="status-state"><i></i><span id="status-message">準備完了</span></span>
    <span id="status-lines">1 行</span>
    <span id="status-path"></span>
    <span class="encoding">UTF-8</span>
  </footer>
  <div id="notification" class="notification" role="status" hidden>
    <span></span>${button(X, '通知を閉じる', 'id="notification-close"')}
  </div>
  <dialog id="prompt"></dialog>
`;

const app = get('#app');
const workbench = get('#workbench');
const outline = get('#outline');
const previewPane = get('#preview-scroll');
const previewWrap = get('#preview-article');
const richPane = get('#rich-scroll');
const richMount = get('#rich-editor');
const searchBar = get('#search-bar');
const searchInput = get<HTMLInputElement>('#search-input');
const prompt = get<HTMLDialogElement>('#prompt');
const notification = get('#notification');
const images = new Images();

let mode: Mode = 'split';
let markdown = '';
let baseline = '';
let path: string | null = null;
let documentRevision: number | null = null;
let headings: Heading[] = [];
let rich: RichEditor | undefined;
let busy = false;
let renderTimer: number | undefined;
let noticeTimer: number | undefined;
let rendering: AbortController | undefined;
let activeLine = 1;
let activeMatch = 0;

const source = new SourceEditor(get('#source-editor'), changed, (error) => notice(messageOf(error), true));
const sync = new ScrollSync(source, previewPane, () => mode === 'split', updateActiveHeading);
new ResizeObserver(() => {
  sync.invalidate();
  if (mode === 'split') sync.fromSource(true);
}).observe(previewWrap);

function dirty() { return markdown !== baseline; }

function updateDocumentHeader() {
  const name = basename(path);
  get('#file-name').textContent = name;
  get('#file-name').title = path ?? name;
  get('#dirty-indicator').hidden = !dirty();
  get('#approve-image-folder').hidden = !path;
  get('#status-path').textContent = path ?? '';
  get('#status-path').title = path ?? '';
  get('#status-lines').textContent = `${markdown.split('\n').length.toLocaleString('ja-JP')} 行`;
  const title = `${dirty() ? '● ' : ''}${name} — 栞`;
  if (document.title !== title) desktop.title(title);
}

function notice(message: string, error = false) {
  window.clearTimeout(noticeTimer);
  notification.querySelector('span')!.textContent = message;
  notification.dataset.error = String(error);
  notification.hidden = false;
  if (!error) noticeTimer = window.setTimeout(() => { notification.hidden = true; }, 2900);
}

function updateActiveHeading(line: number) {
  activeLine = line;
  const active = headings.findLastIndex((heading) => heading.line <= line + 0.2);
  [...outline.children].forEach((button, index) => {
    if (active === index) button.setAttribute('aria-current', 'location');
    else button.removeAttribute('aria-current');
  });
}

function richHeadings() {
  return rich?.view.dom.querySelectorAll('h1, h2, h3, h4, h5, h6');
}

function updateOutline(items: Heading[]) {
  headings = items;
  const minimum = items.reduce((level, heading) => Math.min(level, heading.level), 6);
  const entries = document.createDocumentFragment();
  items.forEach((heading, index) => {
    const item = document.createElement('button');
    item.type = 'button';
    item.className = 'outline-item';
    item.style.setProperty('--depth', String(heading.level - minimum));
    item.title = heading.label;
    const level = document.createElement('span');
    level.className = 'outline-level';
    level.textContent = `H${heading.level}`;
    const label = document.createElement('span');
    label.className = 'outline-label';
    label.textContent = heading.label;
    item.append(level, label);
    item.addEventListener('click', () => {
      updateActiveHeading(heading.line);
      if (mode === 'rich') {
        const target = richHeadings()?.[index];
        if (target) richPane.scrollTop += target.getBoundingClientRect().top - richPane.getBoundingClientRect().top - 24;
      } else {
        if (mode === 'split') source.jump(heading.line);
        const target = previewWrap.querySelector('#' + CSS.escape(contentId(`shiori-heading-${heading.id}`)));
        if (target) previewPane.scrollTop += target.getBoundingClientRect().top - previewPane.getBoundingClientRect().top - 26;
      }
    });
    entries.append(item);
  });
  outline.replaceChildren(entries);
  get('#outline-empty').hidden = items.length > 0;
  get('#heading-count').textContent = String(items.length);
  updateActiveHeading(activeLine);
}

function renderDocument() {
  window.clearTimeout(renderTimer);
  rendering?.abort();
  const { signal } = rendering = new AbortController();
  try {
    const renderedMarkdown = markdown;
    const tree = parseMarkdown(renderedMarkdown);
    if (mode === 'rich') {
      updateOutline(outlineOf(tree));
      refreshSearch();
      return;
    }
    const oldTop = previewPane.scrollTop;
    const preview = createPreview(tree, images, {
      interactive: true,
      signal,
      changeCodeLanguage: mode === 'split' ? (block, language) => {
        if (busy || mode !== 'split') return;
        try {
          source.setCodeLanguage(block, language, renderedMarkdown);
        } catch (error) {
          notice(messageOf(error), true);
        }
        renderDocument();
      } : undefined,
    });
    updateOutline(preview.outline);
    get('#preview-empty').hidden = markdown.trim().length > 0;
    previewWrap.replaceChildren(preview.article);
    if (mode === 'view') previewPane.scrollTop = oldTop;
    sync.invalidate();
    sync.fromSource(true);
    refreshSearch();
    void preview.ready.then(() => {
      if (signal.aborted) return;
      sync.invalidate();
      sync.fromSource(true);
      refreshSearch();
    }).catch((error) => {
      if (!signal.aborted) notice(messageOf(error), true);
    });
  } catch (error) {
    previewWrap.replaceChildren();
    updateOutline([]);
    sync.invalidate();
    notice(messageOf(error), true);
  }
}

function changed(text: string) {
  markdown = text;
  rendering?.abort();
  updateDocumentHeader();
  window.clearTimeout(renderTimer);
  renderTimer = window.setTimeout(renderDocument, 110);
}

function load(document: OpenDocument | null) {
  markdown = baseline = document?.markdown ?? '';
  path = document?.path ?? null;
  documentRevision = document?.revision ?? null;
  images.reset(documentRevision);
  source.reset(markdown);
  if (mode === 'rich') refreshRich(true);
  previewPane.scrollTop = richPane.scrollTop = 0;
  activeLine = 1;
  activeMatch = 0;
  updateDocumentHeader();
  renderDocument();
}

function refreshRich(force: boolean) {
  try {
    rich!.replace(markdown, force);
    rich!.view.dom.dataset.documentPath = path ?? '';
  } catch (error) {
    mode = 'split';
    updateModeButtons();
    notice(`Markdown編集モードで文書を表示します。\n${messageOf(error)}`, true);
  }
}

function updateModeButtons() {
  workbench.dataset.mode = mode;
  get('#formatbar').hidden = mode === 'view';
  for (const button of document.querySelectorAll<HTMLButtonElement>('.mode-selector button')) {
    button.setAttribute('aria-pressed', String(button.dataset.mode === mode));
  }
  get('#reading-label').textContent = mode === 'view' ? 'READING' : 'PREVIEW';
  get('#reading-caption').textContent = mode === 'view' ? '表示' : 'プレビュー';
  get('#empty-caption').textContent = mode === 'view'
    ? 'Markdownファイルを開くか、編集モードで本文を入力してください。'
    : '左のエディタに Markdown を入力すると、ここに表示されます。';
}

async function setMode(next: Mode) {
  if (mode === next) return;
  const targetLine = activeLine;
  if (next === 'rich') {
    if (!rich) {
      const { RichEditor } = await import('./rich');
      rich = await RichEditor.create(richMount, markdown, images, changed, (error) => notice(messageOf(error), true));
    } else rich.replace(markdown, rich.view.dom.dataset.documentPath !== (path ?? ''));
    rich.view.dom.dataset.documentPath = path ?? '';
  }
  source.replace(markdown);
  mode = next;
  updateModeButtons();
  renderDocument();
  const nearest = headings.findLastIndex((heading) => heading.line <= targetLine);
  if (mode === 'rich') {
    const target = richHeadings()?.[nearest] as HTMLElement | undefined;
    richPane.scrollTop = target ? target.getBoundingClientRect().top - richPane.getBoundingClientRect().top + richPane.scrollTop - 24 : 0;
    requestAnimationFrame(() => rich!.view.focus());
  } else if (mode === 'split') {
    source.scrollToLine(targetLine);
    sync.fromSource(true);
    requestAnimationFrame(() => source.view.focus());
  } else {
    sync.goToPreview(targetLine);
    updateActiveHeading(targetLine);
  }
}

function setOutlineVisible(visible: boolean) {
  get('#sidebar').hidden = !visible;
  get('#toggle-outline').setAttribute('aria-expanded', String(visible));
  sync.invalidate();
}

function setSearchVisible(visible: boolean) {
  if (visible === !searchBar.hidden) {
    if (visible) searchInput.focus();
    return;
  }
  searchBar.hidden = !visible;
  get('#open-search').setAttribute('aria-expanded', String(visible));
  activeMatch = 0;
  if (visible) {
    if (mode === 'split' && source.selection && !source.selection.includes('\n')) {
      searchInput.value = source.selection.slice(0, MAX_SEARCH_QUERY);
    }
    searchInput.focus();
    searchInput.select();
  } else if (mode === 'split') source.view.focus();
  else if (mode === 'rich') rich!.view.focus();
  else previewPane.focus();
  refreshSearch(visible);
}

function refreshSearch(reveal = false, step = 0) {
  const query = searchBar.hidden ? '' : searchInput.value;
  const rendered = mode === 'rich' ? richMount : previewWrap;
  const ranges = domMatches(rendered, query);
  const matches = mode === 'split' ? textMatches(source.text, query) : [];
  const count = mode === 'split' ? matches.length : ranges.length;
  activeMatch = count ? (Math.min(activeMatch, count - 1) + step + count) % count : 0;
  source.find(matches, activeMatch, mode === 'split' && reveal);
  highlightRanges(ranges, mode === 'split' ? -1 : activeMatch);
  if (mode !== 'split' && reveal && ranges[activeMatch]) {
    revealRange(mode === 'view' ? previewPane : richPane, ranges[activeMatch]);
  }
  const countLabel = count === MAX_SEARCH_MATCHES ? `${count}以上` : String(count);
  get('#search-count').textContent = query ? (count ? `${activeMatch + 1} / ${countLabel}` : '一致なし') : '';
  get<HTMLButtonElement>('#search-prev').disabled = !count;
  get<HTMLButtonElement>('#search-next').disabled = !count;
}

function showPrompt(content: string, read?: () => unknown) {
  prompt.innerHTML = content;
  prompt.returnValue = '';
  for (const button of prompt.querySelectorAll<HTMLButtonElement>('[data-response]')) {
    button.addEventListener('click', () => prompt.close(button.dataset.response));
  }
  return new Promise<{ response: string; value: unknown }>((resolve) => {
    prompt.addEventListener('close', () => resolve({ response: prompt.returnValue, value: read?.() }), { once: true });
    prompt.showModal();
  });
}

async function save(as: boolean) {
  const revision = documentRevision;
  const saveAs = as || !path;
  const response = await showPrompt(`
    <div class="dialog-eyebrow">SAVE MARKDOWN</div>
    <h2>${saveAs ? '名前を付けて保存' : '上書き保存'}</h2>
    <p class="dialog-description">${escapeHtml(basename(path))} の保存オプション</p>
    <label class="save-option"><input id="embed-images" type="checkbox">
      <span><strong>ローカル画像を埋め込む</strong><small>相対パスの画像を文書に含めます。</small></span>
    </label>
    <label class="save-option"><input id="embed-mermaid" type="checkbox">
      <span><strong>Mermaid図をSVGとして埋め込む</strong><small>MermaidのコードブロックをSVG画像に置き換えます。</small></span>
    </label>
    <div class="dialog-actions">
      <button type="button" data-response="cancel">キャンセル</button>
      <button type="button" class="primary" data-response="save" autofocus>${saveAs ? '保存先を選ぶ' : '保存'}</button>
    </div>`, () => ({
    images: prompt.querySelector<HTMLInputElement>('#embed-images')!.checked,
    diagrams: prompt.querySelector<HTMLInputElement>('#embed-mermaid')!.checked,
  }));
  if (response.response !== 'save') return false;
  const options = response.value as { images: boolean; diagrams: boolean };
  const ticket = saveAs ? await desktop.chooseSavePath() : null;
  if (saveAs && ticket === null) return false;
  try {
    const image = options.images ? (url: string) => images.local(url)
      : ticket !== null && path ? (url: string) => desktop.rebaseImage(url, ticket) : undefined;
    const saved = await prepareMarkdown(markdown, image, options.diagrams ? diagramData : undefined);
    const result = await desktop.save(saved, ticket, revision);
    const moved = documentRevision !== result.revision;
    path = result.path;
    documentRevision = result.revision;
    markdown = baseline = saved;
    if (moved) images.reset(documentRevision);
    source.replace(saved);
    if (mode === 'rich') refreshRich(moved);
    updateDocumentHeader();
    renderDocument();
    notice('保存しました');
    return true;
  } finally {
    if (ticket !== null) await desktop.cancelSavePath(ticket).catch(console.error);
  }
}

async function canDiscard() {
  if (!dirty()) return true;
  const { response } = await showPrompt(`
    <div class="dialog-eyebrow">UNSAVED CHANGES</div>
    <h2>変更を保存しますか？</h2>
    <p class="dialog-description">${escapeHtml(basename(path))} に未保存の変更があります。</p>
    <div class="dialog-actions">
      <button type="button" data-response="cancel" autofocus>キャンセル</button>
      <button type="button" data-response="discard">保存せず続ける</button>
      <button type="button" class="primary" data-response="save">保存</button>
    </div>`);
  return response === 'discard' || (response === 'save' && await save(false));
}

async function operation(label: string, work: () => Promise<unknown>, lock = true) {
  if (busy) return;
  busy = true;
  app.dataset.busy = 'true';
  get('#status-message').textContent = label;
  if (lock) {
    source.lock(true);
    rich?.lock(true);
    for (const select of previewWrap.querySelectorAll<HTMLSelectElement>('select.code-language')) select.disabled = true;
  }
  try {
    await work();
  } catch (error) {
    notice(messageOf(error), true);
  } finally {
    busy = false;
    app.dataset.busy = 'false';
    get('#status-message').textContent = '準備完了';
    if (lock) {
      source.lock(false);
      rich?.lock(false);
      for (const select of previewWrap.querySelectorAll<HTMLSelectElement>('select.code-language')) select.disabled = false;
    }
  }
}

async function exportDocument(kind: 'html' | 'pdf') {
  const base = basename(path).replace(/\.[^.]+$/, '');
  const preview = createPreview(parseMarkdown(markdown), images, { interactive: false, strictImages: true });
  await preview.ready;
  if (kind === 'html') {
    await waitForImages(preview.article);
    if (await desktop.exportHtml(standaloneHtml(preview.article, base))) notice('HTMLを出力しました');
    return;
  }
  const sheet = get('#print-sheet');
  const content = document.createDocumentFragment();
  while (preview.article.firstChild) content.append(preview.article.firstChild);
  sheet.replaceChildren(content);
  try {
    await waitForImages(sheet);
    if (await desktop.exportPdf()) notice('PDFを出力しました');
  } finally {
    sheet.replaceChildren();
  }
}

function applyFormat(format: Format, value?: FormatValue) {
  if (mode === 'rich') rich!.format(format, value);
  else if (format === 'undo') { undo(source.view); source.view.focus(); }
  else if (format === 'redo') { redo(source.view); source.view.focus(); }
  else source.format(format, value);
}

function formatAction(format: Format) {
  if (mode === 'view' || busy) return;
  if (format === 'image') {
    void operation('画像を選択中…', async () => {
      const image = await desktop.chooseImage();
      if (image) applyFormat('image', { source: image.source, label: image.alt });
    }, false);
  } else if (format === 'link') {
    void operation('リンクを挿入…', async () => {
      const selection = mode === 'rich' ? rich!.selection : source.selection;
      const { response, value } = await showPrompt(`
        <div class="dialog-eyebrow">INSERT LINK</div><h2>リンクを挿入</h2>
        <label class="link-label">URL<input id="link-url" maxlength="8192" placeholder="https://example.com" autofocus></label>
        <div class="dialog-actions"><button type="button" data-response="cancel">キャンセル</button>
        <button type="button" class="primary" data-response="insert">挿入</button></div>`,
      () => prompt.querySelector<HTMLInputElement>('#link-url')!.value);
      if (response !== 'insert' || !value) return;
      const url = linkHref(String(value));
      if (!url) throw new Error('リンクのURLを確認してください。');
      applyFormat('link', { source: url, label: selection || url });
    }, false);
  } else applyFormat(format);
}

function newFile() {
  void operation('新規文書を作成中…', async () => {
    if (!await canDiscard()) return;
    await desktop.newDocument();
    load(null);
  });
}

function openFile() {
  void operation('ファイルを選択中…', async () => {
    if (!await canDiscard()) return;
    const document = await desktop.open();
    if (document) load(document);
  });
}

function selectMode(next: Mode) {
  void operation('モードを切り替え中…', () => setMode(next));
}

get('#file-new').onclick = newFile;
get('#file-open').onclick = openFile;
get('#file-save').onclick = () => { void operation('保存中…', () => save(false)); };
get('#file-save-as').onclick = () => { void operation('保存中…', () => save(true)); };
get('#approve-image-folder').onclick = () => {
  void operation('参照フォルダーを選択中…', async () => {
    if (documentRevision === null || !await desktop.approveImageFolder(documentRevision)) return;
    images.reset(documentRevision);
    if (mode === 'rich') refreshRich(true);
    renderDocument();
    notice('この文書の画像の参照フォルダーを許可しました');
  });
};
get('#toggle-outline').onclick = () => setOutlineVisible(get('#sidebar').hidden === true);
get('#hide-outline').onclick = () => setOutlineVisible(false);
get('#open-search').onclick = () => setSearchVisible(searchBar.hidden === true);
get('#search-close').onclick = () => setSearchVisible(false);
get('#search-prev').onclick = () => refreshSearch(true, -1);
get('#search-next').onclick = () => refreshSearch(true, 1);
get('#notification-close').onclick = () => { notification.hidden = true; };
searchInput.addEventListener('input', () => { activeMatch = 0; refreshSearch(true); });

for (const kind of ['pdf', 'html'] as const) {
  get(`#export-${kind}`).onclick = () => {
    get('#export-menu').hidePopover();
    void operation(`${kind.toUpperCase()}を出力中…`, () => exportDocument(kind));
  };
}

document.querySelectorAll<HTMLButtonElement>('.mode-selector button').forEach((button) => {
  button.onclick = () => selectMode(button.dataset.mode as Mode);
});
document.querySelectorAll<HTMLButtonElement>('#formatbar button').forEach((button) => {
  button.addEventListener('pointerdown', (event) => event.preventDefault());
  if (!button.dataset.format) return;
  button.onclick = () => {
    get('#heading-menu').hidePopover();
    formatAction(button.dataset.format as Format);
  };
});

function positionPopover(trigger: HTMLElement, menu: HTMLElement, right = false) {
  trigger.addEventListener('click', () => {
    const rect = trigger.getBoundingClientRect();
    menu.style.top = `${rect.bottom + 6}px`;
    if (right) menu.style.right = `${window.innerWidth - rect.right}px`;
    else menu.style.left = `${rect.left}px`;
  });
}
positionPopover(get('#heading-format'), get('#heading-menu'));
positionPopover(get('.export-button'), get('#export-menu'), true);

richPane.addEventListener('scroll', () => {
  if (mode !== 'rich') return;
  const top = richPane.getBoundingClientRect().top + 30;
  let index = -1;
  richHeadings()?.forEach((heading, position) => {
    if (heading.getBoundingClientRect().top <= top) index = position;
  });
  updateActiveHeading(headings[index]?.line ?? 1);
}, { passive: true });

function activateDocumentLink(event: MouseEvent) {
  if (!(event.target instanceof Element)) return;
  const link = event.target.closest('a');
  if (!link) return;
  const container = richMount.contains(link) ? richMount : previewWrap.contains(link) ? previewWrap : null;
  if (!container) return;
  event.preventDefault();
  if (event.type !== 'click' || !event.isTrusted) return;
  if (mode === 'rich' && !event.ctrlKey && !event.metaKey) return;
  const href = linkHref(link.getAttribute('href') ?? '');
  if (!href) return;
  const anchor = fragmentId(href);
  if (anchor) {
    container.querySelector('#' + CSS.escape(anchor))?.scrollIntoView({ block: 'start' });
  } else if (/^(https?:\/\/|mailto:)/i.test(href)) {
    void desktop.openLink(href).catch((error) => notice(messageOf(error), true));
  }
}
document.addEventListener('click', activateDocumentLink);
document.addEventListener('auxclick', activateDocumentLink);

document.addEventListener('keydown', (event) => {
  if (prompt.open || event.isComposing) return;
  const key = event.key.toLowerCase();
  const control = event.ctrlKey || event.metaKey;
  if (key === 'escape' && !searchBar.hidden) {
    event.preventDefault();
    setSearchVisible(false);
  } else if ((key === 'enter' && event.target === searchInput) || key === 'f3') {
    event.preventDefault();
    if (searchBar.hidden) setSearchVisible(true);
    else refreshSearch(true, event.shiftKey ? -1 : 1);
  } else if (control) {
    const shortcuts: Record<string, () => void> = {
      n: newFile,
      o: openFile,
      s: () => { void operation('保存中…', () => save(event.shiftKey)); },
      f: () => setSearchVisible(true),
      '1': () => selectMode('view'),
      '2': () => selectMode('split'),
      '3': () => selectMode('rich'),
    };
    if (shortcuts[key]) {
      event.preventDefault();
      shortcuts[key]();
    }
  }
}, { capture: true });

updateModeButtons();
updateDocumentHeader();
renderDocument();
void operation('起動時のファイルを確認中…', async () => {
  if (native) {
    await getCurrentWindow().onCloseRequested((event) => {
      if (!busy && !dirty()) return;
      event.preventDefault();
      if (!busy) void operation('終了を確認中…', async () => {
        if (await canDiscard()) await getCurrentWindow().destroy();
      });
    });
  }
  const document = await desktop.startup();
  if (document) load(document);
});
