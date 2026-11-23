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
Oximg.strip("upload.jpg", "clean.jpg")
Oximg.probe("in.jpg")
#=> {content_type: "image/jpeg", format: :jpeg, width: 4000, height: 3000,
#    orientation: 1, display_width: 4000, display_height: 3000}
```

A GIF source is read and probed like the others (`format: :gif`), but
nothing encodes GIF: it leaves as the destination's format, WebP when
the name does not say, and `:gif` is not a `format:` you can ask for.

## Resizing

```ruby
Oximg.resize(source, destination, width: 0, height: 0,
             quality: nil, format: nil, preset: nil,
             icc: nil, auto_rotate: nil, mode: nil)
```

The source is fitted within `width` x `height` and **never enlarged**. A
zero axis is unconstrained, so `width: 750` alone is width-only — what
an `srcset` `w` descriptor means — and the default `0 x 0` re-encodes at
the source's own size, which is how you ask for compression without a
resize. `mode: :crop` covers the box and crops the centre to its shape,
and `mode: :outside` covers it and keeps the whole image; both need
`width` and `height`:

```ruby
Oximg.resize("photo.jpg", "smaller.jpg", quality: 70)
```

| Option | Meaning |
|---|---|
| `width` / `height` | Non-negative Integers; `0` is an unconstrained axis |
| `quality` | 1–100, default 80. JPEG quality |
| `format` | `:jpg`, `:jpeg`, `:png`, `:webp`, `:avif`. Defaults to the destination's extension, else the source's own format |
| `preset` | `:jpegli` (default — maximum quality per byte), `:fast`, `:small` |
| `icc` | `:keep` or `:strip` the source's colour profile. Default: `OXIMG_ICC`, else keep |
| `auto_rotate` | `false` keeps the stored orientation, `true` applies the EXIF one. Default: `OXIMG_AUTO_ROTATE`, else true |

`icc` and `auto_rotate` apply to that call only, and win over the
environment, so a threaded job runner can treat one image differently
without touching `ENV`.

`resize` returns the destination path and raises `Oximg::ProcessingError`
on failure, carrying the binary's own message and exit status. Paths are
expanded before they reach the CLI, so an uploaded file called `-q.jpg`
cannot be read as a flag.

## Stripping metadata

```ruby
Oximg.strip(source, destination, icc: nil, auto_rotate: nil, env: nil)
```

Removes EXIF (GPS, camera, timestamps), XMP, IPTC and comments from a
JPEG, PNG or WebP **without re-encoding**. Decoded pixels are identical,
and it costs a fraction of a `0 x 0` resize in time and memory. This
replaces `saver(strip: true)` (libvips) or `-strip` (ImageMagick). A
rotated photo keeps an EXIF block holding only its orientation, so it
still displays upright. `icc: :strip` also drops the colour profile.
The default is `:keep`, or `OXIMG_ICC`. Dropping a CMYK JPEG's profile
raises `Oximg::ProcessingError`, as do GIF and AVIF sources.

`auto_rotate: true` turns a rotated JPEG upright instead, still without
re-encoding: its compressed 8x8 blocks are moved, as `jpegtran
-perfect` does, so the stored and displayed sizes agree for every
reader, not only for ones that honor EXIF. That is only exact when the
edges it moves are whole 8 or 16 px blocks (a phone's 4032x3024 is);
otherwise, and for PNG and WebP, the file is stripped as without it,
orientation kept. It costs a full entropy decode and encode, about
170 ms for a 12 MP photo.

`strip` returns an `Oximg::Stripped`, the binary's own account of the
run, so telling which happened needs no second read:

```ruby
result = Oximg.strip(upload, clean, icc: :keep, auto_rotate: true)
result.upright?             # no orientation tag left: stored size is displayed size
result.rotated?             # the pixels were turned upright
result.orientation          #=> 6, the tag kept, or nil
result.not_rotated&.code    #=> :unaligned_edges, :corrupt, :over_limit or :not_jpeg
result.not_rotated&.message # for humans; the wording may change
```

`not_rotated` is nil unless `auto_rotate: true` left a rotated source
tagged. The code is the contract (the table is in
[docs/features/paths.md](../../docs/features/paths.md)), so count
fallbacks by it. The result also stands in for the destination path
`strip` used to return: `to_s`, `to_path` and `to_str` are the path and
it compares equal to that String, so `File.size(Oximg.strip(...))`
still works. With an `oximg` older than 0.20, which has no `strip
--json`, the gem reads the summary line that binary prints instead:
`orientation` and `rotated?` are still right, `removed` is nil, and
the code is `:unknown` beside the binary's own reason.

## Knobs, per process or per call

Everything the server tunes through `OXIMG_*` environment variables
applies here too, and is validated the same fail-closed way: a typo'd
knob is a startup error, never a silent default.

To set one for a single call, pass `env:` to `resize` or `strip`:

```ruby
Oximg.resize("in.jpg", "out.jpg", width: 750, env: {"OXIMG_JPEG_PROGRESSIVE" => "0"})
```

Each call spawns its own process, and `env:` reaches that process
only: `ENV` is never touched, so concurrent calls in a threaded job
runner each get their own settings. Names must be in `Oximg::KNOBS`
(the pipeline knobs; server settings such as `OXIMG_KEY` are refused),
values are Strings or Integers, and `nil` unsets a name for that call.
An unknown name raises `ArgumentError` before anything runs; a bad
value is refused by the binary (`ProcessingError`, status 2).

## Probing

```ruby
Oximg.probe("in.jpg")                          # a path
Oximg.probe(Pathname("in.jpg"))                # also a path
Oximg.probe(upload.tempfile)                   # anything with #path
Oximg.probe(File.open("in.jpg", "rb"))         # an open File
Oximg.probe(StringIO.new(bytes))               # an IO: bytes in memory
#=> {content_type: "image/jpeg", format: :jpeg, width: 4000, height: 3000,
#    orientation: 1, display_width: 4000, display_height: 3000}
```

`probe` reads the header only and decodes no pixels. `width` and
`height` are the stored ones, before any EXIF rotation. `orientation`
is the EXIF value, 1..8 (1 when there is none or it is invalid, as
`resize` reads it), and `display_width` / `display_height` are what a
viewer honoring it shows: the axes exchanged for 5..8. A portrait
phone photo stored 4000x3000 with orientation 6, as `strip` leaves it,
lays out at 3000x4000. A WebP keeps its EXIF after the image data, so
probing one that has it reads the whole source. An `oximg` executable
older than this gem reports every source as orientation 1.

### Animation summary

```ruby
Oximg.probe("loop.gif", animation: true)
#=> {content_type: "image/gif", format: :gif, width: 1521, height: 627,
#    orientation: 1, display_width: 1521, display_height: 627,
#    animation: {frames: 45, duration_ms: 9000, loop_count: 0, decode_work: 42_915_015},
#    complete: true}
```

`animation: true` adds what the oximg server's animation budgets are
spent against, so an app can decide per image, before it builds a URL,
whether a GIF will come back animated or as its first frame (a server
over budget answers 200 with the first frame, which a CDN then caches).

- `frames` is compared with `OXIMG_MAX_ANIM_FRAMES`.
- `decode_work` is compared with `OXIMG_MAX_ANIM_DECODE_WORK`: the sum,
  over every source frame, of the larger of the canvas area and that
  frame's own rectangle. A frame may be larger than the canvas, so this
  is not always `frames * width * height`. It is `nil` for an animated
  WebP, which that budget does not price.
- `duration_ms` and `loop_count` (0 means forever) are what oximg would
  emit: very short GIF delays are normalized, and a GIF asking for 3
  repeats reports 4 plays.
- The encode budget, `OXIMG_MAX_ANIM_WORK`, depends on the output size,
  so it is yours to compute: `frames.fdiv(step).ceil * output_width *
  output_height`, where `step` is the server's `OXIMG_ANIM_FRAME_STEP`
  (1 unless set: every frame) and the output size is the display size
  fitted within the requested box, never enlarged.

`animation:` is `nil` for a source that is not animated: a still, a GIF
with one frame, an AVIF image sequence. Without the option the key is
absent and `probe` is the header read described above.

`complete:` says whether the source arrived whole, so an app can reject
a damaged upload before it builds a URL. It is `false` for a GIF that
ends before its trailer byte and for a WebP shorter than its RIFF
header declares, animated or not. That matters most for the commonest
damaged upload, a GIF cut inside its first frame: it has
`animation: nil`, reads as a healthy still, and the server answers it
with a 422. It is `nil` for JPEG, PNG and AVIF, which are not checked,
and when `probe` spawns an `oximg` executable of 0.22, which does not
report it. It is structural, not a decode: `true` does not promise the
pixels decode. Without the option the key is absent.

It is opt-in because it costs more. Counting a GIF's frames means
walking every block to its end, so the whole source is read and held in
memory for the call (no pixel is decoded), and a stream is read to its
end. The frames counted are the ones present: a GIF cut off mid-stream
reports those that arrived, with `complete: false`, and does not raise. Without the probe
library this needs an `oximg` executable of 0.22 or later; an older one
raises `ProcessingError` naming the version it found.

### Sources

A String is always a path, so bytes already in memory go in as a `StringIO`. An open `File` is
read from its start, its position untouched, and it is the file you
hold that is read, even if its name has since been replaced, renamed,
or was relative to a directory the process has left. A closed or
write-only `File`, which cannot be read through, is read by its name.
Other IO — a `StringIO`, a pipe, a socket — is read from its current
position through `#read(n)` and left where the read stopped.

The gem does that reading itself, in Ruby, between calls into the
library, so `#read` behaves as it would anywhere: whatever it raises
comes out of `probe` unchanged, and a `Timeout.timeout` or a
`Thread#kill` lands in it as usual.

With the probe library, `probe` runs in-process: oximg's own probe,
compiled into a small library (`ext/`) the gem calls through
[`ffi`](https://github.com/ffi/ffi). It reads only as much of the source
as the header takes, typically the first 64 KiB, and costs microseconds
where a spawn costs milliseconds. `resize` is not affected and still
runs in a child process, which keeps a full decode's memory and crashes
out of your process.

The platform gems bundle the library beside the executable (x86_64 and
aarch64 Linux, glibc and musl, and arm64 macOS), so they also depend
on `ffi`; beyond that it needs only libc and libgcc_s, which Ruby
itself needs, and not libstdc++.
`OXIMG_PROBE_LIB` points at another one; like `OXIMG_BIN`, one that
does not load raises rather than falling back. To build it from this
checkout, run `rake compile` here: it leaves the library under the
repo's `target/oximg-probe/release/`.

Without the library — the plain-Ruby gem, or a bundled one that cannot
load — `probe` spawns `oximg probe` instead, copying an IO to a
tempfile first. The answer is the same; only the cost differs.

```ruby
Oximg.in_process_probe?  #=> true on a platform gem
```

## Where the binary comes from

`Oximg.executable` resolves, first hit wins:

1. `Oximg.executable = "/path/to/oximg"`, or `OXIMG_BIN` — an explicit
   path always wins, and a wrong one raises instead of falling through
   to something else.
2. The binary bundled in this gem (platform gems ship one).
3. `oximg` on PATH — Homebrew, `cargo install oximg`, or a Docker image.

```ruby
Oximg.available?  #=> true
Oximg.executable  #=> "/…/gems/oximg-0.23.0-arm64-darwin/exe/oximg"
Oximg.version     #=> "0.23.0"   # the binary's version, not the gem's
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
