# frozen_string_literal: true

# Quality per byte for the Ruby image-processing gems.
#
# bench.rb compares them at one quality setting, which cannot rank
# encoders: the contenders write different numbers of bytes there, so a
# higher score may be bought rather than earned. This sweeps quality
# instead, and reports what each gem scores *at the same output size* —
# the only comparison that separates the encoder from the setting.
#
# End-to-end, like QUALITY.md's Group B: each gem resizes and encodes
# from the same JPEG source, scored with SSIMULACRA2 against a
# linear-light Lanczos downscale at the same dimensions. So the number
# covers the resize as much as the encode, which is what a Rails app
# actually ships.
#
# One machine is enough: JPEG encoding is deterministic and the outputs
# were byte-identical across all three CPUs in bench.rb.
#
#   bundle exec ruby quality.rb

require "json"
require "open3"
require "tmpdir"

require "oximg"
require "vips"
require "image_processing/vips"
require "mini_magick"

MAX = 750
SWEEP = [60, 70, 75, 80, 85, 90].freeze
CORPUS = File.expand_path("../quality/corpus", __dir__)

Vips.cache_set_max(0)

CONTENDERS = {
  "oximg (jpegli)" => lambda { |src, out, q|
    Oximg.resize(src, out, width: MAX, height: MAX, quality: q)
  },
  "oximg (mozjpeg small)" => lambda { |src, out, q|
    Oximg.resize(src, out, width: MAX, height: MAX, quality: q, preset: :small)
  },
  "ruby-vips" => lambda { |src, out, q|
    Vips::Image.thumbnail(src, MAX, height: MAX, size: :down).jpegsave(out, Q: q)
  },
  "image_processing/vips" => lambda { |src, out, q|
    ImageProcessing::Vips.source(src).convert("jpeg")
      .resize_to_limit(MAX, MAX).saver(quality: q).call(destination: out)
  },
  # image_processing/magick is the same encoder through the same CLI and
  # wrote byte-identical output in bench.rb; one row covers both.
  "mini_magick" => lambda { |src, out, q|
    MiniMagick.convert do |c|
      c << src
      c.resize "#{MAX}x#{MAX}>"
      c.quality q
      c << out
    end
  }
}.freeze

GROUPS = {
  "large 4000x2667" => Dir["#{CORPUS}/large/*.jpg"].sort,
  "medium 2000x1334" => Dir["#{CORPUS}/medium/*.jpg"].sort,
  "kodak 768x512" => Dir["#{CORPUS}/src/*.jpg"].sort.first(8)
}.freeze

def sh(*cmd)
  out, err, status = Open3.capture3(*cmd.map(&:to_s))
  raise "#{cmd.first} failed: #{err}" unless status.success?

  out
end

def score(src, out, refs, dir)
  w, h = sh("magick", "identify", "-format", "%w %h", out).split.map(&:to_i)
  ref = refs[[src, w, h]] ||= begin
    path = File.join(dir, "ref-#{File.basename(src, ".*")}-#{w}x#{h}.png")
    sh("magick", src, "-colorspace", "RGB", "-filter", "Lanczos",
      "-resize", "#{w}x#{h}!", "-colorspace", "sRGB", path)
    path
  end
  sh("ssimulacra2", ref, out).strip.to_f
end

# Linear interpolation on the (bytes, score) curve. Outside a gem's
# measured range the answer is nil rather than an extrapolation: an
# encoder's curve flattens at the top, and pretending otherwise is how
# a sweep gets read as a win it never measured.
def at_size(curve, kb)
  sorted = curve.sort_by { |point| point[:kb] }
  return nil if kb < sorted.first[:kb] || kb > sorted.last[:kb]

  lower = sorted.select { |p| p[:kb] <= kb }.last
  upper = sorted.select { |p| p[:kb] >= kb }.first
  return lower[:ssim2] if lower[:kb] == upper[:kb]

  t = (kb - lower[:kb]) / (upper[:kb] - lower[:kb])
  lower[:ssim2] + t * (upper[:ssim2] - lower[:ssim2])
end

results = {}

Dir.mktmpdir("oximg-quality") do |dir|
  refs = {}
  GROUPS.each do |group, sources|
    next if sources.empty?

    CONTENDERS.each do |name, run|
      SWEEP.each do |q|
        bytes = []
        scores = []
        sources.each do |src|
          out = File.join(dir, "#{name.gsub(/[^a-z]/i, "")}-#{q}-#{File.basename(src)}")
          run.call(src, out, q)
          bytes << File.size(out)
          scores << score(src, out, refs, dir)
        end
        results[[group, name]] ||= []
        results[[group, name]] << {
          q: q,
          kb: bytes.sum / bytes.size / 1024.0,
          ssim2: scores.sum / scores.size
        }
        $stderr.print "."
      end
    end
  end
end
$stderr.puts

puts "\nquality per byte, fit within #{MAX}x#{MAX}, SSIMULACRA2 vs a linear-light Lanczos reference"
puts "sweep: #{SWEEP.join("/")}\n"

GROUPS.each_key do |group|
  rows = CONTENDERS.keys.filter_map { |name| [name, results[[group, name]]] if results[[group, name]] }
  next if rows.empty?

  puts "\n## #{group}"
  puts format("%-24s %s", "gem", SWEEP.map { |q| "q#{q}".rjust(15) }.join)
  rows.each do |name, curve|
    cells = curve.map { |p| format("%6.1fKB %5.1f", p[:kb], p[:ssim2]) }
    puts format("%-24s %s", name, cells.join(" "))
  end

  # Iso-byte comparison at a size every contender actually measured:
  # the largest of the per-gem minimums and the smallest of the maximums
  # bound the shared range; the midpoint of that overlap is the fairest
  # single point to quote.
  lows = rows.map { |_, c| c.map { |p| p[:kb] }.min }.max
  highs = rows.map { |_, c| c.map { |p| p[:kb] }.max }.min
  target = ((lows + highs) / 2).round(1)
  puts "\nSSIMULACRA2 at #{target} KB (interpolated; shared range #{lows.round(1)}-#{highs.round(1)} KB):"
  scored = rows.filter_map { |name, curve| [name, at_size(curve, target)] if at_size(curve, target) }
  best = scored.map(&:last).max
  scored.sort_by { |_, s| -s }.each do |name, s|
    puts format("  %-24s %6.2f  %+.2f", name, s, s - best)
  end
end

File.write(File.join(__dir__, "quality-results.json"), JSON.pretty_generate(
  results.map { |(group, name), curve| {group: group, gem: name, curve: curve} }
))
