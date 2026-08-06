# frozen_string_literal: true

module Oximg
  module Server
    # The client-side half of an oximg deployment's configuration: where
    # the server is, and the signing material it was started with. What
    # the server *does* with a request (quality profiles, source origin,
    # memory caps) is server-side config and is deliberately not restated
    # here — a knob duplicated on both sides is a knob that drifts.
    #
    # The defaults read the same environment variables the server itself
    # reads (+OXIMG_KEY+, +OXIMG_SALT+, +OXIMG_OPTIONS_PREFIX+), so an app
    # deployed alongside a configured oximg needs no Ruby-side setup at
    # all. +OXIMG_ENDPOINT+ is client-only: the server has no notion of
    # its own public URL.
    class Config
      # Root of the oximg deployment, without a trailing slash
      # (+https://img.example.com+). Leave it +nil+ to emit root-relative
      # URLs — the right answer when oximg is mounted on the app's own
      # origin behind a CDN.
      attr_reader :endpoint

      # Hex +OXIMG_KEY+/+OXIMG_SALT+, decoded to bytes. Both nil means
      # signing is off; exactly one set is a configuration error, checked
      # at URL-build time (the server refuses to boot on the same
      # half-configuration rather than serving unsigned).
      attr_reader :key, :salt

      # Mount point of the Cloudflare-Images-style options route, matching
      # the server's +OXIMG_OPTIONS_PREFIX+ (e.g. +/image+). Nil means the
      # route is not mounted and #options_url_for is unavailable.
      attr_reader :options_prefix

      def initialize(env: ENV)
        self.endpoint = env["OXIMG_ENDPOINT"]
        self.key = env["OXIMG_KEY"]
        self.salt = env["OXIMG_SALT"]
        self.options_prefix = env["OXIMG_OPTIONS_PREFIX"]
      end

      def endpoint=(value)
        @endpoint = blank?(value) ? nil : value.to_s.sub(%r{/+\z}, "")
      end

      def key=(value)
        @key = decode_hex("key", value)
      end

      def salt=(value)
        @salt = decode_hex("salt", value)
      end

      def options_prefix=(value)
        if blank?(value)
          @options_prefix = nil
          return
        end
        prefix = value.to_s.sub(%r{/+\z}, "")
        prefix = "/#{prefix}" unless prefix.start_with?("/")
        @options_prefix = prefix
      end

      # True when URLs will carry a signature.
      def signing?
        !@key.nil? && !@salt.nil?
      end

      # Fail closed the way the server does: a half-configured signing
      # pair must never quietly produce unsigned URLs that the server will
      # answer with 403.
      def validate!
        return if @key.nil? == @salt.nil?

        raise ConfigurationError,
          "key and salt must both be set to enable signing (got only #{@key.nil? ? "salt" : "key"})"
      end

      private

      def blank?(value)
        value.nil? || value.to_s.strip.empty?
      end

      def decode_hex(name, value)
        return nil if blank?(value)

        hex = value.to_s.strip
        unless hex.match?(/\A(?:[0-9a-fA-F]{2})+\z/)
          raise ConfigurationError, "#{name} is not valid hex (even-length 0-9a-f)"
        end

        [hex].pack("H*")
      end
    end
  end
end
