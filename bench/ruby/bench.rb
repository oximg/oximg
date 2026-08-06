# frozen_string_literal: true

# What a Rails app's variant generation costs, per image-processing gem.
#
# The task is the canonical ActiveStorage variant — fit within 750x750,
# never enlarging, re-encoded as JPEG at quality 80 — run through each
# gem at its own defaults, because defaults are what an app gets.
#
# Five axes, because wall time alone is not a result:
#
#   ms      wall time per image
#   cpu     CPU seconds for the group — the number that sets throughput
#           on a job queue, and the one that exposes a processor buying
#           its latency with every core on the box
#   RSS     peak resident set of the worker process (and its children)
#   KB      output size
#   ssim2   SSIMULACRA2 against a linear-light Lanczos reference at the
#           same dimensions, per bench/quality/QUALITY.md. Optional:
#           skipped where ssimulacra2 is not installed
#
# Each (group, gem) pair runs in its own subprocess under /usr/bin/time,
# because peak RSS is not attributable otherwise: one Ruby VM that has
# loaded libvips, ImageMagick and spawned oximg reports the high-water
# mark of whichever ran worst. A per-gem "load only" run measures the
# baseline that costs, so the resize's own peak can be read off it.
#
#   bundle exec ruby bench.rb [reps]

require "fileutils"
require "json"
require "open3"
require "tmpdir"

MAX = 750
QUALITY = 80
CORPUS = File.expand_path("../quality/corpus", __dir__)

GROUPS = {
  "large 4000x2667" => "large/*.jpg",
  "medium 2000x1334" => "medium/*.jpg",
  "kodak 768x512" => "src/*.jpg"
}.freeze

GROUP_LIMIT = {"kodak 768x512" => 8}.freeze

CONTENDERS = %w[oximg ruby-vips image_processing/vips image_processing/magick mini_magick].freeze

def sources(group)
  files = Dir[File.join(CORPUS, GROUPS.fetch(group))].sort
  (limit = GROUP_LIMIT[group]) ? files.first(limit) : files
end

def sh(*cmd)
  out, err, status = Open3.capture3(*cmd.map(&:to_s))
  raise "#{cmd.first} failed: #{err}" unless status.success?

  out
end

# ---------------------------------------------------------------------
# Child mode: does the work, prints one JSON line. Loads only the gem
# under test, so the reported RSS is that gem's and not the union.
# ---------------------------------------------------------------------

def contender(name)
  case name
  when "oximg"
    require "oximg"
    ->(src, out) { Oximg.resize(src, out, width: MAX, height: MAX, quality: QUALITY) }
  when "ruby-vips"
    require "vips"
    # libvips memoizes operations; a rerun over the same file would time
    # a cache lookup rather than a resize. No app resizes the same image
    # twice in a row, so neither does this.
    Vips.cache_set_max(0)
    # thumbnail() is the shrink-on-load path, and what image_processing
    # calls underneath — the fair comparison, not a naive resize().
    ->(src, out) { Vips::Image.thumbnail(src, MAX, height: MAX, size: :down).jpegsave(out, Q: QUALITY) }
  when "image_processing/vips"
    require "image_processing/vips"
    Vips.cache_set_max(0)
    lambda { |src, out|
      ImageProcessing::Vips.source(src).convert("jpeg")
        .resize_to_limit(MAX, MAX).saver(quality: QUALITY).call(destination: out)
    }
  when "image_processing/magick"
    require "image_processing/mini_magick"
    lambda { |src, out|
      ImageProcessing::MiniMagick.source(src).convert("jpeg")
        .resize_to_limit(MAX, MAX).saver(quality: QUALITY).call(destination: out)
    }
  when "mini_magick"
    require "mini_magick"
    lambda { |src, out|
      MiniMagick.convert do |c|
        c << src
        c.resize "#{MAX}x#{MAX}>"
        c.quality QUALITY
        c << out
      end
    }
  else
    raise ArgumentError, "unknown contender #{name}"
  end
end

def run_child(name, group, reps, dir)
  run = contender(name)
  return puts(JSON.generate(baseline: true)) if group == "--load-only"

  times = []
  bytes = []
  dims = []
  sources(group).each do |src|
    out = File.join(dir, "#{name.tr("/", "-")}-#{File.basename(src)}")
    per_image = reps.times.map do
      FileUtils.rm_f(out)
      t = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      run.call(src, out)
      (Process.clock_gettime(Process::CLOCK_MONOTONIC) - t) * 1000
    end
    # Best of N: the floor is the number that is about the work, with
    # scheduler noise and thermal drift stripped out.
    times << per_image.min
    bytes << File.size(out)
    dims << out
  end
  # CPU from Process.times rather than the wrapper: utime/stime cover
  # every thread libvips and ImageMagick start, and cutime/cstime cover
  # the oximg subprocess once it has been reaped. Same number on every
  # platform, and one less tool the machine has to have.
  t = Process.times
  puts JSON.generate(ms: times.sum / times.size, bytes: bytes.sum / bytes.size,
    cpu: t.utime + t.stime + t.cutime + t.cstime, outputs: dims)
end

# ---------------------------------------------------------------------
# Parent mode: wraps each child in /usr/bin/time and collects resources.
# ---------------------------------------------------------------------

DARWIN = RbConfig::CONFIG["host_os"].include?("darwin")

# Peak RSS needs /usr/bin/time, which not every box has. Without it the
# column is reported as absent rather than filled from /proc/self/status:
# that would count only the Ruby process, which is the whole story for
# the in-process gems and none of it for oximg's subprocess — a number
# that means something different per row is worse than no number.
TIME = File.executable?("/usr/bin/time")

# BSD time reports maximum RSS in bytes, GNU time in kibibytes. Getting
# this wrong is a silent factor of 1024, so it is derived from the
# platform rather than guessed from the magnitude.
def peak_rss_mb(stderr)
  if DARWIN
    stderr[/(\d+)\s+maximum resident set size/, 1].to_i / 1048576.0
  else
    stderr[/Maximum resident set size \(kbytes\):\s*(\d+)/, 1].to_i / 1024.0
  end
end

def measure(name, group, reps, dir)
  child = [RbConfig.ruby, __FILE__, "--child", name, group, reps.to_s, dir]
  cmd = TIME ? ["/usr/bin/time", DARWIN ? "-l" : "-v", *child] : child
  out, err, status = Open3.capture3(*cmd)
  raise "#{name}/#{group} failed:\n#{err}" unless status.success?

  json = JSON.parse(out.lines.last.to_s, symbolize_names: true)
  json.merge(rss_mb: TIME ? peak_rss_mb(err) : nil)
end

def score(src, out, refs, dir)
  w, h = sh("magick", "identify", "-format", "%w %h", out).split.map(&:to_i)
  return [nil, w, h] unless SCORING

  ref = refs[[src, w, h]] ||= begin
    path = File.join(dir, "ref-#{File.basename(src, ".*")}-#{w}x#{h}.png")
    sh("magick", src, "-colorspace", "RGB", "-filter", "Lanczos",
      "-resize", "#{w}x#{h}!", "-colorspace", "sRGB", path)
    path
  end
  [sh("ssimulacra2", ref, out).strip.to_f, w, h]
end

if ARGV.first == "--child"
  _, name, group, reps, dir = ARGV
  run_child(name, group, reps.to_i, dir)
  exit
end

# A PATH search, not `command -v`: Kernel#` skips the shell when the
# string has no metacharacters, so the builtin would be exec'd as a
# binary and raise ENOENT on any system that lacks /usr/bin/command.
SCORING = ENV["PATH"].to_s.split(File::PATH_SEPARATOR)
  .any? { |dir| File.executable?(File.join(dir, "ssimulacra2")) }
reps = (ARGV[0] || 5).to_i
results = {}
baselines = {}
refs = {}

Dir.mktmpdir("oximg-bench") do |dir|
  CONTENDERS.each { |name| baselines[name] = measure(name, "--load-only", 0, dir) }

  GROUPS.each_key do |group|
    next if sources(group).empty?

    CONTENDERS.each do |name|
      r = measure(name, group, reps, dir)
      if SCORING
        scores = sources(group).zip(r[:outputs]).map { |src, out| score(src, out, refs, dir).first }
        r[:ssim2] = scores.sum / scores.size
      end
      r[:dims] = sh("magick", "identify", "-format", "%wx%h", r[:outputs].first).strip
      results[[group, name]] = r
    end
  end
end

host = DARWIN ? sh("sysctl", "-n", "machdep.cpu.brand_string").strip : `lscpu`[/Model name:\s*(.+)/, 1].to_s.strip
puts "\n#{`uname -srm`.strip} | #{host}"
puts "ruby #{RUBY_VERSION} | #{sh("magick", "-version").lines.first.strip}"
puts "fit within #{MAX}x#{MAX}, JPEG q#{QUALITY}, best of #{reps} per image\n"
if TIME
  puts "baseline RSS, gem loaded, nothing processed:"
  baselines.each { |name, b| puts format("  %-24s %6.1f MB", name, b[:rss_mb]) }
else
  puts "peak RSS unavailable: /usr/bin/time is not installed"
end

GROUPS.each_key do |group|
  rows = CONTENDERS.filter_map { |name| [name, results[[group, name]]] if results[[group, name]] }
  next if rows.empty?

  n = sources(group).size
  puts "\n## #{group}  (n=#{n}, out #{rows.first[1][:dims]})"
  puts format("%-24s %9s %9s %10s %9s %12s", "gem", "ms", "cpu s", "peak RSS", "KB", "SSIMULACRA2")
  rows.each do |name, r|
    puts format("%-24s %9.1f %9.2f %10s %9.1f %12s",
      name, r[:ms], r[:cpu], r[:rss_mb] ? format("%.1f MB", r[:rss_mb]) : "-",
      r[:bytes] / 1024.0, r[:ssim2] ? format("%.2f", r[:ssim2]) : "-")
  end
end

File.write(File.join(__dir__, "results-#{DARWIN ? "darwin" : "linux"}.json"), JSON.pretty_generate(
  results.map { |(group, name), r| {group: group, gem: name}.merge(r.reject { |k, _| k == :outputs }) }
))
puts
