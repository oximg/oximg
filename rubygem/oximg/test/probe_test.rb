# frozen_string_literal: true

require "test_helper"

# What the CLI's probe line turns into, without running anything: the
# counterpart of ArgvTest, for the other direction of the contract. The
# fixtures reach only one of the CLI's loop spellings, so the grammar
# from src/cli.rs is pinned here in full.
class Oximg::ProbeTest < Oximg::Test
  # The parser is private: nothing outside the gem needs it, and a
  # method kept public for tests would be API the gem has to preserve.
  def parse(line)
    Oximg.send(:parse_probe, line)
  end

  def test_reads_a_still_line
    assert_equal({content_type: "image/jpeg", format: :jpeg, width: 200, height: 150},
      parse("/src/photo.jpg: image/jpeg 200x150 (30000 stored pixels)\n"))
  end

  def test_reads_past_the_animation_summary
    assert_equal({content_type: "image/gif", format: :gif, width: 120, height: 90},
      parse("/src/anim.gif: image/gif 120x90 (10800 stored pixels), 3 frames, 1500ms, looping forever\n"))
  end

  # The CLI spells the loop count three ways; the gem must not care
  # which, and no fixture reaches the second or third.
  def test_accepts_every_loop_spelling
    ["looping forever", "playing once", "playing 3 times"].each do |loops|
      line = "/src/a.webp: image/webp 64x48 (3072 stored pixels), 2 frames, 200ms, #{loops}\n"
      assert_equal [64, 48], parse(line).values_at(:width, :height), loops
    end
  end

  # The summary is not parsed, only skipped, so a field the CLI adds
  # after it later does not break probe the way the summary itself did.
  def test_skips_a_field_it_has_never_seen
    line = "/src/a.gif: image/gif 1x1 (1 stored pixels), 3 frames, 1500ms, looping forever, 4 colors\n"
    assert_equal({content_type: "image/gif", format: :gif, width: 1, height: 1}, parse(line))
  end

  # Every content type the CLI emits names a format. GIF is one of them
  # and stays out of the output allowlist: decode-only.
  def test_names_every_content_type_the_cli_emits
    formats = %w[image/jpeg image/png image/webp image/avif image/gif].map do |type|
      parse("/src/a: #{type} 1x1 (1 stored pixels)\n")[:format]
    end
    assert_equal %i[jpeg png webp avif gif], formats
    refute_includes Oximg::FORMATS, :gif
  end

  # A type the gem has never heard of is the binary's call, not an
  # error: content_type is still reported, format is nil.
  def test_reports_an_unknown_content_type_with_a_nil_format
    probed = parse("/src/a.bmp: image/bmp 8x8 (64 stored pixels)\n")
    assert_equal "image/bmp", probed[:content_type]
    assert_nil probed[:format]
  end

  # The guard on the parser itself: output that is not exactly the one
  # line raises rather than returning a half-read hash.
  def test_rejects_output_that_is_not_exactly_the_line
    [
      "oximg 0.11.0\n", # some other command's output
      "",
      "/src/a.jpg: image/jpeg 200x150\n", # no pixel count
      "/src/a.jpg: image/jpeg (30000 stored pixels)\n", # no dimensions
      "/src/a.jpg: image/jpeg 200x150 (30000 stored pixels) 3 frames\n", # continues without the comma
      "/src/a.jpg: image/jpeg 200x150 (30000 stored pixels)\nsecond line\n",
      "oximg listening on :8081\n/src/a.jpg: image/jpeg 200x150 (30000 stored pixels)\n", # a line before it
      "/src/a.jpg: image/jpeg\n200x150 (30000 stored pixels)\n" # the fields split across lines
    ].each do |output|
      error = assert_raises(Oximg::ProcessingError, output.inspect) { parse(output) }
      assert_match(/unparsable probe output/, error.message)
    end
  end
end
