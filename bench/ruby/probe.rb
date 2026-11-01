# frozen_string_literal: true

# What one header read costs from Ruby (issue #134): Oximg.probe
# in-process through the probe library — by path, by open File, by
# StringIO — against the spawned `oximg probe` it replaces, and
# ruby-vips' header read where ruby-vips is installed. Every mode
# answers the stored size and the EXIF orientation (#163), and every
# answer is checked against the executable's before anything is timed.
#
# Every source is read once per round in each mode, the modes
# interleaved within the round, over several rounds; the median per
# mode is reported (bench/METHODOLOGY.md). Run it from this checkout's
# gem source, against release builds:
#
#   cargo build --release && (cd rubygem/oximg && bundle exec rake compile)
#   ruby -I rubygem/oximg/lib bench/ruby/probe.rb [rounds] [image ...]
#
# Without images it reads the fixtures below, plus an 8 MB JPEG made by
# padding photo.jpg (the header is at the front; the rest is never read).

require "rbconfig"
require "stringio"
require "tmpdir"

ROOT = File.expand_path("../..", __dir__)
rounds = Integer(ARGV.shift || 7)
images = ARGV.dup
if images.empty?
  fixtures = File.join(ROOT, "tests/fixtures")
  images = %w[photo.jpg cmyk_icc.jpg rgb16.png anim.gif photo.webp].map { |f| File.join(fixtures, f) }
  big = File.join(Dir.tmpdir, "oximg-probe-bench-8mb.jpg")
  File.binwrite(big, File.binread(images.first) + ("\0".b * (8 << 20)))
  images << big
end

library = File.join(ROOT, "target/oximg-probe/release/liboximg_probe.#{RbConfig::CONFIG["SOEXT"]}")
ENV["OXIMG_PROBE_LIB"] = library
ENV["OXIMG_BIN"] = File.join(ROOT, "target/release/oximg")
require "oximg"
abort "no probe library at #{library} (rake compile)" unless Oximg.in_process_probe?

begin
  require "vips"
  Vips.cache_set_max(0)
rescue LoadError
  nil
end

# Calls per timing: in-process calls take microseconds, so a batch is
# timed and divided; a spawn takes milliseconds and needs fewer.
BATCH = {path: 2000, file: 2000, stringio: 2000, spawn: 40, vips: 200}

def per_call_us(calls)
  started = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
  calls.times { yield }
  (Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond) - started) / 1000.0 / calls
end

def spawned(path)
  Oximg.send(:parse_probe, Oximg::Binary.run("probe", path).first)
end

images.each do |path|
  bytes = File.binread(path)
  want = spawned(path)
  File.open(path, "rb") do |file|
    modes = {
      path: -> { Oximg.probe(path) },
      file: -> { Oximg.probe(file) },
      stringio: -> { Oximg.probe(StringIO.new(bytes)) },
      spawn: -> { spawned(path) }
    }
    # libvips answers what Oximg.probe does: stored size and the EXIF
    # orientation (#163), 1 when the field is absent.
    if defined?(Vips)
      modes[:vips] = lambda do
        image = Vips::Image.new_from_file(path)
        orientation = image.get_typeof("orientation").zero? ? 1 : image.get("orientation")
        [image.width, image.height, orientation]
      end
    end
    modes.each do |name, run|
      got = run.call
      got = want.values_at(:width, :height, :orientation) == got ? want : got if name == :vips
      raise "#{name} answered #{got.inspect} for #{path}, want #{want.inspect}" unless got == want
    end

    times = Hash.new { |h, k| h[k] = [] }
    rounds.times do
      modes.each { |name, run| times[name] << per_call_us(BATCH.fetch(name)) { run.call } }
    end
    cells = times.map do |name, values|
      median = values.sort[values.size / 2]
      median >= 1000 ? format("%s %.2f ms", name, median / 1000) : format("%s %.1f µs", name, median)
    end
    printf("%-26s %9d B  %s\n", File.basename(path), bytes.bytesize, cells.join("  "))
  end
end
cpu = case RbConfig::CONFIG["host_os"]
      when /darwin/ then `sysctl -n machdep.cpu.brand_string`.strip
      else File.read("/proc/cpuinfo")[/^model name\s*:\s*(.*)$/, 1] || RbConfig::CONFIG["host_cpu"]
      end
puts "#{cpu}; ruby #{RUBY_VERSION} #{RUBY_PLATFORM}; ffi #{FFI::VERSION}; #{rounds} rounds, median"
