# frozen_string_literal: true

# Loads a built probe library and probes known fixtures in-process,
# by path, by open File and by StringIO. The release runs it on a bare
# image of each platform before the library goes into a gem: it proves
# the library loads with nothing but the ffi gem and libc around it,
# which the gem suite, run where the library was built, cannot.
#
#   ruby -I rubygem/oximg/lib rubygem/oximg/ext/smoke.rb <library> <fixtures dir>

require "stringio"

library, fixtures = ARGV
abort "usage: smoke.rb <library> <fixtures dir>" unless library && fixtures

ENV["OXIMG_PROBE_LIB"] = library
require "oximg"
abort "#{library} did not load" unless Oximg.in_process_probe?

want = {
  "photo.jpg" => [:jpeg, 200, 150],
  "cmyk_icc.jpg" => [:jpeg, 64, 48],
  "rgb.png" => [:png, 200, 150],
  "photo.webp" => [:webp, 200, 150],
  "anim.gif" => [:gif, 120, 90]
}
want.each do |name, expected|
  path = File.join(fixtures, name)
  got = [
    Oximg.probe(path),
    File.open(path, "rb") { |file| Oximg.probe(file) },
    Oximg.probe(StringIO.new(File.binread(path)))
  ].map { |probed| probed.values_at(:format, :width, :height) }
  abort "#{name}: #{got.inspect}, want #{expected.inspect}" unless got.uniq == [expected]
end
# The whole-source read (animation: true), by the same three routes.
anim = File.join(fixtures, "anim.gif")
summary = [{frames: 3, duration_ms: 1500, loop_count: 0, decode_work: 32_400}, true]
got = [
  Oximg.probe(anim, animation: true),
  File.open(anim, "rb") { |file| Oximg.probe(file, animation: true) },
  Oximg.probe(StringIO.new(File.binread(anim)), animation: true)
].map { |probed| probed.values_at(:animation, :complete) }
abort "anim.gif animation: #{got.inspect}, want #{summary.inspect}" unless got.uniq == [summary]
puts "ok: #{File.basename(library)} probes in-process on #{RUBY_PLATFORM} (ruby #{RUBY_VERSION})"
