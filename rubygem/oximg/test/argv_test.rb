# frozen_string_literal: true

require "test_helper"

# What a Ruby call turns into, without running anything. The CLI's
# grammar is the contract between the two halves, so it is pinned here
# rather than only observed through end-to-end behaviour.
class Oximg::ArgvTest < Oximg::Test
  def argv(*args, **options)
    Oximg.resize_argv(*args, **options)
  end

  def test_maps_a_resize_onto_the_cli_grammar
    assert_equal ["resize", "/src/a.jpg", "750", "500", "/dst/a.jpg"],
      argv("/src/a.jpg", "/dst/a.jpg", width: 750, height: 500)
  end

  # 0 is the unconstrained axis, and 0 x 0 is the CLI's "re-encode at
  # the source's own size" — compression without a resize.
  def test_defaults_to_a_pure_re_encode
    assert_equal ["resize", "/src/a.jpg", "0", "0", "/dst/a.jpg"],
      argv("/src/a.jpg", "/dst/a.jpg")
  end

  def test_passes_the_encode_knobs
    assert_equal ["resize", "/src/a.jpg", "750", "0", "/dst/a.webp",
      "-q", "80", "-f", "webp", "--preset", "small"],
      argv("/src/a.jpg", "/dst/a.webp", width: 750, quality: 80, format: :webp, preset: :small)
  end

  def test_accepts_string_and_symbol_tokens
    assert_includes argv("/a.jpg", "/b.png", format: "PNG"), "png"
    assert_includes argv("/a.jpg", "/b.jpg", preset: "jpegli"), "jpegli"
  end

  # An uploaded file is rarely named by the person calling this, and a
  # relative "-q.jpg" would reach the CLI's parser as a flag.
  def test_expands_paths_so_a_filename_cannot_read_as_a_flag
    in_tmpdir do |dir|
      Dir.chdir(dir) do
        assert_equal ["resize", File.join(Dir.pwd, "-q.jpg"), "0", "0", File.join(Dir.pwd, "out.jpg")],
          argv("-q.jpg", "out.jpg")
      end
    end
  end

  def test_accepts_anything_that_answers_to_path
    file = Struct.new(:path).new("/src/a.jpg")
    assert_equal "/src/a.jpg", argv(file, "/dst/a.jpg")[1]
  end

  def test_rejects_dimensions_that_are_not_non_negative_integers
    assert_raises(ArgumentError) { argv("/a.jpg", "/b.jpg", width: -1) }
    assert_raises(ArgumentError) { argv("/a.jpg", "/b.jpg", height: "500") }
    assert_raises(ArgumentError) { argv("/a.jpg", "/b.jpg", width: 750.0) }
  end

  def test_rejects_knobs_the_cli_would_refuse
    assert_raises(ArgumentError) { argv("/a.jpg", "/b.jpg", quality: 0) }
    assert_raises(ArgumentError) { argv("/a.jpg", "/b.jpg", quality: 101) }
    assert_raises(ArgumentError) { argv("/a.jpg", "/b.jpg", format: :gif) }
    assert_raises(ArgumentError) { argv("/a.jpg", "/b.jpg", preset: :tiny) }
  end

  def test_rejects_empty_paths
    assert_raises(ArgumentError) { argv("", "/b.jpg") }
    assert_raises(ArgumentError) { argv("/a.jpg", "  ") }
  end
end
