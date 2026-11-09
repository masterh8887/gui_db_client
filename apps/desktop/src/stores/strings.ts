// Strings used by the stores (tab titles, workspace notices). DESIGN.md §7.3.

import { defineStrings } from '@/lib/i18n';

export const ts = defineStrings('stores', {
  en: {
    queryTitle: 'Query {n}',
    scriptTitle: 'Script {n}',
    historyTitle: 'History',
    connNotFound: 'Connection not found',
    closeDirtyTitle: 'Close the workspace of {name}?',
    closeDirtyBody: '{count} query tab(s) have unsaved changes. Their SQL is discarded.',
    closeDirtyConfirm: 'Close workspace',
    selectFailed: 'Could not build the extraction query: {error}',
  },
  ko: {
    queryTitle: '쿼리 {n}',
    scriptTitle: '스크립트 {n}',
    historyTitle: '기록',
    connNotFound: '연결을 찾을 수 없습니다',
    closeDirtyTitle: '{name} 작업공간을 닫을까요?',
    closeDirtyBody: '쿼리 탭 {count}개에 저장하지 않은 변경이 있습니다. 해당 SQL은 사라집니다.',
    closeDirtyConfirm: '작업공간 닫기',
    selectFailed: '추출 쿼리를 만들지 못했습니다: {error}',
  },
});
