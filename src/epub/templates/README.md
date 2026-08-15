# EPUB templates

Askama templates and stylesheets for the two editions (spec §3.10, implementation
notes §11). `src/epub/build.rs` owns the structs they bind to; `askama.toml` at
the crate root points askama here (`dirs = ["src/epub/templates"]`).

| File | Template struct | Purpose |
|---|---|---|
| `base.xhtml` | — | Shared XHTML skeleton (`{% block body_class %}`, `{% block content %}`) |
| `cover_page.xhtml` | `CoverPage` | Page that displays the rasterized cover image |
| `front_page.xhtml` | `FrontPage` | "From the Editor" + issue stats line |
| `in_this_issue.xhtml` | `InThisIssue` | Introduction chapter: per-section linked index |
| `section.xhtml` | `SectionPage` | Section title page + LLM intro |
| `chapter.xhtml` | `ArticleChapter` | Article: header, body, rating/read-online footer |
| `discussion.xhtml` | `DiscussionChapter` | Comment chapter (§3.7); body from `comments::render_xhtml` |
| `world_briefing.xhtml` | `WorldBriefingChapter` | Wikipedia Current Events (§3.8), body from `world::render_xhtml` |
| `colophon.xhtml` | `ColophonChapter` | Back matter: models, cost, counts |
| `cover.svg` | `CoverSvg` | Typographic cover, rasterized with resvg + tiny-skia |
| `style.css` | — | Standard-edition stylesheet, embedded as `stylesheet.css` |
| `style-x4.css` | — | X4 stylesheet: no floats/flex/grid, no fonts, hyphenation on |

Conventions:

- Every content template `{% extends "base.xhtml" %}` and provides `title`.
- Templates declare `escape = "html"` — `.xhtml`/`.svg` are not in askama's
  default escaper extension list. Escaping emits numeric character references
  (`&#38;`), which are valid XML; only pre-sanitized markup uses `|safe`.
- Markup that reaches `|safe` has gone through `ammonia` **and**
  `epub::images::to_xhtml` (void elements self-closed, `&nbsp;` → `&#160;`) so
  the output parses as XML, as EPUB3 content documents must.
- Entities other than the five XML built-ins are written as numeric references
  in the templates themselves (`&#183;`, `&#128077;`, …).
