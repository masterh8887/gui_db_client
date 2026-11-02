// Strings of the console drawer and its toolbar button (DESIGN.md §5.G, §7.3).

import { defineStrings } from '@/lib/i18n';

export const cs = defineStrings('console', {
  en: {
    title: 'Console',
    count: '{n} lines',
    errors: '{n} errors',
    level: 'Minimum level',
    clear: 'Clear console',
    close: 'Close console',
    empty: 'No log entries',
    button: 'Console',
    buttonErrors: 'Console, {n} errors',
    jumpToEnd: 'Jump to latest',
  },
  ko: {
    title: '콘솔',
    count: '{n}줄',
    errors: '오류 {n}건',
    level: '최소 레벨',
    clear: '콘솔 지우기',
    close: '콘솔 닫기',
    empty: '로그 없음',
    button: '콘솔',
    buttonErrors: '콘솔, 오류 {n}건',
    jumpToEnd: '최신 항목으로',
  },
});
