# oximg (Ruby)

Ruby bindings for [oximg](https://github.com/oximg/oximg) — image
compression and resizing in Rust. JPEG, PNG, WebP and AVIF, resized in
linear light at measurably higher output quality than the usual stack
(see the [benchmarks](https://github.com/oximg/oximg/blob/main/BENCH.md)).

The point of the gem: **no libvips, no ImageMagick**. Platform gems
bundle the executable, so `bundle install` is the whole installation —
no `apt-get install libvips42`, no ImageMagick policy file, no native
extension compiled against a system library that the next base-image
bump moves.

```ruby
gem "oximg"
```

```ruby
Oximg.resize("in.jpg", "out.jpg", width: 750)
Oximg.resize("in.jpg", "out.webp", width: 750, height: 500, quality: 80)
Oximg.probe("in.jpg")
#=> {content_type: "image/jpeg", format: :jpeg, width: 4000, height: 3000}
```

A GIF source is read and probed like the others (`format: :gif`), but
nothing encodes GIF: it leaves as the destination's format, WebP when
the name does not say, and `:gif` is not a `format:` you can ask for.

## Resizing

```ruby
Oximg.resize(source, destination, width: 0, height: 0,
             quality: nil, format: nil, preset: nil)
```

The source is fitted within `width` x `height` and **never enlarged**. A
zero axis is unconstrained, so `width: 750` alone is width-only — what
an `srcset` `w` descriptor means — and the default `0 x 0` re-encodes at
the source's own size, which is how you ask for compression without a
resize:

```ruby
Oximg.resize("photo.jpg", "smaller.jpg", quality: 70)
```

| Option | Meaning |
|---|---|
| `width` / `height` | Non-negative Integers; `0` is an unconstrained axis |
| `quality` | 1–100, default 80. JPEG quality |
| `format` | `:jpg`, `:jpeg`, `:png`, `:webp`, `:avif`. Defaults to the destination's extension, else the source's own format |
| `preset` | `:jpegli` (default — maximum quality per byte), `:fast`, `:small` |

`resize` returns the destination path and raises `Oximg::ProcessingError`
on failure, carrying the binary's own message and exit status. Paths are
expanded before they reach the CLI, so an uploaded file called `-q.jpg`
cannot be read as a flag.

Everything the server tunes through `OXIMG_*` environment variables
applies here too, and is validated the same fail-closed way: a typo'd
knob is a startup error, never a silent default.

## Probing

```ruby
Oximg.probe("in.jpg")                          # a path
Oximg.probe(Pathname("in.jpg"))                # also a path
Oximg.probe(upload.tempfile)                   # anything with #path
Oximg.probe(StringIO.new(bytes))               # an IO: bytes in memory
#=> {content_type: "image/jpeg", format: :jpeg, width: 4000, height: 3000}
```

`probe` reads the header only and decodes no pixels. The dimensions are
the stored ones, before any EXIF rotation. A String is always a path,
so bytes already in memory go in as a `StringIO`. A `File` is read by
its name, its position untouched. Other IO — a `StringIO`, a pipe, a
socket — is read from its current position through `#read(n)` and
left where the read stopped.

Platform gems run `probe` in-process: oximg's own probe, compiled into
a small library the gem calls through [`ffi`](https://github.com/ffi/ffi).
It reads only as much of the source as the header takes, typically the
first 64 KiB, and costs microseconds where a spawn costs milliseconds.
`resize` is not affected and still runs in a child process, which keeps
a full decode's memory and crashes out of your process.

Without the library — the plain-Ruby gem, or a platform gem whose
library cannot load — `probe` spawns `oximg probe` instead, copying an
IO to a tempfile first. The answer is the same; only the cost differs.

```ruby
Oximg.in_process_probe?  #=> true on a platform gem
```

`OXIMG_PROBE_LIB` points at a library explicitly; like `OXIMG_BIN`, a
wrong one raises rather than falling back.

## Where the binary comes from

`Oximg.executable` resolves, first hit wins:

1. `Oximg.executable = "/path/to/oximg"`, or `OXIMG_BIN` — an explicit
   path always wins, and a wrong one raises instead of falling through
   to something else.
2. The binary bundled in this gem (platform gems ship one).
3. `oximg` on PATH — Homebrew, `cargo install oximg`, or a Docker image.

```ruby
Oximg.available?  #=> true
Oximg.executable  #=> "/…/gems/oximg-0.17.0-arm64-darwin/exe/oximg"
Oximg.version     #=> "0.17.0"   # the binary's version, not the gem's
```

`resize` shells out to that binary — one process per image, which is
noise next to the encode itself and invisible inside the background job
where variants are generated anyway. Only `probe`, where the spawn was
the whole cost, runs in-process (see [Probing](#probing)).

## Rails

Rails and ActiveStorage integration — and URL building for a remote
oximg **server**, if you run one rather than processing locally — live
in the separate [`oximg-rails`](../oximg-rails) gem, mirroring how
`imgproxy` and `imgproxy-rails` are split.

## Versioning

The gem version tracks the oximg release it bundles, the same
convention the npm package uses. `Oximg.version` reports what the
resolved binary actually is, which can differ when it comes from PATH.

## License

Apache-2.0, same as oximg. See [LICENSE](LICENSE).
