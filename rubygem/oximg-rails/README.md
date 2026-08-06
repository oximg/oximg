# oximg-rails

The Rails-facing half of [oximg](https://github.com/oximg/oximg). The
split mirrors `imgproxy` / `imgproxy-rails`: the
[`oximg`](../oximg) gem does local image processing and stays
framework-free, and everything that points a Rails app at an oximg
**server** — or wires oximg into ActiveStorage — lives here.

```ruby
gem "oximg-rails"
```

## Remote mode: URLs for an oximg server

If you run oximg as a service, the app does not process images at all;
it emits signed URLs and the server does the work.

```ruby
Oximg::Server.configure do |config|
  config.endpoint = "https://img.example.com"
  config.key      = ENV["OXIMG_KEY"]   # hex, as the server takes it
  config.salt     = ENV["OXIMG_SALT"]  # hex
end

Oximg::Server.url_for("photos/a.jpg", width: 750)
#=> "https://img.example.com/YkUw…/resize/750/0/photos/a.jpg"

Oximg::Server.url_for("photos/a.jpg", width: 750, height: 500, format: :webp)
#=> ".../resize/750/500/photos/a.jpg@webp"
```

The source is fitted within the box and never enlarged. A zero axis is
unconstrained, so `width: 750` alone is exactly what an `srcset` `w`
descriptor and a Next.js loader mean.

Every setting defaults from the environment, and three of the four
variables are the ones the server itself reads — so an app deployed
alongside a configured oximg needs no Ruby-side setup at all:

| Variable | Meaning |
|---|---|
| `OXIMG_ENDPOINT` | Root of the deployment. Client-only: the server has no notion of its own public URL. Unset emits root-relative URLs, which is the right answer when oximg is mounted on the app's own origin behind a CDN |
| `OXIMG_KEY` / `OXIMG_SALT` | Hex HMAC material. Both set means URLs are signed; both unset means they are not; **exactly one set raises** — the same half-configuration the server refuses to boot on |
| `OXIMG_OPTIONS_PREFIX` | Mount point of the options route, if the server has one |

### The options route

If the server mounts the Cloudflare-Images-compatible route
(`OXIMG_OPTIONS_PREFIX`), `options_url_for` speaks its grammar — this is
the route that carries per-request `quality`:

```ruby
Oximg::Server.options_url_for("photos/a.png", width: 750, quality: 80)
#=> "https://img.example.com/…/image/width=750,quality=80/photos/a.png"
```

Options are emitted in a fixed order (`width`, `height`, `quality`,
`format`). The signature covers the list verbatim, so a stable order is
also a stable cache key.

### Signing

The scheme is imgproxy's, and it is the server's:

```
base64url(HMAC-SHA256(key, salt || path))
```

over the **percent-decoded** path — so one signature covers every URL
encoding of the same source, and the gem escapes the URL after signing
it. The signature covers the format token too: a URL for `photo.jpg`
does not authorize `photo.jpg@avif` and its heavier encode.

`test/server_signer_test.rb` pins this against the exact vectors the
Rust integration suite serves 200s for. A scheme with two
implementations is kept honest by shared vectors, not by two readings of
the same paragraph.

## Local mode: ActiveStorage

**Not implemented yet.** Planned, on top of the `oximg` gem:

- **A variant processor.** `config.active_storage.variant_processor`
  resolves `ImageProcessing::<Name>`, so an `ImageProcessing::Oximg`
  backend can plug in beside `:vips` and `:mini_magick` without a
  monkeypatch — and a Rails app can drop libvips/ImageMagick entirely.
- **Attachment URL helpers** — `oximg_url` on an attachment, resolving a
  blob's key into an oximg source path, so remote mode needs no manual
  path plumbing.

Until then, `Oximg.resize` from the `oximg` gem works fine inside a job.

## Versioning

The gem version tracks the oximg release whose URL grammar it speaks,
the same convention as the `oximg` gem and the npm package.

## License

Apache-2.0, same as oximg. See [LICENSE](LICENSE).
