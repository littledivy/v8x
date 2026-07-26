// HTML-export shim for the v8x docs site (typst compile --features html).
// Adapted from littledivy.com's shim, which is taken from
// https://github.com/typst/typst/issues/7223#issuecomment-3446402111
//
// All hrefs are RELATIVE (no leading slash): the site is served from a
// project-pages subpath (littledivy.github.io/v8x/). Every page lives flat at
// the site root, so "getting-started" resolves correctly from any page.

/// Produce default document information needed for `default-head`. Requires
/// context.
#let get-document-info() = (
  title: document.title,
  author: document.author,
  description: document.description,
  keywords: document.keywords,
  locale: text.lang + if text.region != none { "-" + text.region },
)

/// Produces default head HTML tag based on document information.
#let default-head(info) = (..args) => {
  let head = if args.pos().len() > 0 { args.pos().first() } else { none }
  html.head(..args.named(), {
    html.meta(charset: "utf-8")
    html.meta(name: "viewport", content: "width=device-width, initial-scale=1")
    html.elem("link", attrs: (rel: "stylesheet", href: "main.css"))
    if info.title != none {
      html.title(info.title)
    }
    if info.description != none {
      html.meta(name: "description", content: info.description.text)
    }
    if info.author.len() != 0 {
      html.meta(name: "authors", content: info.author.join(", "))
    }
    if info.keywords.len() != 0 {
      html.meta(name: "keywords", content: info.keywords.join(", "))
    }
    head
  })
}

/// Produces default html HTML tag based on document information.
#let default-html(info, head: auto) = (..args) => {
  let head = if head == auto { default-head(info) } else { head }
  let body = if args.pos().len() > 0 { args.pos().first() } else { none }
  html.html(head() + html.body(body), lang: info.locale, ..args.named())
}

#let nav-bar() = {
  html.elem("nav", [
    #html.elem("a", attrs: (href: "./"), [v8x])
    #text("   ")
    #html.elem("a", attrs: (href: "getting-started"), [getting started])
    #text("   ")
    #html.elem("a", attrs: (href: "internals"), [internals])
    #text("   ")
    #html.elem("a", attrs: (href: "hill-climb"), [hill climb])
    #text("   ")
    #html.elem("a", attrs: (href: "status/"), [dashboard])
    #text("   ")
    #html.elem("a", attrs: (href: "https://github.com/littledivy/v8x"), [github])
  ])
}

#let site-footer() = html.elem("footer", [
  #html.elem("a", attrs: (href: "https://github.com/littledivy/v8x"), [github.com/littledivy/v8x])
])

/// An aside worth flagging. Usage: `#note[...]`
#let note(body) = html.elem("aside", attrs: (class: "note-card"), body)

/// A figure reused from the paper. `#fig("static/handles.svg", "alt", width: "420")`
#let fig(src, alt, width: none) = html.elem(
  "figure",
  html.elem(
    "img",
    attrs: (src: src, alt: alt, loading: "lazy")
      + (if width != none { (width: width) } else { (:) }),
  ),
)

/// Breadcrumb + position marker for the internals sub-pages.
#let crumb(n, title) = html.elem(
  "p",
  attrs: (class: "crumb"),
  [#html.elem("a", attrs: (href: "internals"), [internals]) · #n of 7 · #title],
)

/// "Next:" pointer at the bottom of a sub-page.
#let next(href, title) = html.elem(
  "p",
  attrs: (class: "next"),
  [Next: #html.elem("a", attrs: (href: href), title)],
)

#let html-shim(doc) = context {
  default-html(get-document-info())(nav-bar() + doc + site-footer())
}
