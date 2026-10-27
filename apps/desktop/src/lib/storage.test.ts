import { afterEach, describe, expect, it, vi } from 'vitest';
import { MemoryStorage, safeLocalStorage } from './storage';

describe('MemoryStorage', () => {
  it('implements the Storage interface', () => {
    const s = new MemoryStorage();
    expect(s.length).toBe(0);
    expect(s.getItem('a')).toBeNull();
    s.setItem('a', '1');
    s.setItem('b', '2');
    expect(s.length).toBe(2);
    expect(s.getItem('a')).toBe('1');
    expect(s.key(1)).toBe('b');
    expect(s.key(5)).toBeNull();
    s.removeItem('a');
    expect(s.getItem('a')).toBeNull();
    s.clear();
    expect(s.length).toBe(0);
  });
});

describe('safeLocalStorage', () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('falls back to memory when localStorage is missing', () => {
    vi.spyOn(window, 'localStorage', 'get').mockReturnValue(undefined as unknown as Storage);
    const s = safeLocalStorage();
    expect(s).toBeInstanceOf(MemoryStorage);
    s.setItem('k', 'v');
    expect(safeLocalStorage().getItem('k')).toBe('v');
  });

  it('falls back to memory when the accessor throws (blocked site data)', () => {
    vi.spyOn(window, 'localStorage', 'get').mockImplementation(() => {
      throw new DOMException('denied', 'SecurityError');
    });
    expect(safeLocalStorage()).toBeInstanceOf(MemoryStorage);
  });

  it('falls back to memory when writes throw (quota / private mode)', () => {
    const broken = new MemoryStorage();
    broken.setItem = () => {
      throw new DOMException('full', 'QuotaExceededError');
    };
    vi.spyOn(window, 'localStorage', 'get').mockReturnValue(broken);
    expect(safeLocalStorage()).not.toBe(broken);
  });

  it('uses the real storage when it works', () => {
    const real = new MemoryStorage();
    vi.spyOn(window, 'localStorage', 'get').mockReturnValue(real);
    expect(safeLocalStorage()).toBe(real);
  });
});
