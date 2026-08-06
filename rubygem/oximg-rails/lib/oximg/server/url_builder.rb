# frozen_string_literal: true

module Oximg
  module Server
    # Builds URLs for the two routes an oximg server mounts. Nothing here
    # talks to the server: a URL is pure arithmetic over the config, so a
    # Rails view can emit thousands of them without leaving the process.
    #
    # Validation mirrors the server's own bounds and fails closed on the
    # same inputs. That is not defence — the server validates regardless —
    # it is so a typo surfaces at the call site with a name, instead of as
    # a 400 in an <img> tag nobody is watching.
    class UrlBuilder
      # The server's cap on either axis (src/main.rs); 0 means
      # "unconstrained" on the positional route only.
      MAX_DIMENSION = 8192

      # Tokens the positional route's @fmt suffix accepts. "jxl" is
      # reserved by the server for a future encoder and answers 400, so it
      # is not offered here.
      FORMATS = %w[jpg jpeg png webp avif].freeze

      # The options route additionally understands format=auto, which
      # means "negotiate, else the source's own format".
      OPTION_FORMATS = (FORMATS + %w[auto]).freeze

      # Everything outside RFC 3986's unreserved set gets percent-encoded.
      # "/" stays literal so S3-style prefixes remain readable, and "@" so
      # the format token survives; both are legal in a path segment, and
      # the server signs the decoded form either way.
      UNSAFE = %r{[^A-Za-z0-9\-._~/@]}

      def initialize(config)
        @config = config
      end

      # The positional route: +/resize/{w}/{h}/{file}+, where the source
      # is fitted within the box and never enlarged. A zero axis is
      # unconstrained — <tt>width: 750</tt> alone is exactly what an
      # +srcset+ +w+ descriptor means.
      #
      #   builder.resize("photos/a.jpg", width: 750)
      #   builder.resize("photos/a.jpg", width: 750, height: 500, format: :webp)
      def resize(source, width: 0, height: 0, format: nil)
        w = dimension(width, "width")
        h = dimension(height, "height")
        if w.zero? && h.zero?
          raise ArgumentError, "at least one of width/height must be non-zero"
        end

        file = source_path(source)
        file = "#{file}@#{token(format, FORMATS, "format")}" unless format.nil?
        build("/resize/#{w}/#{h}/", file)
      end

      # The Cloudflare-Images-compatible options route,
      # +{prefix}/width=750,quality=80/{file}+. Requires the server to
      # have +OXIMG_OPTIONS_PREFIX+ set and the same prefix configured
      # here. Options are emitted in a fixed order: the signature covers
      # the list verbatim, so a stable order is also a stable cache key.
      def options(source, width: nil, height: nil, quality: nil, format: nil)
        prefix = @config.options_prefix
        unless prefix
          raise ConfigurationError,
            "options_prefix is not configured (set it to the server's OXIMG_OPTIONS_PREFIX)"
        end

        build("#{prefix}/#{option_list(width, height, quality, format)}/", source_path(source))
      end

      private

      # +prefix+ is generated, ASCII-safe and already signed-form; only
      # the caller's source path needs escaping. The signature is computed
      # over the decoded path and prepended as its own segment, which is
      # what puts it before the options route's prefix rather than after.
      def build(prefix, file)
        @config.validate!
        url = +""
        url << @config.endpoint if @config.endpoint
        if @config.signing?
          url << "/" << Signer.sign(@config.key, @config.salt, "#{prefix}#{file}")
        end
        url << prefix << escape(file)
        url.freeze
      end

      def escape(path)
        path.gsub(UNSAFE) { |c| c.unpack("C*").map { |b| format("%%%02X", b) }.join }
      end

      def source_path(source)
        path = source.to_s.sub(%r{\A/+}, "")
        raise ArgumentError, "source path is empty" if path.empty?

        # "." and ".." would make the signed path and the path the server
        # resolves two different things; the server refuses them anyway.
        if path.split("/").any? { |segment| segment == "." || segment == ".." }
          raise ArgumentError, "source path must not contain \".\" or \"..\" segments: #{source.inspect}"
        end

        path
      end

      def dimension(value, name)
        unless value.is_a?(Integer) && value >= 0 && value <= MAX_DIMENSION
          raise ArgumentError, "#{name} must be an Integer in 0..#{MAX_DIMENSION}, got #{value.inspect}"
        end

        value
      end

      def option_list(width, height, quality, format)
        if width.nil? && height.nil?
          raise ArgumentError, "at least one of width/height is required on the options route"
        end

        parts = []
        parts << "width=#{bounded(width, "width", 1, MAX_DIMENSION)}" unless width.nil?
        parts << "height=#{bounded(height, "height", 1, MAX_DIMENSION)}" unless height.nil?
        parts << "quality=#{bounded(quality, "quality", 1, 100)}" unless quality.nil?
        parts << "format=#{token(format, OPTION_FORMATS, "format")}" unless format.nil?
        parts.join(",")
      end

      def bounded(value, name, min, max)
        unless value.is_a?(Integer) && value >= min && value <= max
          raise ArgumentError, "#{name} must be an Integer in #{min}..#{max}, got #{value.inspect}"
        end

        value
      end

      def token(value, allowed, name)
        token = value.to_s.downcase
        unless allowed.include?(token)
          raise ArgumentError, "unknown #{name} #{value.inspect} (#{allowed.join("|")})"
        end

        token
      end
    end
  end
end
