# frozen_string_literal: true

require "oximg"

require_relative "server/config"
require_relative "server/signer"
require_relative "server/url_builder"

module Oximg
  # The remote half of oximg: building — and HMAC-signing — URLs for an
  # oximg **server**, for deployments that let the server do the work
  # instead of processing locally with +Oximg.resize+.
  #
  #   Oximg::Server.configure do |config|
  #     config.endpoint = "https://img.example.com"
  #     config.key      = ENV["OXIMG_KEY"]
  #     config.salt     = ENV["OXIMG_SALT"]
  #   end
  #
  #   Oximg::Server.url_for("photos/a.jpg", width: 750)
  #   #=> "https://img.example.com/{sig}/resize/750/0/photos/a.jpg"
  #
  # Nothing here talks to the server: a URL is arithmetic over the
  # config, so a view can emit thousands of them without leaving the
  # process.
  module Server
    # Raised for a configuration that cannot produce a URL the server
    # would accept — a half-set signing pair, or the options route used
    # without its prefix.
    class ConfigurationError < Oximg::Error; end

    class << self
      # The process-wide configuration, seeded from the environment
      # (see Config). Set it up once at boot; it is read, not written,
      # per URL.
      def config
        @config ||= Config.new
      end

      attr_writer :config

      def configure
        yield config
        config
      end

      # Drops the configuration back to the environment defaults.
      # Exists for tests — an app should configure once at boot.
      def reset!
        @config = Config.new
      end

      # The positional route: see UrlBuilder#resize.
      def url_for(source, **options)
        UrlBuilder.new(config).resize(source, **options)
      end

      # The Cloudflare-Images-style options route: see
      # UrlBuilder#options.
      def options_url_for(source, **options)
        UrlBuilder.new(config).options(source, **options)
      end
    end
  end
end
