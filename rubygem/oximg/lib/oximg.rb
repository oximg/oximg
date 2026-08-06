# frozen_string_literal: true

require_relative "oximg/version"
require_relative "oximg/binary"

# Ruby bindings for oximg (https://github.com/oximg/oximg):
# high-performance image compression and resizing, in-process work for a
# Ruby app without libvips or ImageMagick on the box.
#
#   Oximg.resize("in.jpg", "out.jpg", width: 750)
#   Oximg.resize("in.jpg", "out.webp", width: 750, height: 500, quality: 80)
#   Oximg.probe("in.jpg")  #=> {content_type: "image/jpeg", format: :jpeg, ...}
#
# Rails/ActiveStorage integration, and URL building for a remote oximg
# server, live in the separate +oximg-rails+ gem.
module Oximg
  class Error < StandardError; end

  # No oximg executable could be found (or the configured one is not
  # executable). See Binary for the resolution order.
  class ExecutableNotFound < Error; end

  # The executable ran and refused: an unreadable source, an unsupported
  # format, a cap it was configured with. The message is the binary's
  # own stderr, which already names what it refused.
  class ProcessingError < Error
    # Exit status: 2 is a usage or configuration error, 1 a processing
    # failure.
    attr_reader :status

    def initialize(message, status = nil)
      super(message)
      @status = status
    end
  end

  # Output formats the CLI accepts for -f/--format.
  FORMATS = %i[jpg jpeg png webp avif].freeze

  # Encoder profiles: jpegli maximizes quality per byte (the default),
  # mozjpeg's fast/small trade differently.
  PRESETS = %i[jpegli fast small].freeze

  CONTENT_TYPE_FORMATS = {
    "image/jpeg" => :jpeg,
    "image/png" => :png,
    "image/webp" => :webp,
    "image/avif" => :avif
  }.freeze

  PROBE_LINE = /:\s+(\S+)\s+(\d+)x(\d+)\s+\(\d+\s+stored\s+pixels\)\s*\z/

  class << self
    # Absolute path to the executable in use; raises
    # ExecutableNotFound if there is none.
    def executable
      Binary.path
    end

    # Pins the executable, overriding discovery and OXIMG_BIN.
    def executable=(path)
      Binary.reset!
      Binary.path = path
    end

    # True when an executable can be found — for a boot-time check, or
    # for code that falls back to another processor.
    def available?
      Binary.available?
    end

    # Version of the executable, which is not necessarily this gem's
    # VERSION when the binary comes from PATH.
    def version
      Binary.version
    end

    # Fits +source+ within +width+ x +height+ and writes the re-encoded
    # image to +destination+. The source is never enlarged, and a zero
    # axis is unconstrained — <tt>width: 750</tt> alone is width-only,
    # and the default 0 x 0 re-encodes at the source's own size, which
    # is how you ask for compression without a resize.
    #
    # The output format follows +format:+, else +destination+'s
    # extension, else the source's own format. Returns the destination
    # path.
    def resize(source, destination, width: 0, height: 0, quality: nil, format: nil, preset: nil)
      output = expand(destination, "destination")
      Binary.run(*resize_argv(source, output,
        width: width, height: height, quality: quality, format: format, preset: preset))
      output
    end

    # Header-only inspection — no pixels are decoded. Returns
    # <tt>{content_type:, format:, width:, height:}</tt>, where the
    # dimensions are the stored ones (an EXIF rotation is not applied).
    def probe(source)
      out, = Binary.run("probe", expand(source, "source"))
      match = out.match(PROBE_LINE)
      raise ProcessingError, "unparsable probe output: #{out.inspect}" unless match

      {
        content_type: match[1],
        format: CONTENT_TYPE_FORMATS[match[1]],
        width: Integer(match[2]),
        height: Integer(match[3])
      }
    end

    # The argv `resize` would run, minus the executable. Public because
    # it is the honest way to see what a call turns into — and the way
    # to test that without a binary present.
    def resize_argv(source, destination, width: 0, height: 0, quality: nil, format: nil, preset: nil)
      argv = [
        "resize",
        expand(source, "source"),
        dimension(width, "width").to_s,
        dimension(height, "height").to_s,
        expand(destination, "destination")
      ]
      argv.push("-q", bounded(quality, "quality", 1, 100).to_s) unless quality.nil?
      argv.push("-f", token(format, FORMATS, "format")) unless format.nil?
      argv.push("--preset", token(preset, PRESETS, "preset")) unless preset.nil?
      argv
    end

    private

    # Absolute paths throughout: a relative name beginning with "-"
    # would otherwise reach the CLI's argument parser as a flag, and
    # the caller rarely controls what an uploaded file is called.
    def expand(path, name)
      path = path.path if path.respond_to?(:path)
      path = path.to_s
      raise ArgumentError, "#{name} path is empty" if path.strip.empty?

      File.expand_path(path)
    end

    # No upper bound: the server's 8192 cap guards request shapes, and
    # is not a limit of the pipeline the CLI drives.
    def dimension(value, name)
      unless value.is_a?(Integer) && value >= 0
        raise ArgumentError, "#{name} must be a non-negative Integer, got #{value.inspect}"
      end

      value
    end

    def bounded(value, name, min, max)
      unless value.is_a?(Integer) && value >= min && value <= max
        raise ArgumentError, "#{name} must be an Integer in #{min}..#{max}, got #{value.inspect}"
      end

      value
    end

    def token(value, allowed, name)
      token = value.to_s.downcase
      unless allowed.include?(token.to_sym)
        raise ArgumentError, "unknown #{name} #{value.inspect} (#{allowed.join("|")})"
      end

      token
    end
  end
end
