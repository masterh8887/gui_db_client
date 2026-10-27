// CI gate: regenerate the ts-rs bindings and fail when they differ from the committed files.
//
// Two checks, both must pass:
//  1. Drift: the files under packages/ipc-types/src/generated before and after
//     `cargo test -p bulk-core --features ts-export export_bindings` must be identical
//     (compared with line endings normalised, so a CRLF checkout on Windows is not drift).
//  2. Committed: inside a git work tree, `git status --porcelain` for packages/ipc-types/src
//     must be empty after regeneration. Generated files that are untracked or modified would
//     make CI (which only sees committed files) fail, so they fail here too. Skipped when the
//     source is not a git checkout (e.g. a release tarball).
//
// Usage: node scripts/typegen-check.mjs   (or `pnpm typegen:check`)
import { execFileSync, execSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// fileURLToPath decodes percent-escapes, so non-ASCII checkout paths (e.g. a Korean user
// name) resolve to the real directory instead of a non-existent `%EC%9D%B4...` one.
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const pkgSrc = path.join('packages', 'ipc-types', 'src');
const dir = path.join(root, pkgSrc, 'generated');

/** File name -> content with CRLF normalised to LF (ts-rs always writes LF). */
function snapshot() {
  const out = {};
  if (!fs.existsSync(dir)) return out;
  for (const f of fs.readdirSync(dir).sort()) {
    out[f] = fs.readFileSync(path.join(dir, f), 'utf8').replace(/\r\n/g, '\n');
  }
  return out;
}

/** `git status --porcelain` lines for the ipc-types sources, or null outside a git checkout. */
function uncommitted() {
  const git = (args) =>
    execFileSync('git', args, { cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
  try {
    if (git(['rev-parse', '--is-inside-work-tree']).trim() !== 'true') return null;
  } catch {
    return null; // git missing or not a repository
  }
  const out = git(['status', '--porcelain', '--untracked-files=all', '--', pkgSrc.split(path.sep).join('/')]);
  return out.split('\n').filter((l) => l.trim() !== '');
}

const before = snapshot();
execSync('cargo test -p bulk-core --features ts-export export_bindings', { cwd: root, stdio: 'inherit' });
const after = snapshot();

const files = new Set([...Object.keys(before), ...Object.keys(after)]);
const diffs = [...files].filter((f) => before[f] !== after[f]);
if (diffs.length) {
  console.error('ipc-types are out of date. Run `pnpm typegen` and commit:\n  ' + diffs.join('\n  '));
  process.exit(1);
}

const pending = uncommitted();
if (pending && pending.length) {
  console.error(
    'ipc-types match Rust but are not committed (CI only sees committed files). Commit:\n  ' +
      pending.join('\n  '),
  );
  process.exit(1);
}

console.log(
  'ipc-types up to date (' + files.size + ' files' + (pending ? ', committed' : ', git check skipped') + ')',
);
