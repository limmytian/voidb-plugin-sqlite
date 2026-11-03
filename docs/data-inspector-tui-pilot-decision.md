# SQLite Data-Inspector TUI Pilot Decision (Requirement 83)

- **Plugin / profile type**: `voidb-plugin-sqlite` / SQLite file profiles
  (`path` only; a SQLite file holds exactly one database, and the plugin
  declares `secret_classes = []`).
- **Selected state**: `Accepted pilot`.
- **Command**: `voidb-plugin-sqlite tui` (plugin-owned, CLI-launched), also
  routed as `sqlite tui` through `voidb-cli`.
- **Scope**: local file schema triage. This is new plugin-owned UI work built
  on the existing `SqliteService` channel contract (`SyncWorker`); it adds no
  backend operations and does not depend on removed router-hosted factories,
  shared legacy data widgets, or shell-owned plugin UI state.
- **Gate precedent**: this record follows the accepted Redis data-inspector
  TUI pilot (`voidb-plugin-redis/docs/data-inspector-tui-pilot-decision.md`),
  the canonical data-TUI implementation, and its SQL adaptations
  (`voidb-plugin-mysql`, `voidb-plugin-postgres`), which established that a
  bounded plugin-owned pilot with fixture, target error, and real PTY
  evidence produced in the same requirement satisfies the Requirement 83
  gate. SQLite previously sat at `Later candidate`, shipping as `CLI-only`
  in `voidb/docs/data-inspector-cli-first-decisions.md` because the gate
  needed "a dedicated table-page/table-count capability contract, a
  row-mutation preview, and PTY evidence". The bounded read path
  (`SchemaCommand::ListTables` with exact per-table row counts plus
  `LoadCommand::LoadTableData { table, page_size, offset }` — `LIMIT
  page_size OFFSET offset` bound in `service/data_loader.rs`) and the
  PK-anchored bounded delete path (`CrudCommand::SaveChanges` with an empty
  pending-edit set and one deleted row index, anchored by
  `service/value_convert.rs::build_pk_where`) already exist in the plugin
  service layer, so that precondition is met and the pilot is implemented,
  validated, and accepted here.

## Mutation scope decision (recorded up front)

**Slice 1 ships the MySQL-style bounded PK-anchored row delete.** Unlike the
PostgreSQL service (which shipped READ-ONLY because its save surface could
not anchor a bounded delete), the SQLite `CrudCommand::SaveChanges` handler
executes one `DELETE FROM "table" WHERE …` per deleted row index, with the
WHERE clause built by `service/value_convert.rs::build_pk_where` from the
primary-key columns of the page snapshot. Sending an **empty pending-edit
set plus a single deleted row index** therefore produces exactly one
PK-anchored DELETE inside one transaction — there is no bulk, table-level,
or unbounded delete anywhere in the pilot:

- the first viewport is read-only; deletes exist only inside the table data
  viewer (`d`);
- the delete requires **typed confirmation of the exact table name** plus an
  explicit `y` (two confirmations, `destructive` risk);
- tables without a primary key refuse up front (`build_pk_where` errors on
  empty `pk_columns`; the fixture `keyvalue` table exercises the refusal);
- `--readonly` launches stage plans but block at confirm with
  `readonly launch blocked delete row on <table>`;
- the current page offset is refetched after every mutation so the deleted
  row disappears and totals stay accurate;
- row `UPDATE` cell edits, `InsertRow`, `PasteRows`, `ImportCsv`, and all
  `BrowserCommand` DDL are **`later candidate`** and unreachable from the
  TUI. Mutation policy is recorded in `evidence["mutation_rescan_policy"]`
  and `evidence["plan_transcript"]`.

## Named human workflow

**Schema triage on a local SQLite store.** An operator investigates a
data-quality or over-retention report against an application's local SQLite
file (its `users`, `sessions`, and `documents` tables):

1. **Start**: the operator opens the file through an existing profile
   (`sqlite tui --profile app-store --purpose triage`). The TUI opens
   read-only on the tables view; the first viewport is the catalog with
   name/type/exact row count per table (the service listing runs one
   bounded `COUNT(*)` per table), so oversized tables are visible before
   any of them is opened.
2. **Structure**: the operator presses `2` on `sessions`; the structure
   view renders columns (nullable/PK/default markers), the secondary index
   `idx_sessions_user_id`, the foreign key to `users (on_delete=CASCADE)`,
   and the `CREATE TABLE` DDL — read-only, bounded to twelve lines with a
   visible truncation marker.
3. **Page**: pressing `i` on `users` opens the full-screen TABLE DATA
   VIEWER on the first bounded page (`LIMIT 5 OFFSET 0` through the
   service's paged loader). `n` fetches `offset + 5`, `b` walks back; pages
   are **replaced**, not appended, so the viewport stays bounded. `/`
   filters the current page client-side with a live match count.
4. **Inspect**: `Enter` on a row opens the typed row detail viewer — one
   line per column with declared type and NULL/NOT NULL/PK flags. NULL
   renders literally; a BLOB cell renders its `[BLOB N bytes]` size label
   plus a bounded hex head (`hex de ad be ef …`, first 16 bytes only),
   mirroring the storage probe in `service/value_convert.rs::
   decode_sqlite_value`; long text stays inside the 2,000-line / 2 MiB
   bounded viewer buffer.
5. **Act**: for a poisoned artifact row the operator presses `d`, **types
   the exact table name**, and presses `y`; one PK-anchored
   `DELETE FROM "users" WHERE "id" = 1` is sent through the existing
   `SaveChanges` path, and the current page is refetched so the row
   disappears and the total drops to 11. A failed delete (for example
   `SQLITE_BUSY` on a file locked by a concurrent writer) shows a redacted
   target error in a non-modal banner and the operator keeps working.
6. **Success state**: without leaving the TUI, the operator has walked
   catalog → structure → page → row and removed one identified row, with
   the refreshed page as confirmation. Row edits and inserts are **not**
   part of this slice (explicitly `later candidate` below).

**Why the CLI is materially worse here**: the same loop requires one
`sqlite query "SELECT * FROM users LIMIT 5 OFFSET m"` invocation per page
plus a hand-written `sqlite exec "DELETE FROM users WHERE id = 1"` per
candidate row — and `sqlite tables` / `sqlite describe` for the structural
context. Every step pays a fresh process that re-opens the file, and none
share table, page, filter, or cursor context; the operator retypes table
names and re-derives pagination offsets in shell history. The TUI keeps the
table selection, page offset state, per-page filter, and typed row
inspection in one viewport with persistent per-view selection memory, so an
operator walks a suspect table page by page in one sitting. Generic "browse
data" is not the claim; the claim is cursor-preserving local triage over
repeated page-inspect cycles on the same file.

## Gate-by-gate evidence

| Gate | Evidence in this implementation |
|---|---|
| Named human workflow | Above; exercised end to end in fixture mode and by the PTY transcript (open `users` → page 1→2→3 with a partial final page → viewer filter `alice` → 1 match → typed delete of row `id=1`, refetch shows `rows 1-5 of 11` and the row gone). |
| Capability-backed bounded reads | Reads are the existing service-layer operations only: `SchemaCommand::ListTables` (sqlite_master listing with one bounded `COUNT(*)` per table — exact counts, not estimates), `SchemaCommand::LoadStructure { table }` (columns + indexes + foreign keys), `SchemaCommand::ShowCreateTable { table }` (read-only DDL render), and `LoadCommand::LoadTableData { table, page_size, offset }` — `LIMIT page_size OFFSET offset` bound in `service/data_loader.rs`, with total count and PK columns piggybacked by the same command. The TUI adds no new backend operations; there is no free-form SQL surface in the pilot. |
| Pagination and large-result safety | One page (`PAGE_SIZE = 5` rows) is rendered at a time; `n` fetches `offset + PAGE_SIZE` only and pages are replaced, never appended, so the viewport stays bounded. The viewer title reports `rows X-Y of T offset N`, so empty state (`empty_archive`: "No rows in this table (total 0)"), partial last page (`users`: 12 rows over 3 pages), and end-of-table state ("end of table reached") are all visible. Row-viewer bodies are bounded by `BoundedLines` at 2,000 lines / 2 MiB with dropped-line counters in the status panel, grid cells are capped at 28 chars with an ellipsis, and BLOB hex heads render at most 16 bytes. Structure DDL renders at most 12 lines with an explicit "N more DDL lines (bounded)" marker. |
| Redaction and error shape | SQLite profiles carry no credentials (`secret_classes = []`); preflight (`--format json`) reports `"auth": "none"` with `"secret_material": "redacted"` and the **file basename only** — a full path can embed the operator's username, so `"diagnostics_include_full_file_path": false`. `secret_leak_markers` scan covers `sqlite_password_value`, `sqlite_secret_data_value`, `plaintext_credential_value`, `raw_connection_url` and passes (`passed: true`, 0 markers) in `build_sqlite_tui_evidence` output. Errors render through `safe_error_summary` in the non-modal banner and keep the service-layer `target error:` category text (validation/auth/permission/transport/timeout/plugin/target categories come from the shared capability error contract). |
| Mutation safety | **First viewport is read-only.** `--readonly` launch stages plans but blocks at confirm with `readonly launch blocked delete row on <table>`. Row `DELETE` requires **typed confirmation of the exact table name** plus explicit `y` (two confirmations, `destructive` risk), is refused up front when the table has no primary key, is dropped as stale when the staged page is no longer current, and sends exactly one PK-anchored `DELETE FROM <table> WHERE …` through the existing `CrudCommand::SaveChanges` path with an empty pending-edit set and a single deleted row index. Row `UPDATE` (SaveChanges cell edits), `InsertRow`, `PasteRows`/`ImportCsv`, and all `BrowserCommand` DDL are **`later candidate`** and unreachable from the TUI; the structure view renders CREATE TABLE DDL read-only and never executes it. |
| Fixture and target-error evidence | `fixtures/sqlite_tui_operations.json` covers 6 tables and 1 view: exact row counts, typed columns with PK flags, 12-row paging over 5-row pages (full/partial/empty), NULL text/integer cells, BLOB payloads, JSON cells, a deliberately long-text cell (`users.settings_json` for `mallory`), a no-primary-key table (`keyvalue`), an empty table (`empty_archive`), per-table structure snapshots with indexes/FKs/DDL, and a deliberately erroring table (`locked_table` → `target error: SQLITE_BUSY: database file is locked while mutating locked_table (fixture)`). Startup carries a `target error: SQLITE_BUSY: database file is locked by a concurrent writer (fixture)` banner; the PTY proves the TUI stays fully responsive across it (all assertions pass with the banner present on the first frame). |
| PTY lifecycle evidence | Real PTY run (`pty.fork()` + `TIOCSWINSZ` `HHHH` at 36x130, winsize set pre-exec): 57/57 assertions across 3 consecutive clean runs, exit code 0 each run. First frame markers (`SQLite Data TUI`, `1:tables`, `2:structure`, `fixture-sqlite-ops`, `fixture-sqlite-local`, fixture rows, hint footer, target-error banner); quit exits 0; resize to 24x100 keeps content visible and reports `resized sqlite view to 100x24`; raw-mode cleanup is delegated to `ratatui::restore()` after the loop (raw input is not used: `raw_input = false`). |
| No legacy dependency | The command lives in the plugin (`src/tui.rs`), is launched from the plugin CLI (`sqlite tui`), consumes only `SqliteService` channel events over the `SyncWorker`, and contains no reference to removed router-hosted data-inspector adapters or shared legacy widgets. The agent-session/agent_live machinery is untouched. |

## Interaction surface

- Views and hotkeys: `1` tables browser, `2` structure (Tab cycles); pending
  table/structure/data fetches show the family `⟳` marker in the header
  tabs. **Hotkey 3 (server view) is deferred**: the SQLite service channel
  contract exposes no bounded diagnostics command, and SQLite is embedded —
  there is no server process to introspect; recorded as `later candidate`
  in `evidence["deferred"]`.
- Tables browser: name/type/exact row count columns with a views counter in
  the list title; `/` filters the table list client-side with a live match
  count; `r` refreshes.
- `i`/`Enter` on a table opens the full-screen TABLE DATA VIEWER: one
  bounded LIMIT/OFFSET page at a time with `j/k`/PgUp/PgDn row cursor, `n`
  next page / `b` back (replace semantics), `/` live page filter with a
  live match count, and `r` refetch.
- Row inspect: `Enter` on a row opens the row detail viewer (typed column
  rendering: name, declared type, NULL/NOT NULL and PK flags, value; NULL
  renders literally; BLOB renders a size label plus a bounded hex head)
  with scroll and a bounded line budget.
- Structure view: columns list (nullable/PK/default) plus an indexes,
  foreign-keys, and read-only CREATE TABLE DDL panel for the selected
  table.
- Mutation: `d` on a row (data viewer or row viewer) stages a typed delete
  (type the exact table name, then `y`); readonly launches stage but block
  at confirm; the current page is refetched after execution.
- Confirm-quit fires while a table/structure/data fetch or a staged plan is
  active; the error banner is non-modal (any key acknowledges while the key
  keeps its meaning).

## Deferred inside the accepted pilot

- **Server view (hotkey 3)**: `later candidate` — no bounded diagnostics
  command exists in the service channel contract, and SQLite is embedded
  (no server process). If PRAGMA-based diagnostics (`page_count`,
  `integrity_check`, `journal_mode`) gain a bounded service command later,
  they can slot into hotkey 3 with their own fixture and PTY evidence.
- **Row edits and inserts** (`CrudCommand::SaveChanges` cell updates,
  `CrudCommand::InsertRow`): `later candidate` — value-editing UX needs its
  own fixture, confirmation, and redaction evidence before it can ship.
- **Paste and import** (`CrudCommand::PasteRows`, `ImportCsv`): `later
  candidate` — bulk write paths outside the slice 1 mutation gate.
- **DDL and table management** (`BrowserCommand` drop/rename/duplicate,
  `ExecuteSql`): `later candidate`; blast radius is beyond any slice 1
  mutation gate. The structure view renders the CREATE TABLE DDL read-only
  and never executes it.
- **Server-side list filtering**: `later candidate`;
  `SchemaCommand::ListTables` exposes no filter parameter, so the tables
  filter is client-side only.
- Until these ship, docs and release copy must keep pointing to the CLI
  (`sqlite query`, `sqlite tables`, `sqlite describe`, `sqlite exec`, and
  `voidb-cli invoke run sqlite.<capability> …`) as the supported general
  path for writes, arbitrary queries, and diagnostics; `plugin.toml`
  declares `[ui] tui = true, raw_input = false` for this bounded pilot
  only.

## Validation commands and evidence

All run in `voidb-plugin-sqlite` with `VOIDB_DISABLE_KEYRING=1` (no OS
keyring touch):

```bash
VOIDB_DISABLE_KEYRING=1 cargo test            # 66 passed (48 pre-existing + 18 TUI)
cargo clippy --all-targets -- -D warnings     # 0 warnings
cargo fmt --all && cargo fmt --all -- --check # clean (CI enforces this; pre-existing drift fixed separately)
cargo run --example export_contract -- --check # manifest/contract drift green
```

TUI interaction tests (in `src/tui.rs#[cfg(test)]`): fixture evidence +
secret-leak scan, preflight basename-only redaction, non-modal startup
banner, view hotkeys and tab cycling, tables browser row counts, data-viewer
paging with offset/total state and partial/end states, filter match count in
browser and viewer, row viewer typed columns with NULL and BLOB hex head
plus long-text bounded buffer, structure view columns/indexes/FKs/DDL plus
missing-structure target error, typed-delete exact-table-name gate,
delete-execute + page refetch, delete refusal without a primary key,
readonly block, erroring-action responsiveness, empty table state,
confirm-quit, page-key movement, and bounded cell rendering helpers.

Preflight and evidence:

```bash
target/debug/voidb-plugin-sqlite tui --fixture fixtures/sqlite_tui_operations.json --format json
target/debug/voidb-plugin-sqlite tui --fixture fixtures/sqlite_tui_operations.json --evidence evidence.json
```

Evidence JSON: `kind: sqlite_tui_fixture_evidence`, quality gate
`retained_tui_quality_gate("sqlite", ["fixture-sqlite-ops",
"fixture-sqlite-local"], ["target error"], 80, 24, 10_000)`, 24 coverage
entries including the new interactions (`tables_browser_row_counts`,
`table_data_paging`, `null_value_rendering`, `blob_value_rendering`,
`structure_viewer_columns_indexes_fks`, `structure_ddl_read_only`,
`typed_delete_confirmation`, `delete_requires_primary_key`,
`mutation_refetch`, …), `secret_leak_scan.passed = true`.

PTY lifecycle record (macOS, Python 3.9 stdlib only, `struct "HHHH"`, 36
rows x 130 cols): 57/57 assertions, 3 consecutive clean runs, exit code 0
each run; script retained out-of-repo at `/tmp/sqlite_tui_pty_smoke.py` (run
`python3 -I /tmp/sqlite_tui_pty_smoke.py`). Covered: first frame
(non-blank, marker-checked), banner acknowledgement, full-screen data viewer
with offset paging forward/back and partial final page, viewer filter with
live and applied match counts and clear, typed row detail with NULL literal
and BLOB hex head, structure view with indexes/FKs/read-only DDL, erroring
mutation (`SQLITE_BUSY`) banner with the row retained, typed-delete
confirmation and page refetch showing the row gone and the total dropped,
live resize with content retention, normal `q` quit with clean exit 0 and
terminal restore. One PTY finding was fixed during the loop: the data-viewer
popup uses the family 84% height so the status line under the overlay stays
visible. Local Python is 3.9 (no tomllib), so the python provenance tests
cannot run on this machine (CI runs 3.11+); this does not affect the PTY
evidence, which uses only stdlib.

## Migration note

This pilot is new plugin-owned UI work. It does not restore, wrap, or depend
on the router-hosted SQLite data-inspector adapters removed in Requirement
77, and it does not reintroduce shared legacy data widgets or shell-owned
plugin UI state. The default TUI guidance for SQLite profiles remains the
CLI/capability path; `sqlite tui` is a bounded, profile-scoped triage pilot
whose only supported mutation is a single PK-anchored row delete. Release
docs must not advertise the deferred server view, row-edit, insert,
paste/import, or DDL workflows as available in the TUI.
