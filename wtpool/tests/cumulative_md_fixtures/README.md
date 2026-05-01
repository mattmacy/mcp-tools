# cumulative_md auto-resolve fixtures

Each subdirectory holds three files:

- `ours.md` — the `<<<<<<<` half (HEAD side at rebase time).
- `theirs.md` — the `=======` half (incoming branch's lines).
- `expected.md` — what the heuristic must produce.

The test driver (`merge_dry_run.rs`) wraps `ours.md` and `theirs.md`
with `<<<<<<<` / `=======` / `>>>>>>>` markers, embeds them between a
>=100-line preamble and >=50-line footer, and asserts
`resolve_cumulative_md_conflict(...)` produces a body that, after
stripping the same preamble + footer, exactly matches `expected.md`.

## Provenance

`git log --merges --oneline --grep="cumulative" -- <cumulative.md>`
returned exactly one merge commit (`981a8b9b`) at the time these
fixtures were authored, and that merge had no conflict (it was a
clean fast-forward of a doc-only branch). So:

**These fixtures are SYNTHESIZED from realistic shapes**, not extracted
from real conflict resolutions. Replace each `case-*` with a real
case the first time a real cumulative-md conflict arises in
production. The synthetic shapes are still load-bearing — they assert
the heuristic resolves the table-row + branch-comment patterns the
spec §3.5 describes — but a fixture pulled from a real merge is more
adversarial because it carries whatever idiosyncrasies the actual
session-state introduced.

## Cases

- `case-table-rows-only/` — pure `| ... |` table-row append on each
  side. The most common conflict shape.
- `case-branch-comments-and-rows/` — interleaves
  `<!-- branch-name: ... -->` comments with table rows on each side.
  Asserts the comment + row stay grouped after the union-merge.
- `case-overlapping-row-collapses/` — both sides happen to add the
  identical row. Asserts the heuristic deduplicates rather than
  emitting the row twice.
- `case-three-rows-each-different-branches/` — three rows per side,
  six distinct branch names. Asserts the lex-by-branch-name ordering
  is stable across larger inputs.
