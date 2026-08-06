# frozen_string_literal: true

require "test_helper"

# Cross-implementation vectors. Every signature here is one the Rust
# server accepts in tests/server.rs — signing_gate,
# signed_urls_cover_the_format_token and signed_urls_cover_nested_paths
# assert a 200 for exactly these strings. A scheme with two
# implementations is pinned by shared vectors, not by two readings of
# the same paragraph: if this file goes red, the gem stopped speaking
# the server's scheme.
class Oximg::ServerSignerTest < Oximg::Test
  def sign(path)
    Oximg::Server::Signer.sign([KEY].pack("H*"), [SALT].pack("H*"), path)
  end

  def test_matches_the_servers_vector_for_a_bare_path
    assert_equal "t-jKRoyvzhs4dEBnGGBUS_t6Uh_HE6WysfGYvs8UaTo",
      sign("/resize/100/100/photo.jpg")
  end

  def test_matches_the_servers_vector_for_a_format_token
    assert_equal "XQ8C3eYRVAkFAnUczGBsuXMOu-J6vMoYi3W8_4-sT6Q",
      sign("/resize/100/100/photo.jpg@webp")
  end

  def test_matches_the_servers_vector_for_a_nested_path
    assert_equal "i1gy8Dm1yo32_9FMzrRj8MDG_c0F0kJDV22jAgvUCow",
      sign("/resize/100/100/albums/2026/photo.jpg")
  end

  def test_is_base64url_without_padding
    sig = sign("/resize/100/100/photo.jpg")
    assert_match(/\A[A-Za-z0-9_-]+\z/, sig)
    assert_equal 43, sig.length, "32 digest bytes, unpadded"
  end

  # The signature is what stops one URL from authorizing a heavier
  # encode; changing any covered byte must change it.
  def test_covers_every_byte_of_the_path
    base = sign("/resize/100/100/photo.jpg")
    refute_equal base, sign("/resize/101/100/photo.jpg")
    refute_equal base, sign("/resize/100/100/photo.jpg@webp")
    refute_equal base, sign("/resize/100/100/other.jpg")
  end

  # A non-ASCII filename must sign, not raise: the salt is binary and
  # the path is UTF-8, and joining those two is the one place the
  # encodings meet.
  def test_signs_a_utf8_path
    assert_match(/\A[A-Za-z0-9_-]{43}\z/, sign("/resize/100/0/相片/日本.jpg"))
  end
end
