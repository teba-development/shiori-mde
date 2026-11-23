import { invoke, isTauri } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';

export interface SavedDocument { path: string; revision: number }
export interface OpenDocument extends SavedDocument { markdown: string }
interface ChosenImage { source: string; alt: string }

export const native = isTauri();

export const desktop = {
  startup: () => native ? invoke<OpenDocument | null>('startup_document') : Promise.resolve(null),
  newDocument: () => native ? invoke<void>('new_document') : Promise.resolve(),
  open: () => invoke<OpenDocument | null>('open_document'),
  chooseSavePath: () => invoke<number | null>('choose_save_path'),
  cancelSavePath: (ticket: number) => invoke<void>('cancel_save_path', { ticket }),
  save: (markdown: string, ticket: number | null, revision: number | null) =>
    invoke<SavedDocument>('save_document', { markdown, ticket, revision }),
  readImage: (source: string, revision: number) => invoke<string>('read_image', { source, revision }),
  rebaseImage: (source: string, ticket: number) => invoke<string>('rebase_image', { source, ticket }),
  approveImageFolder: (revision: number) => invoke<boolean>('approve_image_folder', { revision }),
  chooseImage: () => invoke<ChosenImage | null>('choose_image'),
  openLink: (url: string) => native ? invoke<void>('open_link', { url }) : Promise.resolve(window.open(url, '_blank', 'noopener,noreferrer')),
  exportHtml: (html: string) => invoke<boolean>('export_html', { html }),
  exportPdf: () => invoke<boolean>('export_pdf'),
  title: (title: string) => {
    document.title = title;
    if (native) void getCurrentWindow().setTitle(title).catch(console.error);
  },
};

export const basename = (path: string | null) => path?.split(/[\\/]/).pop() || '無題.md';
export const messageOf = (error: unknown) => error instanceof Error ? error.message : String(error);

export function escapeHtml(value: string) {
  return value.replace(/[&<>"']/g, (character) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  })[character]!);
}
