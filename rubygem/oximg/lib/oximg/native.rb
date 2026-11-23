# frozen_string_literal: true

require "rbconfig"

module Oximg
  # The in-process header read: oximg's own probe, compiled as a small
  # C library (rubygem/oximg/ext) and called through the +ffi+ gem, so
  # Oximg.probe costs microseconds instead of a process spawn.
  #
  # Only the header read runs in-process. It reads a bounded prefix of
  # the source and decodes no pixels; resize stays a child process, for
  # the memory and crash isolation a full decode wants. With
  # +animation: true+ the whole source is read, to count its frames,
  # still without decoding one.
  #
  # Resolution order, first hit wins:
  #
  #   1. +OXIMG_PROBE_LIB+ — an explicit path always wins, and a wrong
  #      one raises rather than falling back, as +OXIMG_BIN+ does.
  #   2. the library bundled in this gem, beside this file — platform
  #      gems ship one. If it cannot load (no +ffi+ gem, a missing
  #      system library), probe falls back to spawning the executable:
  #      slower, same answer.
  #
  # The plain-Ruby gem ships no library and always spawns.
  module Native
    LIBRARY = "liboximg_probe.#{RbConfig::CONFIG["SOEXT"]}"

    # Populated by the platform gems at package time; absent in the
    # plain-Ruby gem.
    BUNDLED = File.expand_path("native/#{LIBRARY}", __dir__)

    # The library's oximg_probe_abi_version this code was written for.
    ABI_VERSION = 4

    # Statuses (ext/src/lib.rs).
    OK = 0
    READ = 3

    # The byte count READ gives for "to the end".
    READ_ALL = 2**64 - 1

    # What a call writes (ext/src/lib.rs): stored width, stored height
    # and orientation; with the animation summary, also frames (0: not
    # animated), duration_ms, loop_count, decode_work, whether
    # decode_work is priced, and whether the source is complete.
    HEADER_SLOTS = 3
    ANIMATION_SLOTS = 9

    # The complete slot (ext/src/lib.rs): 0 is a format the walk does
    # not check, so a zeroed buffer never reads as "whole".
    COMPLETE_VALUES = [nil, false, true].freeze

    # Room for an error message; the library cuts a longer one at a
    # character boundary.
    MESSAGE_BYTES = 1024

    class << self
      # True when the library is loaded and probe runs in-process.
      def available?
        !binding_module.nil?
      end

      # Header of the file at +path+ (already expanded); with
      # +animation+, its animation summary too, which reads all of it.
      def probe_path(path, animation: false)
        # Copied off the Ruby heap: the call runs without the GVL, while
        # other threads may run GC.
        bytes = FFI::MemoryPointer.from_string(path)
        function = animation ? :oximg_probe_animation_path : :oximg_probe_path
        call(animation) { |out, msg| binding_module.public_send(function, bytes, path.bytesize, out, msg, MESSAGE_BYTES) }
      end

      # Header of the open, readable regular +file+, read through its
      # descriptor from the start with positioned reads: what it holds,
      # whatever its name leads to by now, its position untouched.
      def probe_file(file, animation: false)
        function = animation ? :oximg_probe_animation_fd : :oximg_probe_fd
        call(animation) { |out, msg| binding_module.public_send(function, file.fileno, out, msg, MESSAGE_BYTES) }
      end

      # Header of +io+, read from its current position through
      # IO#read(n) — a pipe, a socket, a StringIO. Only what the header
      # needs is read, and the IO is left where the read stopped.
      #
      # The library decides how much to read and the reading happens
      # here, between calls, so the IO's code never runs under the
      # library's frames: whatever it raises, a Timeout or a Thread#kill
      # included, leaves the way it would leave any Ruby method.
      #
      # With +animation+ there is nothing to decide: the IO is read to
      # its end, here, and the library is handed all of it.
      def probe_io(io, animation: false)
        prefix = String.new(encoding: Encoding::BINARY)
        if animation
          read_to(io, prefix, READ_ALL)
          return call(true) do |out, msg|
            binding_module.oximg_probe_animation_bytes(prefix, prefix.bytesize, out, msg, MESSAGE_BYTES)
          end
        end

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

      def call(animation = false)
        slots = animation ? ANIMATION_SLOTS : HEADER_SLOTS
        out = FFI::MemoryPointer.new(:uint64, slots)
        msg = FFI::MemoryPointer.new(:char, MESSAGE_BYTES)
        status = yield(out, msg)
        return if status == READ

        text = msg.read_string.force_encoding(Encoding::UTF_8)
        raise ProcessingError.new(text, 1) unless status == OK

        width, height, orientation, frames, duration_ms, loop_count, decode_work, priced, complete =
          out.read_array_of_uint64(slots)
        header = Header.build(text, width, height, orientation)
        return header unless animation

        summary = unless frames.zero?
          {frames: frames, duration_ms: duration_ms, loop_count: loop_count,
           decode_work: priced.zero? ? nil : decode_work}
        end
        header.merge(animation: summary, complete: COMPLETE_VALUES.fetch(complete))
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

          # Absolute, so the file loaded is the one just checked. Where
          # dlopen looks for a bare name depends on the platform (glibc
          # never looks in the working directory; dyld does, after other
          # places). absolute_path, unlike expand_path, leaves "~" alone,
          # as File.file? did.
          open_library(File.absolute_path(configured))
        end
      end

      def open_library(path)
        require "ffi"
        Module.new do
          extend FFI::Library
          ffi_lib path
          attach_function :oximg_probe_abi_version, [], :uint32
          # Checked before the rest is bound: an older library lacks the
          # newer entry points, and should be refused for its version
          # rather than for a symbol.
          abi = oximg_probe_abi_version
          unless abi == ABI_VERSION
            raise LoadError, "#{path} speaks probe ABI #{abi}; this gem speaks #{ABI_VERSION}"
          end

          # A path or descriptor read blocks on the filesystem, so it runs
          # without the GVL. A step only parses bytes already in memory and
          # keeps it, which also keeps the prefix String where it is.
          attach_function :oximg_probe_path, %i[pointer size_t pointer pointer size_t], :int, blocking: true
          attach_function :oximg_probe_fd, %i[int pointer pointer size_t], :int, blocking: true
          attach_function :oximg_probe_step, %i[buffer_in size_t uint8 pointer pointer size_t], :int
          # The same split for the whole-source reads: the library reads
          # a path or a descriptor itself, without the GVL; bytes Ruby
          # already read are walked under it (no pixel is decoded).
          attach_function :oximg_probe_animation_path, %i[pointer size_t pointer pointer size_t], :int,
            blocking: true
          attach_function :oximg_probe_animation_fd, %i[int pointer pointer size_t], :int, blocking: true
          attach_function :oximg_probe_animation_bytes, %i[buffer_in size_t pointer pointer size_t], :int
        end
      end
    end
  end
end
