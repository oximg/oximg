# frozen_string_literal: true

require "minitest/autorun"
require "tmpdir"
require "oximg"

module Oximg
  class Test < Minitest::Test
    # The Rust suite's fixtures, used rather than copied: the gem lives
    # in the oximg repo, and a second photo.jpg would be a second thing
    # to keep honest.
    FIXTURES = File.expand_path("../../../tests/fixtures", __dir__)

    def setup
      Oximg::Binary.reset!
      Oximg::Native.reset!
    end

    def teardown
      Oximg::Binary.reset!
      Oximg::Native.reset!
    end

    def fixture(name)
      File.join(FIXTURES, name)
    end

    # What probe answers for a source with no orientation: 1, displayed
    # as stored. A class method too, for hashes a test class holds as a
    # constant.
    def self.upright(content_type, format, width, height)
      {content_type: content_type, format: format, width: width, height: height,
       orientation: 1, display_width: width, display_height: height}
    end

    def upright(...)
      Test.upright(...)
    end

    # The integration tests run the real binary when one is around —
    # a `cargo build` in this checkout, a platform gem, or PATH — and
    # skip when there is none, so the unit suite stays runnable on a
    # machine with no Rust toolchain.
    #
    # Of this checkout's builds, the newest: `agent` is the profile
    # agents iterate on, and a stale release build left behind would
    # otherwise test yesterday's binary.
    def executable
      @executable ||= begin
        built = %w[debug agent release]
          .map { |profile| File.expand_path("../../../target/#{profile}/oximg", __dir__) }
          .select { |path| File.executable?(path) }
        built.max_by { |path| File.mtime(path) } ||
          (Oximg.available? ? Oximg.executable : nil)
      end
    end

    def require_executable!
      skip "no oximg executable (cargo build, or install one)" unless executable

      Oximg.executable = executable
    end

    # The probe library `rake compile` builds. Its tests skip without
    # one, like the executable's — unless OXIMG_TEST_REQUIRE_PROBE_LIB
    # is set, as CI sets it, where a missing library is a failure: a
    # suite that skipped every in-process test would otherwise pass.
    def probe_library
      @probe_library ||= File.expand_path("../../../target/oximg-probe/release/#{Oximg::Native::LIBRARY}", __dir__)
      File.file?(@probe_library) ? @probe_library : nil
    end

    def require_probe_library!
      return if probe_library

      message = "no probe library (rake compile)"
      flunk message if ENV["OXIMG_TEST_REQUIRE_PROBE_LIB"]
      skip message
    end

    # A two-frame GIF on a 2x2 logical screen whose second frame's
    # rectangle is 3x3: larger than the screen, which a writer may do.
    # Decode work prices each frame at the larger of the two, so it is
    # 4 + 9, where frames x width x height says 8. Built here,
    # independent of oximg.
    def oversized_frame_gif
      frame = lambda do |w, h|
        # Image descriptor, no local palette; LZW minimum code size 2,
        # one data sub-block, block terminator.
        [0x2C, 0, 0, w, h, 0].pack("CvvvvC") + [2, 2, 0x4C, 0x01, 0].pack("C*")
      end
      # Logical screen 2x2 with a two-color global palette.
      "GIF89a".b + [2, 2, 0x80, 0, 0].pack("vvCCC") + [0, 0, 0, 255, 255, 255].pack("C*") +
        frame.call(2, 2) + frame.call(3, 3) + ";".b
    end

    def with_env(name, value)
      previous = ENV[name]
      ENV[name] = value
      yield
    ensure
      ENV[name] = previous
    end

    def in_tmpdir(&block)
      Dir.mktmpdir("oximg-gem", &block)
    end
  end
end
