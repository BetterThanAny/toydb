# Review and Fix Report

## Changes
- Added recursive grouped-query validation so non-aggregate column references inside SELECT, HAVING, and ORDER BY must be group keys.
- Removed grouped resolver fallback to a representative row for ungrouped columns.
- Added tests for `a + SUM(b)`, HAVING/ORDER BY ungrouped columns, and nested group expressions.
- Implemented tombstone slot reuse by preserving deleted slot offset/capacity and reusing the record area when a new record fits.
- Added storage tests for tombstone reuse without contiguous free space.

## Verification
- `CARGO_TARGET_DIR=/tmp/toydb-target cargo test` passed.
- `git diff --check` passed.

## Remaining
- Multi-row INSERT/UPDATE statement atomicity was not changed; that needs a transaction/snapshot design pass.
