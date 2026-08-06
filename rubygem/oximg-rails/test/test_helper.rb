# frozen_string_literal: true

require "minitest/autorun"
require "oximg/server"

module Oximg
  class Test < Minitest::Test
    # The key/salt the Rust integration suite signs with
    # (tests/server.rs), so the vectors below are the same bytes the
    # server answers 200 for.
    KEY = "deadbeef" * 8
    SALT = "cafebabe" * 8

    def setup
      # An explicit empty env: the real one would otherwise leak an
      # OXIMG_* variable from the developer's shell into every test.
      Oximg::Server.config = Oximg::Server::Config.new(env: {})
    end

    def teardown
      Oximg::Server.reset!
    end

    def config(**attrs)
      Oximg::Server::Config.new(env: {}).tap do |c|
        attrs.each { |name, value| c.public_send("#{name}=", value) }
      end
    end

    def builder(**attrs)
      Oximg::Server::UrlBuilder.new(config(**attrs))
    end
  end
end
