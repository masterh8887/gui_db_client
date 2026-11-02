// Connection environment tags (DESIGN.md §7.2): local, development, testing, staging,
// production. The tag key stays English in both languages (`production`, `PRODUCTION` in the
// status pill); pickers show `production — 운영`. Colours come from the `--env-<tag>` tokens.
//
// `ConnTag` and the optional `Connection.tag` come from `ipc-types` (core typegen). `connTag` still
// validates the value, so an unknown tag from a newer backend reads as untagged.

import type { Connection, ConnTag } from 'ipc-types';
import { defineStrings } from './i18n';

export type EnvTag = ConnTag;

export const ENV_TAGS: readonly EnvTag[] = ['local', 'development', 'testing', 'staging', 'production'];

/** Welcome "group by tag" order: the riskiest first, untagged last. */
export const ENV_GROUP_ORDER: readonly EnvTag[] = ['production', 'staging', 'testing', 'development', 'local'];

const ts = defineStrings('env', {
  en: {
    local: 'Local',
    development: 'Development',
    testing: 'Testing',
    staging: 'Staging',
    production: 'Production',
    none: 'No tag',
  },
  ko: {
    local: '로컬',
    development: '개발',
    testing: '테스트',
    staging: '스테이징',
    production: '운영',
    none: '태그 없음',
  },
});

export function isEnvTag(v: unknown): v is EnvTag {
  return typeof v === 'string' && (ENV_TAGS as readonly string[]).includes(v);
}

/** The connection's tag, or `undefined` (no tag, unknown value, field absent). */
export function connTag(conn: Pick<Connection, 'tag'> | undefined): EnvTag | undefined {
  const tag: unknown = conn?.tag;
  return isEnvTag(tag) ? tag : undefined;
}

/** 로컬 / 개발 / 테스트 / 스테이징 / 운영 (en: Local …). */
export function envLabel(tag: EnvTag): string {
  return ts(tag);
}

/** "태그 없음" / "No tag" (grouping header, picker option). */
export function noTagLabel(): string {
  return ts('none');
}

/** `production — 운영` (pickers). */
export function envOptionLabel(tag: EnvTag): string {
  return `${tag} — ${envLabel(tag)}`;
}

/**
 * Copy of a `conn_save` input (or a connection) with `tag` set, or removed when `undefined`
 * (omitting `tag` clears it in core).
 */
export function withConnTag<T extends { tag?: ConnTag }>(input: T, tag: EnvTag | undefined): T {
  const out = { ...input };
  if (tag) out.tag = tag;
  else delete out.tag;
  return out;
}

/** CSS custom-property references for a tag. */
export function envVars(tag: EnvTag): { color: string; soft: string; fill: string } {
  return { color: `var(--env-${tag})`, soft: `var(--env-${tag}-soft)`, fill: `var(--env-${tag}-fill)` };
}
