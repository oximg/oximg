# frozen_string_literal: true

require "test_helper"

class Oximg::ServerUrlBuilderTest < Oximg::Test
  def signed
    builder(endpoint: "https://img.example.com", key: KEY, salt: SALT)
  end

  def unsigned
    builder(endpoint: "https://img.example.com")
  end

  def test_builds_the_positional_route
    assert_equal "https://img.example.com/resize/750/500/photos/a.jpg",
      unsigned.resize("photos/a.jpg", width: 750, height: 500)
  end

  # A width-only URL is what srcset "w" descriptors and Next.js loaders
  # emit; the zero axis is the server's own convention for it.
  def test_leaves_an_omitted_axis_unconstrained
    assert_equal "https://img.example.com/resize/750/0/photos/a.jpg",
      unsigned.resize("photos/a.jpg", width: 750)
    assert_equal "https://img.example.com/resize/0/500/photos/a.jpg",
      unsigned.resize("photos/a.jpg", height: 500)
  end

  def test_appends_the_format_token
    assert_equal "https://img.example.com/resize/750/0/photos/a.jpg@webp",
      unsigned.resize("photos/a.jpg", width: 750, format: :webp)
  end

  # The signature is a segment of its own, before the route.
  def test_signs_the_positional_route
    assert_equal "https://img.example.com/t-jKRoyvzhs4dEBnGGBUS_t6Uh_HE6WysfGYvs8UaTo/resize/100/100/photo.jpg",
      signed.resize("photo.jpg", width: 100, height: 100)
  end

  def test_signs_the_format_token_too
    assert_equal "https://img.example.com/XQ8C3eYRVAkFAnUczGBsuXMOu-J6vMoYi3W8_4-sT6Q/resize/100/100/photo.jpg@webp",
      signed.resize("photo.jpg", width: 100, height: 100, format: :webp)
  end

  def test_signs_nested_paths
    assert_equal "https://img.example.com/i1gy8Dm1yo32_9FMzrRj8MDG_c0F0kJDV22jAgvUCow/resize/100/100/albums/2026/photo.jpg",
      signed.resize("albums/2026/photo.jpg", width: 100, height: 100)
  end

  def test_omits_the_endpoint_when_unset
    assert_equal "/resize/750/0/photos/a.jpg",
      builder.resize("photos/a.jpg", width: 750)
  end

  def test_strips_a_trailing_slash_from_the_endpoint
    assert_equal "https://img.example.com/resize/750/0/a.jpg",
      builder(endpoint: "https://img.example.com/").resize("a.jpg", width: 750)
  end

  def test_accepts_a_leading_slash_on_the_source
    assert_equal "https://img.example.com/resize/750/0/photos/a.jpg",
      unsigned.resize("/photos/a.jpg", width: 750)
  end

  # Escaping is URL-side only: the signature covers the decoded path, so
  # an escaped URL and its decoded spelling carry the same signature.
  def test_escapes_the_source_path_but_signs_the_decoded_form
    url = signed.resize("holiday photos/a b.jpg", width: 100, height: 100)
    sig = Oximg::Server::Signer.sign([KEY].pack("H*"), [SALT].pack("H*"),
      "/resize/100/100/holiday photos/a b.jpg")
    assert_equal "https://img.example.com/#{sig}/resize/100/100/holiday%20photos/a%20b.jpg", url
  end

  def test_escapes_characters_that_would_change_the_url_shape
    assert_equal "https://img.example.com/resize/100/0/a%3Fb%23c%25d.jpg",
      unsigned.resize("a?b#c%d.jpg", width: 100)
  end

  def test_escapes_non_ascii_as_utf8_octets
    assert_equal "https://img.example.com/resize/100/0/%E6%97%A5.jpg",
      unsigned.resize("日.jpg", width: 100)
  end

  def test_rejects_dimensions_the_server_would_refuse
    assert_raises(ArgumentError) { unsigned.resize("a.jpg") }
    assert_raises(ArgumentError) { unsigned.resize("a.jpg", width: 0, height: 0) }
    assert_raises(ArgumentError) { unsigned.resize("a.jpg", width: 8193) }
    assert_raises(ArgumentError) { unsigned.resize("a.jpg", width: -1) }
    assert_raises(ArgumentError) { unsigned.resize("a.jpg", width: "750") }
  end

  def test_rejects_unknown_and_reserved_formats
    assert_raises(ArgumentError) { unsigned.resize("a.jpg", width: 100, format: :gif) }
    # Reserved by the server for a future encoder: a 400, not an image.
    assert_raises(ArgumentError) { unsigned.resize("a.jpg", width: 100, format: :jxl) }
  end

  def test_rejects_traversal_and_empty_sources
    assert_raises(ArgumentError) { unsigned.resize("../secrets.jpg", width: 100) }
    assert_raises(ArgumentError) { unsigned.resize("a/./b.jpg", width: 100) }
    assert_raises(ArgumentError) { unsigned.resize("", width: 100) }
  end

  def test_builds_the_options_route
    b = builder(endpoint: "https://img.example.com", options_prefix: "/image")
    assert_equal "https://img.example.com/image/width=750,quality=80/photos/a.png",
      b.options("photos/a.png", width: 750, quality: 80)
  end

  # Fixed emission order: the signature covers the list verbatim, so a
  # reordered list is a different URL and a second cache entry.
  def test_emits_options_in_a_fixed_order
    b = builder(endpoint: "https://img.example.com", options_prefix: "/image")
    assert_equal "https://img.example.com/image/width=750,height=500,quality=80,format=webp/a.png",
      b.options("a.png", quality: 80, format: :webp, height: 500, width: 750)
  end

  # The signature segment precedes the prefix — the server's route is
  # "/{sig}{prefix}/{options}/{file}".
  def test_signs_the_options_route_before_the_prefix
    b = builder(endpoint: "https://img.example.com", options_prefix: "/image",
      key: KEY, salt: SALT)
    sig = Oximg::Server::Signer.sign([KEY].pack("H*"), [SALT].pack("H*"), "/image/width=750/a.png")
    assert_equal "https://img.example.com/#{sig}/image/width=750/a.png",
      b.options("a.png", width: 750)
  end

  def test_normalizes_the_options_prefix
    b = builder(endpoint: "https://img.example.com", options_prefix: "image/")
    assert_equal "https://img.example.com/image/width=750/a.png", b.options("a.png", width: 750)
  end

  def test_options_route_requires_a_prefix
    assert_raises(Oximg::Server::ConfigurationError) do
      unsigned.options("a.png", width: 750)
    end
  end

  def test_options_route_validates_its_own_grammar
    b = builder(options_prefix: "/image")
    assert_raises(ArgumentError) { b.options("a.png") }
    assert_raises(ArgumentError) { b.options("a.png", width: 0) }
    assert_raises(ArgumentError) { b.options("a.png", width: 750, quality: 0) }
    assert_raises(ArgumentError) { b.options("a.png", width: 750, quality: 101) }
    assert_raises(ArgumentError) { b.options("a.png", width: 750, format: :gif) }
    # "auto" is the options route's own token: negotiate, else source.
    assert_equal "/image/width=750,format=auto/a.png", b.options("a.png", width: 750, format: :auto)
  end

  def test_module_level_helpers_use_the_global_config
    Oximg::Server.configure do |c|
      c.endpoint = "https://img.example.com"
      c.options_prefix = "/image"
    end
    assert_equal "https://img.example.com/resize/750/0/a.jpg", Oximg::Server.url_for("a.jpg", width: 750)
    assert_equal "https://img.example.com/image/width=750/a.jpg",
      Oximg::Server.options_url_for("a.jpg", width: 750)
  end
end
