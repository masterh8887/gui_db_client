// Pure helpers behind the plan tree: node ids, relation labels, cost shares and the mapping of
// plan warnings onto the nodes they talk about. No React here, so it is unit-testable.

import type { PlanNode, PlanWarning, Severity } from 'ipc-types';

/** Id of the root node; a child's id is `<parent id>.<index>` (`0.1.0`). */
export const ROOT_ID = '0';

export function childId(parentId: string, index: number): string {
  return `${parentId}.${index}`;
}

/** Ids of the ancestors of `id`, root first (`0.1.2` -> `['0', '0.1']`). */
export function ancestorIds(id: string): string[] {
  const parts = id.split('.');
  return parts.slice(1).map((_, i) => parts.slice(0, i + 1).join('.'));
}

/** Depth-first walk with ids. */
export function walkPlan(root: PlanNode, visit: (node: PlanNode, id: string, depth: number) => void): void {
  const go = (node: PlanNode, id: string, depth: number) => {
    visit(node, id, depth);
    node.children.forEach((c, i) => go(c, childId(id, i), depth + 1));
  };
  go(root, ROOT_ID, 0);
}

/** Ids of every node that has children (what "collapse all" collapses). */
export function parentIds(root: PlanNode | undefined): string[] {
  const out: string[] = [];
  if (root) walkPlan(root, (n, id) => n.children.length > 0 && out.push(id));
  return out;
}

export function countNodes(root: PlanNode | undefined): number {
  let n = 0;
  if (root) walkPlan(root, () => n++);
  return n;
}

/** `schema.name` / `name` of the node's relation, if any. */
export function relationLabel(node: PlanNode): string | undefined {
  const r = node.relation;
  if (!r?.name) return undefined;
  return r.schema ? `${r.schema}.${r.name}` : r.name;
}

/**
 * Cost the whole plan is measured against: the summary's total cost, or the largest node cost
 * (node costs include their children, so that is normally the root).
 */
export function planTotalCost(root: PlanNode | undefined, summaryTotal: number | undefined): number | undefined {
  let max = summaryTotal !== undefined && Number.isFinite(summaryTotal) && summaryTotal > 0 ? summaryTotal : 0;
  if (root) walkPlan(root, (n) => (max = Math.max(max, n.totalCost ?? 0)));
  return max > 0 ? max : undefined;
}

/** Share (0..1) of `total` taken by the node's (cumulative) total cost. */
export function costShare(node: PlanNode, total: number | undefined): number | undefined {
  if (node.totalCost === undefined || !total) return undefined;
  return Math.max(0, Math.min(1, node.totalCost / total));
}

export const SEVERITY_RANK: Record<Severity, number> = { info: 0, warning: 1, danger: 2 };

export function worstSeverity(warnings: readonly PlanWarning[]): Severity | undefined {
  let worst: Severity | undefined;
  for (const w of warnings) if (!worst || SEVERITY_RANK[w.severity] > SEVERITY_RANK[worst]) worst = w.severity;
  return worst;
}

// ---- warnings -> nodes ---------------------------------------------------------------------

export interface WarningMatches {
  /** Node id -> the warnings that point at it, in warning order. */
  byNode: Map<string, PlanWarning[]>;
  /** For each warning (same index as the input), the ids of the nodes it points at. */
  nodesOf: string[][];
}

const escapeRe = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

/** Whole-word (identifier-boundary) occurrence of `needle`; case-insensitive for relations. */
function mentions(text: string, needle: string, caseSensitive: boolean): boolean {
  if (!needle) return false;
  return new RegExp(`(^|[^\\w$])${escapeRe(needle)}(?![\\w$])`, caseSensitive ? '' : 'i').test(text);
}

/**
 * Node types the text names, longest first and without overlaps, so `Hash Join` does not also
 * count as `Hash`. Case-sensitive: plan node types are capitalised (`Sort`), prose is not.
 */
function mentionedNodeTypes(text: string, types: readonly string[]): Set<string> {
  const taken: [number, number][] = [];
  const found = new Set<string>();
  for (const type of [...types].sort((a, b) => b.length - a.length)) {
    const re = new RegExp(`(^|[^\\w$])(${escapeRe(type)})(?![\\w$])`, 'g');
    for (const m of text.matchAll(re)) {
      const start = (m.index ?? 0) + m[1].length;
      const end = start + type.length;
      if (taken.some(([s, e]) => start < e && end > s)) continue;
      taken.push([start, end]);
      found.add(type);
    }
  }
  return found;
}

/** Integers written in the text (`5,000,000` -> 5000000), for telling same-type nodes apart. */
function numbersIn(text: string): Set<number> {
  const out = new Set<number>();
  for (const m of text.matchAll(/\d[\d,]*/g)) {
    const n = Number(m[0].replace(/,/g, ''));
    if (Number.isFinite(n)) out.add(n);
  }
  return out;
}

/**
 * Node kinds implied by well-known warning codes (`PLAN_SORT_SPILL`, `SEQ_SCAN_LARGE`,
 * `LATERAL_PATTERN`, `HASH_LARGE`, ...), for warnings whose text names no node.
 */
const CODE_NODE_KINDS: [RegExp, RegExp][] = [
  [/LATERAL/, /Nested Loop/],
  [/SEQ_SCAN|FULL_SCAN/, /Seq Scan|Table Scan|Index Scan/],
  [/SORT/, /Sort/],
  [/HASH/, /Hash/],
];

/**
 * Point each warning at the plan nodes it refers to. Warnings carry only text, so the match is
 * by what the text names:
 *
 * 1. a node of a type the text names (`Sort`, `Nested Loop`, `Hash Match`) that reads a relation
 *    the text names (`public.orders`, else a bare table name, else an alias of 3+ characters);
 * 2. otherwise the nodes of the named types — narrowed, when there are several, by a row count in
 *    the text and then to those above a scan of a named relation. A relation that only appears in
 *    a qualified sort key or hash condition (`orders.created_at`) thus never pulls a Sort/Hash
 *    warning down onto the scan;
 * 3. otherwise, when the text names no node type, the nodes reading a named relation;
 * 4. otherwise the node kind its code implies (`SORT_SPILL` -> sorts), but only when that points
 *    at a single node, directly or through a row count in the text.
 *
 * A warning that matches nothing is only listed above the tree.
 */
export function matchWarnings(root: PlanNode | undefined, warnings: readonly PlanWarning[]): WarningMatches {
  const byNode = new Map<string, PlanWarning[]>();
  const nodesOf: string[][] = warnings.map(() => []);
  if (!root || warnings.length === 0) return { byNode, nodesOf };

  const nodes: { id: string; node: PlanNode }[] = [];
  walkPlan(root, (node, id) => nodes.push({ id, node }));
  const types = [...new Set(nodes.map((n) => n.node.nodeType).filter((t) => t.trim().length > 0))];

  warnings.forEach((w, wi) => {
    const text = `${w.message}\n${w.detail ?? ''}`;
    const named = mentionedNodeTypes(text, types);

    const qualified = nodes.filter((n) => n.node.relation?.schema && mentions(text, relationLabel(n.node)!, false));
    const bareName = qualified.length > 0 ? qualified : nodes.filter((n) => n.node.relation?.name && mentions(text, n.node.relation.name, false));
    const byRelation =
      bareName.length > 0
        ? bareName
        : nodes.filter((n) => n.node.alias && n.node.alias.length >= 3 && mentions(text, n.node.alias, false));

    const numbers = numbersIn(text);
    const rowCountNamed = (n: (typeof nodes)[number]) => n.node.planRows !== undefined && numbers.has(Math.round(n.node.planRows));
    const byRowCount = (candidates: typeof nodes) => candidates.filter(rowCountNamed);
    /** `candidates` cut down by `keep`, unless that would leave none. */
    const narrow = (candidates: typeof nodes, keep: (n: (typeof nodes)[number]) => boolean) => {
      if (candidates.length <= 1) return candidates;
      const kept = candidates.filter(keep);
      return kept.length > 0 ? kept : candidates;
    };

    let hits: typeof nodes;
    const relationTyped = byRelation.filter((n) => named.has(n.node.nodeType));
    if (relationTyped.length > 0) {
      // "Seq Scan on public.orders": the named node that reads the named relation.
      hits = relationTyped;
    } else if (named.size > 0) {
      // The text names a node type, and no node of that type reads a named relation: the
      // relation is incidental (a qualified `Sort Key` / `Hash Cond` such as `orders.created_at`),
      // so it may only narrow the named nodes — to those above a scan of it — never replace them.
      hits = nodes.filter((n) => named.has(n.node.nodeType));
      hits = narrow(hits, rowCountNamed);
      hits = narrow(hits, (n) => byRelation.some((r) => r.id === n.id || r.id.startsWith(`${n.id}.`)));
    } else if (byRelation.length > 0) {
      hits = byRelation;
    } else {
      const kind = CODE_NODE_KINDS.find(([code]) => code.test(w.code))?.[1];
      const candidates = kind ? nodes.filter((n) => kind.test(n.node.nodeType)) : [];
      hits = candidates.length === 1 ? candidates : byRowCount(candidates);
      if (hits.length > 1) hits = [];
    }

    for (const { id } of hits) {
      nodesOf[wi].push(id);
      const list = byNode.get(id);
      if (list) list.push(w);
      else byNode.set(id, [w]);
    }
  });
  return { byNode, nodesOf };
}
