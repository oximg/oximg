# frozen_string_literal: true

require "test_helper"
require "pathname"
require "stringio"

# The in-process probe (Native): the library `rake compile` builds,
# loaded through OXIMG_PROBE_LIB. Its contract is the executable's
# answer, so the central test runs both on every fixture.
class Oximg::NativeTest < Oximg::Test
  def setup
    super
    require_probe_library!
  end

  def with_library(&block)
    with_env("OXIMG_PROBE_LIB", probe_library) do
      Oximg::Native.reset!
      block.call
    end
  end

  # Every image fixture, the malformed ones included.
  def image_fixtures
    Dir[File.join(FIXTURES, "*.{jpg,png,webp,gif,avif}")].sort
  end

  # The answer, or the error class it raised: a hash, or
  # [ProcessingError, status].
  def outcome
    yield
  rescue Oximg::ProcessingError => e
    [e.class, e.status]
  end

  def spawned(path)
    outcome { Oximg.send(:parse_probe, Oximg::Binary.run("probe", path).first) }
  end

  def test_answers_like_the_executable_on_every_fixture
    require_executable!
    with_library do
      assert Oximg.in_process_probe?
      fixtures = image_fixtures
      assert_operator fixtures.size, :>=, 30
      # The library is built without AVIF, as the release executables
      # are; an executable from `cargo build --features avif` (the
      # local full gate builds one) reads AVIF, and differs there by
      # build, not by probe.
      if spawned(fixture("photo.avif")).is_a?(Hash)
        fixtures = fixtures.reject { |path| path.end_with?(".avif") }
      end
      fixtures.each do |path|
        assert_equal spawned(path), outcome { Oximg.probe(path) }, File.basename(path)
      end
    end
  end

  def test_reads_an_io_as_it_reads_the_path
    with_library do
      image_fixtures.each do |path|
        from_io = outcome { Oximg.probe(StringIO.new(File.binread(path))) }
        assert_equal outcome { Oximg.probe(path) }, from_io, File.basename(path)
      end
    end
  end

  # Only the header is read: 64 KiB of a 4 MiB stream, and the IO is
  # left where the read stopped.
  def test_reads_only_the_header_from_an_io
    with_library do
      io = StringIO.new(File.binread(fixture("photo.jpg")) + ("\0".b * (4 << 20)))
      assert_equal({content_type: "image/jpeg", format: :jpeg, width: 200, height: 150}, Oximg.probe(io))
      assert_equal 64 << 10, io.pos
    end
  end

  # A Pathname answers #read too, and a File both #read and #path: both
  # are paths, never streams.
  def test_treats_pathname_and_file_as_paths
    with_library do
      want = {content_type: "image/png", format: :png, width: 200, height: 150}
      assert_equal want, Oximg.probe(Pathname(fixture("rgb.png")))
      File.open(fixture("rgb.png"), "rb") do |file|
        file.read(100)
        assert_equal want, Oximg.probe(file)
        assert_equal 100, file.pos, "a File is probed by path, its position untouched"
      end
    end
  end

  # Every IO answers #path since Ruby 3.2: nil for a pipe, "" for a
  # socket. Those are streams, not paths.
  def test_reads_a_pipe_and_a_socket
    require "socket"
    bytes = File.binread(fixture("photo.jpg"))
    want = {content_type: "image/jpeg", format: :jpeg, width: 200, height: 150}
    with_library do
      [IO.pipe, UNIXSocket.pair].each do |reader, writer|
        writer.write(bytes)
        writer.close
        assert_equal want, Oximg.probe(reader), reader.class.name
      ensure
        reader&.close
      end
    end
  end

  PNG = {content_type: "image/png", format: :png, width: 200, height: 150}.freeze

  # An open File is read through its descriptor, from the start: what
  # it holds, whatever its name leads to by now, and wherever it was
  # left. Each case opens rgb.png, reads 100 bytes, then moves the name
  # (photo.jpg is what a lookup by name would find instead).
  def test_reads_an_open_file_by_its_descriptor
    with_library do
      {
        "untouched" => ->(_dir, _path) {},
        "name replaced" => ->(dir, path) {
          File.binwrite(File.join(dir, "new"), File.binread(fixture("photo.jpg")))
          File.rename(File.join(dir, "new"), path)
        },
        "renamed away" => ->(dir, path) { File.rename(path, File.join(dir, "moved")) },
        "unlinked" => ->(_dir, path) { File.delete(path) }
      }.each do |name, move|
        in_tmpdir do |dir|
          path = File.join(dir, "x.png")
          File.binwrite(path, File.binread(fixture("rgb.png")))
          File.open(path, "rb") do |file|
            file.read(100)
            move.call(dir, path)
            assert_equal PNG, Oximg.probe(file), name
            assert_equal 100, file.pos, "#{name}: position untouched"
          end
        end
      end
    end
  end

  # A File opened by a relative name, after the process moved to a
  # directory where that name is another image.
  def test_reads_an_open_file_after_the_working_directory_moved
    with_library do
      in_tmpdir do |dir|
        Dir.mkdir(File.join(dir, "a"))
        Dir.mkdir(File.join(dir, "b"))
        File.binwrite(File.join(dir, "a", "x"), File.binread(fixture("rgb.png")))
        File.binwrite(File.join(dir, "b", "x"), File.binread(fixture("photo.jpg")))
        home = Dir.pwd
        begin
          file = Dir.chdir(File.join(dir, "a")) { File.open("x", "rb") }
          Dir.chdir(File.join(dir, "b"))
          assert_equal PNG, Oximg.probe(file)
        ensure
          Dir.chdir(home)
          file&.close
        end
      end
    end
  end

  # A write-only File cannot be read through its descriptor; it is
  # read by its name, as before.
  def test_reads_a_write_only_file_by_its_name
    with_library do
      in_tmpdir do |dir|
        path = File.join(dir, "x.png")
        File.binwrite(path, File.binread(fixture("rgb.png")))
        File.open(path, "ab") { |file| assert_equal PNG, Oximg.probe(file) }
      end
    end
  end

  # A File on a FIFO has no start to come back to: it is a stream.
  def test_reads_a_fifo_as_a_stream
    with_library do
      in_tmpdir do |dir|
        fifo = File.join(dir, "fifo")
        File.mkfifo(fifo)
        writer = Thread.new { File.open(fifo, "wb") { |w| w.write(File.binread(fixture("rgb.png"))) } }
        File.open(fifo, "rb") { |file| assert_equal PNG, Oximg.probe(file) }
        writer.join
      end
    end
  end

  # A closed File still names its file.
  def test_reads_a_closed_file_by_its_name
    file = File.open(fixture("rgb.png"), "rb")
    file.close
    with_library do
      assert_equal({content_type: "image/png", format: :png, width: 200, height: 150}, Oximg.probe(file))
    end
  end

  def test_reports_a_missing_file
    with_library do
      error = assert_raises(Oximg::ProcessingError) { Oximg.probe(fixture("missing.jpg")) }
      assert_match(/\Aread .*missing\.jpg: No such file or directory/, error.message)
      assert_equal 1, error.status
    end
  end

  def test_reports_a_directory
    with_library do
      error = assert_raises(Oximg::ProcessingError) { Oximg.probe(FIXTURES) }
      assert_match(/Is a directory/, error.message)
    end
  end

  def test_reports_what_is_not_an_image
    with_library do
      error = assert_raises(Oximg::ProcessingError) { Oximg.probe(StringIO.new("hello")) }
      assert_equal "source too short", error.message
      assert_equal Encoding::UTF_8, error.message.encoding
    end
  end

  # The library does no reading: IO#read runs in Ruby between steps,
  # never under the library. Whatever leaves it, leaves probe as from
  # any Ruby method, and the library is usable after.
  def test_reraises_what_the_io_raises
    boom = Class.new(StandardError)
    io = Object.new
    io.define_singleton_method(:read) { |_n| raise boom, "disk went away" }
    with_library do
      error = assert_raises(boom) { Oximg.probe(io) }
      assert_equal "disk went away", error.message
      assert_equal 200, Oximg.probe(fixture("photo.jpg"))[:width]
    end
  end

  # A kill from another thread (a request timeout, a shutdown) lands in
  # the read at once, rather than once a probe finishes.
  def test_lets_a_kill_land_in_the_read
    reading = Queue.new
    io = Object.new
    io.define_singleton_method(:read) do |_n|
      reading << true
      sleep 5
      ""
    end
    with_library do
      thread = Thread.new { Oximg.probe(io) }
      reading.pop
      started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      thread.kill
      assert_nil thread.value, "the thread was killed"
      assert_operator Process.clock_gettime(Process::CLOCK_MONOTONIC) - started, :<, 2
      assert_equal 200, Oximg.probe(fixture("photo.jpg"))[:width]
    end
  end

  # A Timeout.timeout the source's own #read sets up fires inside it.
  def test_lets_a_timeout_inside_the_read_fire
    require "timeout"
    io = Object.new
    io.define_singleton_method(:read) { |_n| Timeout.timeout(0.05) { sleep 5 } }
    with_library do
      started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      assert_raises(Timeout::Error) { Oximg.probe(io) }
      assert_operator Process.clock_gettime(Process::CLOCK_MONOTONIC) - started, :<, 2
    end
  end

  # So does a Thread#raise from outside, as an outer Timeout.timeout
  # sends.
  def test_lets_an_outer_raise_land_in_the_read
    reading = Queue.new
    io = Object.new
    io.define_singleton_method(:read) do |_n|
      reading << true
      sleep 5
      ""
    end
    with_library do
      thread = Thread.new { Oximg.probe(io) }
      reading.pop
      thread.report_on_exception = false
      started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      thread.raise(ArgumentError, "from outside")
      error = assert_raises(ArgumentError) { thread.value }
      assert_equal "from outside", error.message
      # Now, not once the 5 s read is over.
      assert_operator Process.clock_gettime(Process::CLOCK_MONOTONIC) - started, :<, 2
    end
  end

  # "To the end" (an AVIF answer, pipeline::probe_step) is read in
  # bounded chunks: the library built here has no AVIF, so this calls the
  # loop directly, with an IO that answers #read(n) only.
  def test_reads_to_the_end_in_chunks
    bytes = "x".b * ((3 << 20) + 5)
    source = StringIO.new(bytes)
    lengths = []
    io = Object.new
    io.define_singleton_method(:read) do |n|
      lengths << n
      source.read(n)
    end
    prefix = String.new(encoding: Encoding::BINARY)
    assert Oximg::Native.send(:read_to, io, prefix, Oximg::Native::READ_ALL), "reports the end"
    assert_equal bytes, prefix
    assert_equal [1 << 20], lengths.uniq
  end

  # Path reads run without the GVL; concurrent probes must not trip on
  # one another.
  def test_probes_from_many_threads
    with_library do
      want = Oximg.probe(fixture("photo.jpg"))
      threads = 8.times.map { Thread.new { 200.times.map { Oximg.probe(fixture("photo.jpg")) } } }
      threads.each { |t| t.value.each { |got| assert_equal want, got } }
    end
  end

  # An explicit library is never second-guessed: a wrong one raises,
  # where a broken bundled one would fall back to spawning.
  def test_refuses_an_explicit_library_that_is_not_there
    with_env("OXIMG_PROBE_LIB", File.join(FIXTURES, "missing.so")) do
      Oximg::Native.reset!
      error = assert_raises(LoadError) { Oximg.probe(fixture("photo.jpg")) }
      assert_match(/OXIMG_PROBE_LIB/, error.message)
    end
  end

  def test_refuses_a_library_that_is_not_one
    with_env("OXIMG_PROBE_LIB", fixture("photo.jpg")) do
      Oximg::Native.reset!
      assert_raises(LoadError) { Oximg.probe(fixture("photo.jpg")) }
    end
  end
end
