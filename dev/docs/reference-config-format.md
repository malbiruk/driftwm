# `config.reference.toml` format

`config.reference.toml` is both a working config (copy it, uncomment what you
want) and the single source of truth that two tools parse:

- a **test** that asserts the documented defaults match the compiled-in defaults
  (so the file can never silently drift from the code), and
- a **doc generator** that renders the file into a docs page.

For that to work the file follows a small, strict grammar. This is the contract.

## Line types

| Line                                      | Meaning                                                                                                  |
| ----------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `[section]` / `[[section]]` (uncommented) | Section header. Always preceded by a blank line.                                                         |
| `# key = value` (col-0, one space)        | A **default**. Uncomment it (drop the leading `# `) to override. May carry an inline gloss.              |
| `# key = value  # note`                   | Inline gloss — the default's terse description.                                                          |
| `<spaces># …` (leading whitespace)        | Gloss **continuation** — wraps the inline gloss above it, aligned under its `#`.                         |
| `# # …`                                   | **Description** prose — describes the section (right after the header) or the default(s) that follow it. |
| `# ## <text>` / `# ### <text>`            | **Doc heading** — a markdown heading in the generated page. TOML- and test-invisible (it's `# #` prose). |
| `# # Example:` / `# # Example: <label>`   | Start of an **example** block.                                                                           |
| `# # …` after a marker                    | Example **body** — rendered verbatim, never TOML-parsed.                                                 |
| blank line (no `#`)                       | Hard separator between units.                                                                            |
| `# #` (empty)                             | Soft separator inside a comment block.                                                                   |

## Rules

1. **Single `#` is reserved for active config.** Only a col-0 `# key = value`
   line is a default. Everything else commentary is `# #` (or a leading-whitespace
   gloss continuation). A `# #` line is never active config.

2. **All of a section's prose lives AFTER its `[header]`, never before.** The
   header comes first; its description, then its defaults, then its examples.
   For a group of subtables (`[mouse.on-window]`, `[gestures.anywhere]`, …) the
   shared vocabulary docs go after the parent section's defaults, as a lead-in.

3. **Blank line vs `# #`:**
   - A **blank line** means you've _left_ the comment block — it separates
     sections, or groups of related defaults within a section, or closes an
     example block.
   - A **`# #` line** means you're _still inside_ one comment block — a paragraph
     break in prose, or the divider between sibling examples under one marker.

4. **Examples** are introduced by exactly one `# # Example[: label]` marker.
   Sibling examples within a block are separated by a blank `# #` line. The block
   ends at a blank line, a default, a section header, or another marker. Bodies
   are rendered as-is and are **never parsed as TOML** — they may contain partial
   snippets or repeated keys (e.g. four `type`/`path` background alternatives).

5. **Header-less doc blocks.** Arrays-of-tables with no scalar defaults
   (`[[outputs]]`, `[[window_rules]]`) have no `[section]` header — they exist
   only in examples. They are documented as a standalone `# #` prose +
   `# # Example:` block, introduced by a `# ## <title>` doc heading (since there
   is no `[section]` line for the generator to title them from), and rendered as
   their own doc sections.

6. **Doc headings.** A `# ## <text>` (or `# ### <text>`) line is a markdown
   heading emitted into the generated docs page only. It is `# #` prose, so TOML
   never sees it and the reconstruction tests skip it. Use it to title the
   header-less doc blocks; a real `[[outputs]]` / `[[window_rules]]` header can't
   serve that role because an empty one would spawn a phantom output/rule when
   the file is copied.

## Invariants

- **Reconstruction.** Take every col-0 `# key = value` line, strip the leading
  `# `, keep the uncommented `[section]` headers, drop everything else → the
  result is valid TOML and equals the compiled-in defaults. The test does exactly
  this and compares against `Config::from_toml("")`.
- **Determinism.** Default bindings are pure (`default_bindings` takes no
  environment), so `[keybindings]` / `[mouse.*]` / `[gestures.*]` reconstruct to a
  fixed map. Runtime-detected values (terminal/launcher) are actions
  (`exec-terminal` / `exec-launcher`), not baked-in commands.
- **Inherit sentinels.** A field whose real default is "unset / inherit from the
  environment" still gets a concrete, uncomment-able default by spelling that
  state explicitly and having the code normalize it back to unset: `theme = "none"`
  / `size = 0` (cursor), `click_method = "none"` (trackpad), `type = "default"`
  (background). These reconstruct to the same `Config` as omitting the field, so
  they satisfy the reconstruction invariant — don't "correct" them to a literal
  value.
