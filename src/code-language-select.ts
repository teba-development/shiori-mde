import { languages } from '@codemirror/language-data';
import { codeLanguage } from './code-highlight';

const languagePattern = /^[a-z0-9_+#.-]{1,48}$/i;
const choices = languages.flatMap((description) => {
  const value = [description.name.toLowerCase(), ...description.alias, ...description.extensions]
    .find((name) => languagePattern.test(name) && codeLanguage(name) === description);
  return value ? [{ value, label: description.name, description }] : [];
}).sort((first, second) => first.label.localeCompare(second.label, 'en'));
const options = [
  { value: '', label: 'テキスト（言語なし）' },
  ...choices,
  { value: 'mermaid', label: 'Mermaid' },
];
const selectable = new Set(options.map((option) => option.value));
const currentValues = new WeakMap<HTMLSelectElement, { language: string; value: string }>();

export function updateLanguageSelect(select: HTMLSelectElement, language: string) {
  select.querySelector('option[data-current]')?.remove();
  const name = language.length <= 64 ? language.trim().toLowerCase() : null;
  const plain = name !== null && (!name || ['text', 'txt', 'plaintext', 'none'].includes(name));
  const description = codeLanguage(language);
  const known = description && choices.find((choice) => choice.description === description);
  const choice = plain ? { value: '', label: 'テキスト（言語なし）' }
    : known || (name === 'mermaid' ? { value: 'mermaid', label: 'Mermaid' }
      : { value: language, label: language.slice(0, 80) });
  if (![...select.options].some((option) => option.value === choice.value)) {
    const current = document.createElement('option');
    current.value = choice.value;
    current.textContent = choice.label;
    current.dataset.current = 'true';
    select.append(current);
  }
  select.value = choice.value;
  currentValues.set(select, { language, value: choice.value });
}

export function createLanguageSelect(language: string, change: (language: string) => void) {
  const select = document.createElement('select');
  select.className = 'code-language';
  select.setAttribute('aria-label', 'コードブロックの言語');
  select.title = 'コードブロックの言語';
  let expanded = false;
  function expand() {
    if (expanded) return;
    expanded = true;
    const current = select.value;
    select.replaceChildren();
    for (const { value, label } of options) {
      const option = document.createElement('option');
      option.value = value;
      option.textContent = label;
      select.append(option);
    }
    updateLanguageSelect(select, current);
  }
  updateLanguageSelect(select, language);
  select.addEventListener('pointerdown', expand);
  select.addEventListener('focus', expand);
  select.addEventListener('change', () => {
    const current = currentValues.get(select)!;
    if (select.disabled || !select.isConnected || !selectable.has(select.value)
        || select.value === current.value) {
      updateLanguageSelect(select, current.language);
      return;
    }
    change(select.value);
  });
  return select;
}
