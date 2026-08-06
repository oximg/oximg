# frozen_string_literal: true

require "open3"
require "rbconfig"

module Oximg
  # Locates the oximg executable and runs it.
  #
  # Resolution order, first hit wins:
  #
  #   1. +Oximg.executable=+, or +OXIMG_BIN+ — an explicit path always
  #      wins, and a wrong one raises rather than falling through: a
  #      silent fallback is how a process ends up running a different
  #      build than the one it was configured with.
  #   2. the binary bundled in this gem — platform gems
  #      (+oximg-x.y.z-arm64-darwin+ and friends) ship one, so
  #      +bundle install+ is the whole installation.
  #   3. +oximg+ on PATH — a Homebrew, +cargo install+ or Docker-image
  #      binary. This is what the plain-Ruby gem resolves to.
  #
  # The gem deliberately does not declare +spec.executables+: RubyGems
  # would generate a Ruby binstub around it, and this is a native
  # binary, not a script. Ask for the path instead.
  module Binary
    EXE = "oximg#{RbConfig::CONFIG["EXEEXT"]}"

    # Populated by the platform gems at package time; absent in the
    # plain-Ruby gem.
    BUNDLED = File.expand_path("../../exe/#{EXE}", __dir__)

    NOT_FOUND = <<~MSG.tr("\n", " ").strip
      The oximg executable was not found. Install a platform gem
      (it bundles one), a release binary
      (https://github.com/oximg/oximg/releases), `brew install
      oximg/tap/oximg` or `cargo install oximg` — or point OXIMG_BIN at
      the binary you already have.
    MSG

    class << self
      attr_writer :path

      def path
        @path ||= discover || raise(ExecutableNotFound, NOT_FOUND)
      end

      # True when a binary can be found — for a boot-time check, or for
      # code that falls back to another processor.
      def available?
        !path.nil?
      rescue ExecutableNotFound
        false
      end

      # `oximg --version`, without the leading program name.
      def version
        @version ||= capture("--version").sub(/\Aoximg\s+/, "")
      end

      # Runs the binary with +args+ passed as a real argv — no shell, so
      # nothing in a filename can be interpreted as syntax. Returns
      # [stdout, stderr]; a non-zero exit raises with the binary's own
      # stderr, which already names what it refused.
      def run(*args)
        out, err, status = Open3.capture3(path, *args)
        unless status.success?
          message = err.strip
          message = "oximg exited #{status.exitstatus}" if message.empty?
          raise ProcessingError.new(message, status.exitstatus)
        end
        [out, err]
      end

      # Resets the memoized lookup. For tests, and for a process that
      # changes OXIMG_BIN after boot.
      def reset!
        @path = nil
        @version = nil
      end

      private

      def capture(*args)
        out, err = run(*args)
        out.strip.empty? ? err.strip : out.strip
      end

      def discover
        configured = ENV["OXIMG_BIN"]
        unless configured.nil? || configured.strip.empty?
          unless executable_file?(configured)
            raise ExecutableNotFound, "OXIMG_BIN is set to #{configured.inspect}, which is not an executable file"
          end
          return configured
        end

        return BUNDLED if executable_file?(BUNDLED)

        search_path
      end

      def search_path
        ENV["PATH"].to_s.split(File::PATH_SEPARATOR).each do |dir|
          next if dir.empty?

          candidate = File.join(dir, EXE)
          return candidate if executable_file?(candidate)
        end
        nil
      end

      def executable_file?(candidate)
        File.file?(candidate) && File.executable?(candidate)
      end
    end
  end
end
