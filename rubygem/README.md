# Ruby gems

Two gems, split the way `imgproxy` and `imgproxy-rails` are:

| Gem | What it is |
|---|---|
| [`oximg`](oximg) | The engine. Drives the oximg executable from Ruby — `Oximg.resize`, `Oximg.probe` — so an app can compress and resize without libvips or ImageMagick. Platform gems bundle the binary |
| [`oximg-rails`](oximg-rails) | The Rails-facing half. Today: `Oximg::Server`, which builds and HMAC-signs URLs for a remote oximg server. Next: the ActiveStorage glue |

Both track the oximg release they belong to, the same convention the npm
package uses.

## Layout

```
rubygem/oximg/         lib/, test/, exe/ (empty; filled at package time)
rubygem/oximg-rails/   lib/, test/
```

`rubygem/oximg/exe/` is empty in the repo. The release workflow drops
the matching binary in and builds one platform gem per released target,
then a plain-Ruby gem with an empty `exe/` for everything else — those
resolve a binary from `OXIMG_BIN` or PATH.

| Gem platform | From release target | Notes |
|---|---|---|
| `x86_64-linux-gnu` | `x86_64-unknown-linux-gnu` | glibc; spelled `-gnu` so it never installs on musl |
| `aarch64-linux-gnu` | `aarch64-unknown-linux-gnu` | glibc |
| `arm64-darwin` | `aarch64-apple-darwin` | Apple silicon |
| (plain Ruby) | — | Intel macOS, Alpine/musl, anything else: needs a binary on PATH |

Filling the musl and Intel-macOS gaps means adding those targets to the
`binaries` job first; the gem side is one more line in the platform
loop.

## Testing

```sh
cargo build                                  # the tests drive a real binary
cd rubygem/oximg      && bundle install && bundle exec rake test
cd rubygem/oximg-rails && bundle install && bundle exec rake test
```

The `oximg` suite runs its integration cases against `target/debug/oximg`
(or `target/release/oximg`, or one on PATH) and skips them when there is
none. `oximg-rails`'s signing tests need no binary: they assert the exact
signatures the Rust suite's server answers 200 for.

## Releasing

A `v*` tag publishes both gems from `.github/workflows/release.yml` via
[RubyGems Trusted Publishing](https://guides.rubygems.org/trusted-publishing/)
(OIDC — no API token in the repo), the same way crates.io and npm already
publish here. Each gem needs a trusted publisher configured once on its
rubygems.org settings page: repository `oximg/oximg`, workflow
`release.yml`, environment `release`.

0.10.1 was pushed by hand, before that was wired up.
