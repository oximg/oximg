# frozen_string_literal: true

require "test_helper"

# End-to-end against a real binary: the argv tests pin what the gem
# says, these pin that the CLI still answers it. Skipped when no
# executable is around.
class Oximg::ProcessingTest < Oximg::Test
  def setup
    super
    require_executable!
  end

  def test_fits_within_the_box_without_enlarging
    in_tmpdir do |dir|
      out = File.join(dir, "out.jpg")
      assert_equal out, Oximg.resize(fixture("photo.jpg"), out, width: 100, height: 100)

      probed = Oximg.probe(out)
      assert_equal :jpeg, probed[:format]
      assert_operator probed[:width], :<=, 100
      assert_operator probed[:height], :<=, 100
    end
  end

  def test_a_zero_axis_is_unconstrained
    in_tmpdir do |dir|
      out = File.join(dir, "out.jpg")
      Oximg.resize(fixture("photo.jpg"), out, width: 100)
      assert_equal 100, Oximg.probe(out)[:width]
    end
  end

  def test_transcodes_by_destination_extension
    in_tmpdir do |dir|
      out = File.join(dir, "out.webp")
      Oximg.resize(fixture("photo.jpg"), out, width: 100)
      assert_equal :webp, Oximg.probe(out)[:format]
    end
  end

  def test_an_explicit_format_beats_the_extension
    in_tmpdir do |dir|
      out = File.join(dir, "out.bin")
      Oximg.resize(fixture("photo.jpg"), out, width: 100, format: :png)
      assert_equal :png, Oximg.probe(out)[:format]
    end
  end

  # The compression-only call: no resize, just a re-encode.
  def test_re_encodes_at_the_sources_own_size_by_default
    in_tmpdir do |dir|
      source = Oximg.probe(fixture("photo.jpg"))
      out = File.join(dir, "out.jpg")
      Oximg.resize(fixture("photo.jpg"), out, quality: 60)

      probed = Oximg.probe(out)
      assert_equal [source[:width], source[:height]], [probed[:width], probed[:height]]
      assert_operator File.size(out), :<, File.size(fixture("photo.jpg"))
    end
  end

  def test_probe_reads_headers_only
    probed = Oximg.probe(fixture("photo.jpg"))
    assert_equal "image/jpeg", probed[:content_type]
    assert_operator probed[:width], :>, 0
    assert_operator probed[:height], :>, 0
  end

  # The binary already names what it refused; the gem must surface that
  # rather than a bare exit status.
  def test_surfaces_the_binarys_own_error
    in_tmpdir do |dir|
      error = assert_raises(Oximg::ProcessingError) do
        Oximg.resize(File.join(dir, "missing.jpg"), File.join(dir, "out.jpg"), width: 100)
      end
      assert_match(/missing\.jpg/, error.message)
      refute_nil error.status
    end
  end
end
