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
    end

    def teardown
      Oximg::Binary.reset!
    end

    def fixture(name)
      File.join(FIXTURES, name)
    end

    # The integration tests run the real binary when one is around —
    # a `cargo build` in this checkout, a platform gem, or PATH — and
    # skip when there is none, so the unit suite stays runnable on a
    # machine with no Rust toolchain.
    def executable
      @executable ||= begin
        debug = File.expand_path("../../../target/debug/oximg", __dir__)
        release = File.expand_path("../../../target/release/oximg", __dir__)
        [debug, release].find { |path| File.executable?(path) } ||
          (Oximg.available? ? Oximg.executable : nil)
      end
    end

    def require_executable!
      skip "no oximg executable (cargo build, or install one)" unless executable

      Oximg.executable = executable
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
