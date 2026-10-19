# InfinityDB website

The static website for InfinityDB: a landing page, the documentation and the
engineering blog, deployed to GitHub Pages from `site/`. The visual system
is specified in [DESIGN.md](DESIGN.md).

Plain HTML, one stylesheet and one script, no framework and no
dependencies. Pages are rendered by `build.py` (Python 3 standard library
only) and the rendered `site/` is committed, so what is reviewed is exactly
what is deployed.

## Layout

```
build.py                 renders src/ + ../docs/compat-matrix.md into site/
src/
  index.html             landing page body
  docs/<page>.html       one fragment per docs page (front-matter + HTML)
  blog/<post>.html       one fragment per post (front-matter + HTML)
  assets/site.css        the stylesheet: tokens, components, breakpoints
  assets/site.js         theme, menus, copy, filters, search, pixel field
site/                    GENERATED, committed, deployed as-is
```

`build.py` owns everything more than one page shows, defined once: the
milestone train and the current milestone (`NOW`), the docs navigation
(`DOCS_NAV`), the post list (`POSTS`), the logo and Moss, the post covers,
the figures, and the page chrome. It also writes the favicon, the RSS feed,
the sitemap, the search index and the 404 page.

## Build and preview

From the repository root:

```bash
python3 website/build.py            # render site/
python3 website/build.py --check    # exit 1 if site/ is stale
cd website/site && python3 -m http.server 8000   # http://localhost:8000
```

The output is a pure function of the inputs (no dates, seeded art), so
`--check` is a byte comparison. CI runs it on every pull request
(`website-gates` in `.github/workflows/infinity-ci.yml`) and again before
every deploy (`.github/workflows/pages.yml`). Edit `src/`, run the build,
commit both.

## Common changes

- **The current milestone moves.** Change `NOW` in `build.py`. The
  announcement bar, alpha badges, roadmap train, docs tags and "Also on the
  train" row follow. The build also checks that every internal link and
  `#fragment` resolves.
- **A docs page.** Add `src/docs/<slug>.html` and its entry in `DOCS_NAV`.
  A source without an entry, or an entry without a source, fails the
  build. Front-matter keys: `title`, `description`, `lede`, optional
  `chips` (`Available · alpha | Law L1`). Every `<h2 id>` becomes an "On
  this page" entry; `{{fig:<name>}}` inserts a figure.
- **A blog post.** Add `src/blog/<slug>.html` (front-matter: `title`, `dek`,
  `date`, `topic`, `group`, `crumb`, `kicker`, `excerpt`) and its entry in
  `POSTS`, newest first, with a cover `kind` and `seed`.
- **Code blocks.** `<pre class="code" data-label="shell">`. Lines starting
  with `$ ` or `127.0.0.1:6379&gt; ` are commands: their prompts are muted
  and the copy button copies only the commands. Other lines are output.

## The compatibility page is generated

`site/docs/compat.html` is rendered from `docs/compat-matrix.md`, which the
compat crate generates from the command registry and the corpus diffed
against Redis (with its own staleness gate). The page states each note
without citing unpublished records; a citation that survives the rewrite
fails the build. Never edit the page; regenerate the matrix and rebuild.

`bins/infinityd/tests/boot_refusal.rs` reads `site/docs/observability.html`:
the page quotes the refusal line the binary prints, and the test compares
the two. Reword one and the test goes red until the other matches.

## Content rules

- InfinityDB is alpha. Shipped work is labelled as shipped; everything else
  carries its milestone, in copy, figures and terminal examples.
- No release has been published. No page claims a version, image or
  binary that does not exist; the quickstart builds from the repository.
- No performance, latency, throughput or memory numbers. The Evidence
  section and the "no benchmarks" post explain why.
- Big milestones only (M0 to M11): no dot milestone, story, decision or
  review identifier in any page. In a combined checkout `just check` runs
  the public-docs gate over the site and its sources.
- Commands and replies are ones the current tree accepts and prints.

## Deploying

The `website` workflow deploys `site/` on every push to `main` that touches
`website/**` or the compat matrix. In the repository settings, Pages must
use **GitHub Actions** as its source. `SITE_URL` in `build.py` sets the
canonical URLs, the feed and the 404 page's links (absolute, because Pages
serves that page at any missing path). Moving to a custom domain means
changing it and having the build emit a `CNAME` file.
