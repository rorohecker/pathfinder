# Translations (i18n)

Pathfinder uses **Slint’s built-in `@tr()` + bundled gettext `.po` files**, not a custom key map.

## Why this approach

| Approach | Fit for Pathfinder |
| --- | --- |
| Opaque keys (`t("settings.title")`) | Extra indirection; English source is harder to read in `.slint` |
| Slint `@tr("Settings")` + `.po` | Native to Slint; English stays readable; gettext tooling works |

English source text in the UI is the message id. Translators edit `msgstr` in each language file.

## Layout

```
lang/
  <lang>/LC_MESSAGES/pathfinder.po
```

`<lang>` is the locale folder name passed to `slint::select_bundled_translation` (`it`, `es`, …).
The domain file must be named after the Cargo package: `pathfinder.po`.

Bundling is configured in `build.rs` via `with_bundled_translations("lang")`.
Default component `msgctxt` is disabled so English source text alone is the lookup key.

## Workflow for a new language

1. Mark every user-visible string in `.slint` with `@tr("...")` (plurals/context supported).
2. Extract / refresh the template:

   ```bash
   slint-tr-extractor -d pathfinder -o lang/pathfinder.pot ui/**/*.slint
   ```

3. Create `lang/<lang>/LC_MESSAGES/pathfinder.po` from the pot (copy or `msginit`).
4. Translate `msgstr` entries.
5. Rebuild. The language appears in **Settings → Appearance → Language**.

## What still needs work (phase 2)

Many labels are **pushed from Rust** (sidebar, command palette, toasts, choice chip copy). Those are not covered by `@tr()` alone. Prefer:

- Keep English as the canonical id.
- Translate via `src/i18n.rs` (same English keys), then refresh models when the language changes.

## Ambiguous English

Same English in different meanings needs an explicit context:

```slint
@tr("settings-tab" => "View")
@tr("preview-mode" => "View")
```

That becomes `msgctxt` in the `.po` file.

## Literal braces

`@tr` treats `{...}` as format placeholders. Escape literal tokens with doubled braces:

```slint
@tr("Tokens: {{n}} {{name}}")
```


## Language setting

`settings.json` field: `ui_language`

| Value | Behavior |
| --- | --- |
| `system` (default) | Best-effort OS/locale detection, else English |
| `en` | English (source strings) |
| `it` | Italian |
| `es` | Spanish |
