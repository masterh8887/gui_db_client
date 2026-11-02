// Strings of the database password prompt (DESIGN.md §5.I "Prompts").

import { defineStrings } from '@/lib/i18n';

export const dp = defineStrings('dbPassword', {
  en: {
    title: 'Database password',
    label: 'Password for {user} on {name}',
    labelNoUser: 'Password for {name}',
    hint: 'This connection does not store its password. It stays in memory until BulkDB quits.',
    cancel: 'Cancel',
    submit: 'Connect',
    unlocked: '{name}: password accepted',
  },
  ko: {
    title: '데이터베이스 비밀번호',
    label: '{name}의 {user} 비밀번호',
    labelNoUser: '{name} 비밀번호',
    hint: '이 연결은 비밀번호를 저장하지 않습니다. BulkDB를 끝낼 때까지 메모리에만 보관됩니다.',
    cancel: '취소',
    submit: '연결',
    unlocked: '{name}: 비밀번호를 확인했습니다',
  },
});
