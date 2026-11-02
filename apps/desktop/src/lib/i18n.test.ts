import { afterEach, describe, expect, it } from 'vitest';
// Every folder string table, so the namespace check sees them all.
import '@/components/layout/strings';
import '@/components/ui/strings';
import '@/stores/strings';
import './env';
import './strings';
import './table-target';
import { defineStrings, listNamespaces, setLang, t } from './i18n';

describe('defineStrings', () => {
  afterEach(() => setLang('ko'));

  it('looks keys up at call time in the current language, with {name} placeholders', () => {
    const ts = defineStrings('test.basic', {
      en: { hello: 'Hello {name}', only: 'English only' },
      ko: { hello: '안녕하세요 {name}', only: '' },
    });
    setLang('ko');
    expect(ts('hello', { name: '훈' })).toBe('안녕하세요 훈');
    setLang('en');
    expect(ts('hello', { name: 'Hun' })).toBe('Hello Hun');
    expect(ts.ns).toBe('test.basic');
    expect(ts.keys).toEqual(['hello', 'only']);
  });

  it('falls back to English, then to the key', () => {
    const ts = defineStrings('test.fallback', {
      en: { a: 'A' },
      ko: { a: undefined as unknown as string },
    });
    setLang('ko');
    expect(ts('a')).toBe('A');
    expect((ts as unknown as (k: string) => string)('missing')).toBe('missing');
  });

  it('tolerates re-registration (HMR replaces the namespace)', () => {
    const first = defineStrings('test.hmr', { en: { k: 'one' }, ko: { k: '하나' } });
    defineStrings('test.hmr', { en: { k: 'two' }, ko: { k: '둘' } });
    setLang('en');
    expect(first('k')).toBe('two');
  });

  it('keeps the shared t() keys working', () => {
    setLang('en');
    expect(t('common.cancel')).toBe('Cancel');
    setLang('ko');
    expect(t('common.cancel')).toBe('취소');
  });

  it('every app folder registers a unique namespace', () => {
    const app = listNamespaces().filter((n) => !n.startsWith('test.'));
    expect(app).toEqual(expect.arrayContaining(['layout', 'ui', 'stores', 'env', 'lib', 'tableTarget']));
    expect(new Set(app).size).toBe(app.length);
  });
});
