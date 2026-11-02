import { clsx, type ClassValue } from 'clsx';

// Conflict groups for `cn`: two utilities of one group set the same CSS property, and Tailwind
// orders them by its own rules, not by their order in the class string, so an override passed as
// `className` (`px-1` over a primitive's `px-2`) could silently lose. `cn` keeps the last utility
// of each group (per variant prefix), like tailwind-merge does for these groups; tailwind-merge
// itself is not a dependency. Utilities outside these groups pass through untouched.
const GROUPS: readonly (readonly [string, RegExp])[] = [
  ['p', /^p-/],
  ['px', /^px-/],
  ['py', /^py-/],
  ['pt', /^pt-/],
  ['pr', /^pr-/],
  ['pb', /^pb-/],
  ['pl', /^pl-/],
  ['m', /^-?m-/],
  ['mx', /^-?mx-/],
  ['my', /^-?my-/],
  ['mt', /^-?mt-/],
  ['mr', /^-?mr-/],
  ['mb', /^-?mb-/],
  ['ml', /^-?ml-/],
  ['gap', /^gap-(?![xy]-)/],
  ['gap-x', /^gap-x-/],
  ['gap-y', /^gap-y-/],
  ['h', /^h-/],
  ['w', /^w-/],
  ['min-w', /^min-w-/],
  ['max-w', /^max-w-/],
  ['min-h', /^min-h-/],
  ['max-h', /^max-h-/],
  ['rounded', /^rounded(?:-(?:none|xs|sm|md|lg|xl|2xl|3xl|full|\[[^\]]+\]))?$/],
  ['font-weight', /^font-(?:thin|light|normal|medium|semibold|bold|extrabold|black)$/],
  ['text-size', /^text-(?:xs|sm|base|lg|xl|[2-9]xl|\[\d[^\]]*\])$/],
  [
    'text-color',
    /^text-(?:fg(?:-secondary|-tertiary)?|on-accent(?:-muted)?|accent(?:-hover|-fill)?|danger|warning|success|env-[a-z]+(?:-fill)?|inherit|current|white|transparent)$/,
  ],
  [
    'bg',
    /^bg-(?:window|sidebar|toolbar|tabbar|panel|elevated|input|control|btn(?:-hover|-pressed)?|seg-thumb|hover|pressed|selected(?:-focus)?|separator(?:-strong)?|fg(?:-secondary|-tertiary)?|accent(?:-hover|-fill|-soft)?|danger(?:-soft)?|warning(?:-soft)?|success(?:-soft)?|env-[a-z]+(?:-soft|-fill)?|transparent|current|\[[^\]]+\])$/,
  ],
  ['display', /^(?:block|inline-block|inline|flex|inline-flex|grid|inline-grid|hidden|contents)$/],
  ['items', /^items-(?:start|end|center|baseline|stretch)$/],
  ['justify', /^justify-(?:start|end|center|between|around|evenly|stretch)$/],
  ['shadow', /^shadow(?:-|$)/],
];

/** Splits `hover:aria-selected:bg-hover` into its variant prefix and utility (colons in [...] ignored). */
function splitVariant(token: string): [string, string] {
  let depth = 0;
  let cut = -1;
  for (let i = 0; i < token.length; i++) {
    const ch = token[i];
    if (ch === '[' || ch === '(') depth++;
    else if (ch === ']' || ch === ')') depth--;
    else if (ch === ':' && depth === 0) cut = i;
  }
  return cut < 0 ? ['', token] : [token.slice(0, cut + 1), token.slice(cut + 1)];
}

function groupOf(utility: string): string | null {
  const u = utility.startsWith('!') ? utility.slice(1) : utility;
  for (const [name, re] of GROUPS) if (re.test(u)) return name;
  return null;
}

/** Class name joiner (clsx) where a later utility of the same group replaces an earlier one. */
export function cn(...inputs: ClassValue[]): string {
  const joined = clsx(inputs);
  if (!joined.includes(' ')) return joined;
  const tokens = joined.split(/\s+/).filter(Boolean);
  const last = new Map<string, number>();
  const keep = tokens.map(() => true);
  tokens.forEach((t, i) => {
    const [variant, utility] = splitVariant(t);
    const group = groupOf(utility);
    if (!group) return;
    const key = `${variant}${utility.startsWith('!') ? '!' : ''}${group}`;
    const prev = last.get(key);
    if (prev !== undefined) keep[prev] = false;
    last.set(key, i);
  });
  return tokens.filter((_, i) => keep[i]).join(' ');
}

/** Copy text to the clipboard; falls back to a hidden textarea when the async API is missing. */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (typeof navigator !== 'undefined' && navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // fall through to the legacy path
  }
  try {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand('copy');
    document.body.removeChild(ta);
    return ok;
  } catch {
    return false;
  }
}

/** Stable key for a table reference inside one connection. */
export function tableKey(connId: string, ref: { database?: string; schema?: string; name: string }): string {
  return `${connId}/${ref.database ?? ''}/${ref.schema ?? ''}/${ref.name}`;
}

/** `schema.name` or `name` for display. */
export function tableLabel(ref: { schema?: string; name: string }): string {
  return ref.schema ? `${ref.schema}.${ref.name}` : ref.name;
}

