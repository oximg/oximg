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
    ABI_VERSION = 1

    # Statuses (ext/src/lib.rs).
    OK = 0
    READ_FAILED = 3

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
      # IO#read(n) — a File, a socket, a StringIO. Only what the header
      # needs is read, and the IO is left where the read stopped. An
      # exception IO#read raises comes out of here unchanged.
      def probe_io(io)
        failure = nil
        reader = proc do |_ctx, buf, cap|
          # Nothing may raise through the library's frames: it is not
          # Ruby code, and unwinding across it is undefined behavior.
          # Every exception, Interrupt included, is held and re-raised
          # once the call is back in Ruby.
          chunk = io.read(cap)
          if chunk.nil? || chunk.empty?
            0
          elsif chunk.bytesize > cap
            raise IOError, "#{io.class}#read(#{cap}) returned #{chunk.bytesize} bytes"
          else
            buf.put_bytes(0, chunk)
            chunk.bytesize
          end
        rescue Exception => e
          failure = e
          -1
        end
        # Thread#kill and Thread#raise from another thread are not
        # exceptions the reader above can rescue; delivered inside it,
        # they too would jump through the library's frames. They wait
        # until the call returns. The cost: a read that blocks forever
        # cannot be interrupted, so give such an IO a timeout
        # (IO#timeout=, Ruby 3.2+), whose IO::TimeoutError is an
        # ordinary exception. #read must not `throw` or kill its own
        # thread, for the same reason: both act at once, and no mask
        # holds them back.
        result = Thread.handle_interrupt(Object => :never) do
          call { |dims, msg| binding_module.oximg_probe_read(reader, nil, dims, msg, MESSAGE_BYTES) }
        end
        raise failure if failure

        result
      end

      # Forgets the loaded library. For tests, and for a process that
      # changes OXIMG_PROBE_LIB after boot.
      def reset!
        remove_instance_variable(:@binding_module) if instance_variable_defined?(:@binding_module)
      end

      private

      def call
        dims = FFI::MemoryPointer.new(:uint64, 2)
        msg = FFI::MemoryPointer.new(:char, MESSAGE_BYTES)
        status = yield(dims, msg)
        return if status == READ_FAILED

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
          # GVL; the IO variant calls back into Ruby and keeps it.
          attach_function :oximg_probe_path, %i[pointer size_t pointer pointer size_t], :int, blocking: true
          callback :oximg_read_fn, %i[pointer pointer size_t], :ssize_t
          attach_function :oximg_probe_read, %i[oximg_read_fn pointer pointer pointer size_t], :int
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
