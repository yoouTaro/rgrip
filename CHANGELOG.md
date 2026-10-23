# Changelog

All notable changes to rgrip are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- **Open in console** in a table's right-click menu: a new console on that table's schema, with `SELECT * FROM schema.table LIMIT 100;` ready to run.

## [0.0.14-alpha] - 2026-10-22

### Added
- Your own placeholders in slash commands: any `{name}` (or `{name=default}`) in a template is an argument pasted in as typed. Arguments go in the order the placeholders appear, and the last one takes the rest of the line, so `/people ('a', 'b')` fills `where id in {ids}`. Quotes and brackets keep an earlier argument's spaces together.
- `/new [name]` in the console (Tab, or pick it from the `/` menu) opens the slash command editor on a new command.
- Value popup: double-click a JSON cell for the value indented and colored (keys in their order), with Copy and Copy compact. In read-only results any cell opens it; right-click → **View value** works everywhere.

### Changed
- The slash command editor shows the template and its preview side by side, with the preview colored like the editor and the placeholders still waiting for a value listed under it.

## [0.0.13-alpha] - 2026-10-22

### Added
- Schema diagram: right-click a schema → **Open diagram** for an ER diagram of its tables. Foreign keys are solid lines from the column to the key it points at; for databases that declare none, links guessed from column names are dashed. Drag the boxes, pan and zoom; positions are remembered per connection and schema.
- Relations in query results: a console result knows which table each column comes from, so the row menu's **Related** works there too, and a column that references another table gets a lookup icon in its header that shows the referenced table's rows.
- Total rows of a console result: a paged query shows its total count next to the pager.
- Saved queries remember the console's database and schemas, and reopen with them.

### Changed
- **Copy as IN ('a', 'b')** (was "Copy as SQL list") copies the distinct values of the selected rows quoted and in brackets, ready to paste after `where col in`.
- Sort and the column menu work anywhere on a column header, not just on its name.

### Fixed
- A connection that died while the laptop slept (or the network changed) reconnects by itself on the next action; refreshing no longer needs a disconnect and connect.
- Changing only the database or schema of a saved query's console now saves too.
- The map's area filter works on query results and reloaded pages.
- A lookup no longer gives up on a column that appears twice in a result (`select name, person_category, *`).

## [0.0.12-alpha] - 2026-10-17

### Added
- Map layout: a tab for print maps. Right-click a table with a geometry column → **Add to map layout** (or a connection → **New map layout**) and add more tables from the tab. Set the page (A5–A2, Letter, Legal, Tabloid; portrait or landscape; margin), basemap and scale (1:N), pan and zoom the map on the page, and style each layer (color, line width, point size, fill, legend label, SQL filter). Drag the title, legend, scale bar and north arrow anywhere on the page; they snap to the frame's edges and center. Export as **PDF** (vector shapes and text) or **PNG** at 150, 300 or 600 dpi; the preview, the PNG and the PDF are drawn from the same page. Layers load only the features in view (simplified to print resolution and reprojected by PostGIS, so any SRID works) and reload as you pan and zoom. **Color by** a column gives each value its own color and an editable legend label; the categories, counts and legend follow what's in the frame (up to 12 values, the rest as Other), and a value keeps its color and label as it leaves and comes back into view. Margins can be set per side. Legend labels can break onto several lines. A **graticule** (latitude/longitude grid) can be shown, with an automatic or fixed interval, labels in decimal degrees or degrees-minutes-seconds along the frame, and its own line color and opacity. Title: size, color, bold and a white outline; legend: text size, color and box. Elements snap to the page's center and edges as well as the frame (and just above or below it), with a guide line.
- Sort query results: click a column header in a console result (ascending, descending, then back to the order returned); the header shows an arrow. A single read-only query is sorted by the database, so every page is in order; other results sort the rows already loaded.
- Export map: the **Export map** button on the map saves what you see as a PNG (or copies it), laid out like a printed map: an optional title, a legend (layer and selected features), a scale bar and a north arrow, with the basemap credit. Saved at screen resolution.

## [0.0.11-alpha] - 2026-10-16

### Fixed
- Check for updates no longer fails with "HTTP 403" when GitHub's API limit is reached (60 requests an hour per address): it falls back to the public releases feed, and says so plainly if that fails too.

## [0.0.10-alpha] - 2026-10-16

### Added
- Quotes and brackets close themselves in the console: typing `'`, `"`, `(` or `[` adds the closing one, typing the closer steps over it, Backspace in an empty pair removes both, and a selection gets wrapped.

### Fixed
- Khmer in the console editor is drawn shaped: subscript consonants stack and vowels sit in the right place, like in the grid.

## [0.0.9-alpha] - 2026-10-16

### Added
- Read-only connections: a checkbox in the connection dialog makes the session read-only on the server (including the per-database connections), so Postgres refuses writes with "This connection is read-only". A lock badge shows in the explorer, console toolbar and status bar, and inline editing is turned off.
- Confirm before risky statements: Run / Run all asks first for `DELETE`/`UPDATE` without `WHERE`, `TRUNCATE`, `DROP` and `ALTER … DROP` on any connection, and for any write on a red (production) connection, listing each statement with the reason. Cancel is the default; ⌘↩ runs anyway. Saving inline edits on a red connection asks once.
- Linked map and grid: rows selected in the grid are highlighted on the map (accent color, drawn on top), and clicking a feature selects its row in the grid (⌘-click adds or removes; the grid scrolls to it). The feature popup still opens on click.
- Zoom to selected: a button on the map next to Fit; opening the map with rows selected fits to them.
- Split view: Grid / Map / Split in table tabs and console results, side by side with a resizable divider, live-linked.
- Filter by area: draw a box on the map (button, or hold shift and drag) to set the table's WHERE filter to `ST_Intersects(geom, ST_Transform(ST_MakeEnvelope(…, 4326), srid))` and reload. The area stays on the map until cleared.
- Import data: right-click a schema (Import data…) or a table (Import into this table…) to load a CSV / TSV, GeoJSON or Shapefile. Pick the file, check a 50-row preview, edit column names and inferred types (integer, bigint, numeric, text, boolean, date, timestamp, jsonb, geometry), set the SRID, then create a new table or append to an existing one. CSV delimiter is detected (UTF-8 and BOM, quoted fields), lon/lat columns can become a point column, and a Shapefile's `.prj` is read to guess the SRID (WGS 84, UTM zones, EPSG codes). Rows go in with COPY inside one transaction on a connection of its own, with live progress; any error rolls everything back and names the row.
- Add primary key: a table with no key (often an import or a shapefile load) shows "Add primary key" next to read-only; it adds an identity `id` column after showing the statement, and the rows become editable. Import data adds one by default too (or makes an imported column the key, or none).
- Edit table: right-click a table → Edit table… to rename it, add, rename, retype or drop columns, set not null and defaults, and pick the primary key. The dialog shows the SQL it will run; it applies all or nothing. Dropping columns or working on a red connection is spelled out on the Apply button; read-only connections can't open it.
- ⌘R reloads a table tab (not while edits are pending).
- ⌘/ comments or uncomments the selected lines (or the cursor's line) with `--` in the console.

### Changed
- New site: leads with the map and import; the other features are in one short list.
- The console's schema list stays open while you tick several schemas; click outside to close it.
- Passwords, SSH passwords and key passphrases are stored in the macOS Keychain instead of plain text in `connections.toml`. Existing plain-text secrets migrate automatically; if the Keychain is unavailable rgrip warns once and falls back to the file.

### Fixed
- Spikes on map outlines where a coastline or sliver doubles back on itself: the outline is split at hairpin turns instead of mitered.

## [0.0.8-alpha] - 2026-10-14

### Added
- Smart relations: Related also follows relations inferred from column names and types when there is no foreign key (`orders.customer_id` → `customers.id`, plurals, shared key names like `parcel_code`). Inferred ones are marked `~ inferred`; generic keys such as `objectid` are ignored.
- Go to column: ⌘J, the corner button of any grid, or the header menu. Search, jump and the column flashes.
- Sort indicators: painted arrows on sorted columns, a hint on sortable headers.
- Connection colors: tint every tab of a connection (e.g. red for production); console tabs are titled `name@database`.
- Schema picker per console: queries, paging and Export all run with that schema on the `search_path`; autocomplete and slash commands prefer it.
- Configurable slash commands (Settings → Slash commands…): edit the built-ins or add your own SQL templates with `{table}`, `{field}`, `{field+_4326}` and `{n=100}`, with a live preview. Saved to `commands.toml`.
- Title bar buttons: appearance (System / Light / Dark), check for updates (or "Update to x.y"), GitHub and Sponsor.

## [0.0.7-alpha] - 2026-10-13

### Added
- Date/time picker for inline cell edits, matched to the column type (date, timestamp, timestamptz, time, timetz), with Today / Now in local time. Offsets and fractional seconds are kept.
- Related rows: right-click rows, then Related, to follow foreign keys in both directions (→ users, ← posts) in a popup. Chain further, go Back, or open the result as a tab. Works with self-references and multi-column keys.
- Freeze (pin) columns: right-click a column header, then Freeze column. Frozen columns and row numbers stay on the left while scrolling sideways, and survive reloads and re-runs.
- Landing page, screenshots, dual MIT/Apache-2.0 license, contributing guide and issue templates.

### Fixed
- Map: polygons with a repeated vertex no longer draw spikes across the map.

## [0.0.6-alpha] - 2026-10-13

### Added
- Inline cell editing in table tabs: double-click a cell, type, Enter to keep (Esc cancels). Right-click for Edit cell, Set NULL and Revert cell.
  - Changed cells are highlighted; the toolbar shows N changes with Save and Discard. Save (⌘S) writes one `UPDATE` per row, matched on the primary key, in a single transaction: if any row was changed or deleted meanwhile, nothing is saved.
  - Tables without a primary key, and views, are read-only. Geometry columns are not editable. Paging, sorting and filtering wait until you Save or Discard.
- Copy as image: right-click rows, then Copy as image (or Export, Copy as, Image (PNG)) to put a clean table picture on the clipboard.

### Fixed
- Khmer in table images renders correctly (stacked consonants, mixed Latin/Khmer, no dropped letters).
- No more empty boxes in Khmer text (zero-width spaces, stray vowel marks).
- Map popup: proper copy and close icons, lined up in one column.

## [0.0.5-alpha] - 2026-10-13

### Added
- Format query: ⇧⌘F or the Format button formats the selection, or the whole editor. Slash command lines and comments are kept.
- SVG table export: Save rows as, SVG table (or Copy as, SVG table to paste into Figma), also in Export. Bold header, zebra rows, right-aligned numbers, grey NULLs, geometries as WKT.

### Changed
- Slash commands always expand to a complete query: `/con_4326` alone gives `select t.*, ST_Transform(t.shape, 4326) as shape_4326 from table_name t;`.
- `/con_merc` source SRID defaults to 32648 (UTM 48N); add a number to override, e.g. `/con_merc parcels 3857`.
- Table names in slash commands match regardless of case, and a table the catalog does not know is used as typed.
- Run and Run all expand slash command lines before sending.

### Fixed
- A `-- comment` after the final `;` no longer causes a syntax error.
- Trailing `/* ... */` comments are no longer treated as a separate statement.

## [0.0.4-alpha] - 2026-10-13

### Added
- Slash commands in the editor (Tab to expand, or ⌘↩ to expand and run): `/con_4326`, `/con_4326_2` (ArcGIS via text), `/con_merc`, `/srid`, `/top`, `/count`. Add a field or a table to write the whole query.
- ArcGIS `st_geometry` columns get the same conversions as autocomplete suggestions.
- Connection strings: paste a `postgres://` URL, a `host=... dbname=...` string or a `jdbc:postgresql://` URL into the URL field. Right-click a connection, then Copy connection URL.
- Multi-row selection (click, ⇧-click, ⌘-click, ⌘A, ⌘C) with Copy as CSV / JSON / Markdown / GeoJSON / SQL INSERT / `where id in (...)`, Copy for AI agent (query, column types and rows as Markdown), and Save rows as.
- ⌘N opens a new query.
- Map: features with a vertex far from the rest (e.g. swapped lat/lon) are flagged ("N far off", Hide), and Fit zooms to the real data.
- Map: the feature popup is selectable, with copy buttons (geometries copy as WKT).
- Map: queries from `/con_*` show their `*_4326` column automatically.

## [0.0.3-alpha] - 2026-10-13

### Added
- PostGIS autocomplete: `ST_*` functions with arguments, plus generated SQL for each geometry column based on its SRID (e.g. `ST_Transform(geom, 4326) AS geom`, `ST_AsGeoJSON`, `ST_AsText`, Web Mercator transform).
- Reproject to 4326: when the map cannot draw a geometry's SRID itself, one click re-runs the query with `ST_Transform(..., 4326)`.
- Light basemap (Esri Light Gray), the default in light themes.
- Settings: System / Light / Dark switch with icons, and compact theme lists with color swatches.

### Fixed
- The theme menu no longer stretches across the whole window.
- PostGIS helper schemas (`tiger`, `topology`) no longer clutter autocomplete.

## [0.0.2-alpha] - 2026-10-13

### Added
- Themes: Settings, Appearance (System / Light / Dark), with Dracula, Nord, One Dark Pro, Tokyo Night, Monokai, Catppuccin Mocha and Latte, Gruvbox Dark, GitHub Dark and Light, Solarized Dark and Light, One Light. System follows macOS and switches live.
- Over-the-air updates: rgrip checks GitHub Releases on launch (can be turned off) or via Settings, Check for updates. Install & restart downloads the update, verifies its SHA-256 and replaces the app in place.
- Saved queries are `.sql` files in `~/Library/Application Support/dev.rgrip.rgrip/queries/`. Reveal in Finder, edit in any editor; deleting moves them to the Trash.

### Fixed
- The app icon now has a transparent background (no white square in the Dock).

## [0.0.1-alpha] - 2026-10-13

First alpha: a minimal DataGrip-style PostgreSQL client written in Rust.

### Added
- Connections: SSL from `disable` to `verify-full`, custom CA and client certificates; SSH tunnels with a password or key.
- Explorer: schemas, tables, views and columns, plus saved queries.
- Console: syntax highlighting, line numbers and context-aware autocomplete; ⌘↩ runs a statement, ⇧⌘↩ runs everything, ⌘S saves the query.
- Results: virtualized grid with 1,000-row server-side paging, sorting and filtering.
- Export: CSV, TSV, JSON, GeoJSON, Markdown and SQL INSERT, for the current page or all rows.
- PostGIS: readable geometry cells and a built-in map (dark, streets and satellite basemaps); WGS84, Web Mercator and UTM are reprojected automatically.
- Khmer: correctly shaped text in the grid, explorer and map.

### Known limitations
- Passwords are stored in plain text in `connections.toml` (file mode 0600).
- Khmer typed into the SQL editor is not shaped yet.
- Only macOS on Apple silicon is published; other platforms can build from source.

[Unreleased]: https://github.com/yoouTaro/rgrip/compare/v0.0.14-alpha...HEAD
[0.0.14-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.14-alpha
[0.0.13-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.13-alpha
[0.0.12-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.12-alpha
[0.0.11-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.11-alpha
[0.0.10-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.10-alpha
[0.0.9-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.9-alpha
[0.0.8-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.8-alpha
[0.0.7-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.7-alpha
[0.0.6-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.6-alpha
[0.0.5-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.5-alpha
[0.0.4-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.4-alpha
[0.0.3-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.3-alpha
[0.0.2-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.2-alpha
[0.0.1-alpha]: https://github.com/yoouTaro/rgrip/releases/tag/v0.0.1-alpha
