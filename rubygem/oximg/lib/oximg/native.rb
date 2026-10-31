# frozen_string_literal: true

require "rbconfig"

module Oximg
  # The in-process header read: oximg's own probe, compiled as a small
  # C library (rubygem/oximg/ext) and called through the +ffi+ gem, so
  # Oximg.probe costs microseconds instead of a process spawn.
  #
  # Only the header read runs in-process. It reads a bounded prefix of
  # the source and decodes no pixels; resize stays a child process, for
  # the memory and crash isolation a full decode wants.
  #
  # Resolution order, first hit wins:
  #
  #   1. +OXIMG_PROBE_LIB+ — an explicit path always wins, and a wrong
  #      one raises rather than falling back, as +OXIMG_BIN+ does.
  #   2. the library bundled in this gem, beside this file, when the
  #      gem ships one. If it cannot load (no +ffi+ gem, a missing
  #      system library), probe falls back to spawning the executable:
  #      slower, same answer.
  #
  # A gem without a library, as released gems are today, always spawns.
  module Native
    LIBRARY = "liboximg_probe.#{RbConfig::CONFIG["SOEXT"]}"

    # Populated at package time when a gem bundles the library; absent
    # otherwise.
    BUNDLED = File.expand_path("native/#{LIBRARY}", __dir__)

    # The library's oximg_probe_abi_version this code was written for.
    ABI_VERSION = 1

    # Statuses (ext/src/lib.rs).
    OK = 0
    READ = 3

    # The byte count READ gives for "to the end".
    READ_ALL = 2**64 - 1

    # Room for an error message; the library cuts a longer one at a
    # character boundary.
    MESSAGE_BYTES = 1024

    class << self
      # True when the library is loaded and probe runs in-process.
      def available?
        !binding_module.nil?
      end

      # Header of the file at +path+ (already expanded).
      def probe_path(path)
        # Copied off the Ruby heap: the call runs without the GVL, while
        # other threads may run GC.
        bytes = FFI::MemoryPointer.from_string(path)
        call { |dims, msg| binding_module.oximg_probe_path(bytes, path.bytesize, dims, msg, MESSAGE_BYTES) }
      end

      # Header of +io+, read from its current position through
      # IO#read(n) — a pipe, a socket, a StringIO. Only what the header
      # needs is read, and the IO is left where the read stopped.
      #
      # The library decides how much to read and the reading happens
      # here, between calls, so the IO's code never runs under the
      # library's frames: whatever it raises, a Timeout or a Thread#kill
      # included, leaves the way it would leave any Ruby method.
      def probe_io(io)
        prefix = String.new(encoding: Encoding::BINARY)
        eof = false
        loop do
          want = nil
          result = call do |dims, msg|
            status = binding_module.oximg_probe_step(prefix, prefix.bytesize, eof ? 1 : 0, dims, msg, MESSAGE_BYTES)
            want = dims.read_uint64 if status == READ
            status
          end
          return result unless want

          eof = read_to(io, prefix, want)
        end
      end

      # Forgets the loaded library. For tests, and for a process that
      # changes OXIMG_PROBE_LIB after boot.
      def reset!
        remove_instance_variable(:@binding_module) if instance_variable_defined?(:@binding_module)
      end

      private

      # Reads +io+ onto +prefix+ until it holds +want+ bytes; true if the
      # source ended first. READ_ALL asks for everything, which is read
      # a MiB at a time: a length that large would be allocated up
      # front, and an IO need only answer #read(n).
      def read_to(io, prefix, want)
        while want == READ_ALL || prefix.bytesize < want
          chunk = io.read(want == READ_ALL ? 1 << 20 : want - prefix.bytesize)
          return true if chunk.nil? || chunk.empty?

          prefix << chunk.b
        end
        false
      end

      def call
        dims = FFI::MemoryPointer.new(:uint64, 2)
        msg = FFI::MemoryPointer.new(:char, MESSAGE_BYTES)
        status = yield(dims, msg)
        return if status == READ

        text = msg.read_string.force_encoding(Encoding::UTF_8)
        raise ProcessingError.new(text, 1) unless status == OK

        width, height = dims.read_array_of_uint64(2)
        {content_type: text, format: CONTENT_TYPE_FORMATS[text], width: width, height: height}
      end

      # nil when there is no library to use. Memoized either way, so a
      # gem without one does not look again on every probe.
      def binding_module
        return @binding_module if instance_variable_defined?(:@binding_module)

        @binding_module = load_library
      end

      def load_library
        configured = ENV["OXIMG_PROBE_LIB"]
        if configured.nil? || configured.strip.empty?
          return nil unless File.file?(BUNDLED)

          begin
            open_library(BUNDLED)
          rescue LoadError
            nil
          end
        else
          unless File.file?(configured)
            raise LoadError, "OXIMG_PROBE_LIB is set to #{configured.inspect}, which is not a file"
          end

          open_library(configured)
        end
      end

      def open_library(path)
        require "ffi"
        mod = Module.new do
          extend FFI::Library
          ffi_lib path
          attach_function :oximg_probe_abi_version, [], :uint32
          # A path read blocks on the filesystem, so it runs without the
          # GVL. A step only parses bytes already in memory and keeps
          # it, which also keeps the prefix String where it is.
          attach_function :oximg_probe_path, %i[pointer size_t pointer pointer size_t], :int, blocking: true
          attach_function :oximg_probe_step, %i[buffer_in size_t uint8 pointer pointer size_t], :int
        end
        abi = mod.oximg_probe_abi_version
        unless abi == ABI_VERSION
          raise LoadError, "#{path} speaks probe ABI #{abi}; this gem speaks #{ABI_VERSION}"
        end

        mod
      end
    end
  end
end
