# oximg.dev

The project website: a static [Astro](https://astro.build) site with
the home page, `/docs` and `/benchmarks`. Quality and migration pages
are planned.

Every figure on `/benchmarks` lives in `src/data/bench.ts`, copied from
BENCH.md, README.md or QUALITY.md with a link back to its table. Update
the markdown first, then the data file.

```sh
npm install
npm run dev       # http://localhost:4321
npm run build     # static output in dist/
```

## Docs

`/docs` is [Starlight](https://starlight.astro.build), but its pages are
not written here. `scripts/sync-docs.mjs` generates them from the repo's
own markdown (`README.md` sections, `docs/`, the gem READMEs) before
every `dev`, `build` and `check`, so the README stays the single source
of truth. The output in `src/content/docs/docs/` is gitignored.

- **To change a page**, edit its source; each page's "Edit page" link
  points there.
- **To add a page**, add an entry to `PAGES` in the script and a
  sidebar item in `astro.config.mjs`.
- Relative links are rewritten: synced files become site routes, README
  anchors go to the page that now holds that heading, anything else goes
  to GitHub. Renaming a synced README heading fails the build, which is
  why the Site workflow also runs on README and `docs/` changes.

## Rules

- **Numbers come from the repo.** Every figure lives in
  `src/data/bench.ts`, copied from `BENCH.md`, `README.md` or
  `bench/quality/QUALITY.md`, with the machine, date and a link to the
  source table. Never type a number into a component.
- **Show the losses.** The benchmark section keeps a "where we don't
  lead" list; update it whenever a remeasure changes a cell.
- **No JavaScript unless it earns it.** The chart switch is CSS; the
  only script on the home page is the copy button.

## Color system: "Oxide"

Tokens are in `src/styles/tokens.css`: primitives first, then a
semantic layer. Components only use the semantic layer.

| Token | Hue | Meaning |
|---|---|---|
| `--brand` | rust `#e8622c` | oximg — its data, its CTAs, its mark |
| `--accent` | patina `#3fb6a8` | secondary: links, states, "correct" |
| `--series-theirs-*` | graphite grays | competitors in charts, always |
| `--bg` / `--surface` | graphite `#0b0d0e` / `#111416` | dark-first surfaces |

Rust is reserved for oximg, so in any chart a reader can tell whose bar
is whose without a legend. Code samples use the matching Shiki theme in
`src/styles/code-theme.ts`. Type is Inter for prose and JetBrains Mono
for code, labels and every number (`.num` sets tabular figures).
