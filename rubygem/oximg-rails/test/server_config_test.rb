# frozen_string_literal: true

require "test_helper"

class Oximg::ServerConfigTest < Oximg::Test
  # The three server-side variables carry the same meaning here, so an
  # app deployed next to a configured oximg needs no Ruby-side setup.
  def test_reads_the_servers_own_environment_variables
    c = Oximg::Server::Config.new(env: {
      "OXIMG_ENDPOINT" => "https://img.example.com/",
      "OXIMG_KEY" => KEY,
      "OXIMG_SALT" => SALT,
      "OXIMG_OPTIONS_PREFIX" => "/image"
    })
    assert_equal "https://img.example.com", c.endpoint
    assert_equal "/image", c.options_prefix
    assert c.signing?
  end

  def test_defaults_to_unsigned_and_relative
    c = Oximg::Server::Config.new(env: {})
    assert_nil c.endpoint
    assert_nil c.options_prefix
    refute c.signing?
    c.validate!
  end

  def test_treats_blank_values_as_unset
    c = Oximg::Server::Config.new(env: {"OXIMG_KEY" => "", "OXIMG_SALT" => "  "})
    refute c.signing?
    c.validate!
  end

  def test_decodes_hex_key_material
    c = config(key: "cafe")
    assert_equal "\xCA\xFE".b, c.key
  end

  # Fail closed, like the server: a set-but-undecodable key is a
  # configuration error, never a silently unsigned URL.
  def test_rejects_non_hex_key_material
    assert_raises(Oximg::Server::ConfigurationError) { config(key: "not-hex-at-all") }
    assert_raises(Oximg::Server::ConfigurationError) { config(salt: "abc") }
  end

  # Same half-configuration the server refuses to boot on.
  def test_rejects_a_half_configured_signing_pair
    assert_raises(Oximg::Server::ConfigurationError) { config(key: KEY).validate! }
    assert_raises(Oximg::Server::ConfigurationError) { config(salt: SALT).validate! }
    error = assert_raises(Oximg::Server::ConfigurationError) do
      builder(key: KEY).resize("a.jpg", width: 100)
    end
    assert_match(/both be set/, error.message)
  end
end
