# frozen_string_literal: true

require "test_helper"
require "stringio"

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

  # Issue #128, end to end: photo.jpg with an Exif orientation 6 and an
  # ICC profile spliced in after SOI. The defaults rotate (150x200) and
  # keep the profile; the overrides do neither, and beat the knobs.
  def test_icc_and_auto_rotate_override_the_environment
    in_tmpdir do |dir|
      source = File.join(dir, "rotated-icc.jpg")
      File.binwrite(source, tagged_photo)
      out = File.join(dir, "out.jpg")

      Oximg.resize(source, out)
      assert_equal [150, 200], Oximg.probe(out).values_at(:width, :height)
      assert_includes File.binread(out), "ICC_PROFILE"

      Oximg.resize(source, out, icc: :strip, auto_rotate: false)
      assert_equal [200, 150], Oximg.probe(out).values_at(:width, :height)
      refute_includes File.binread(out), "ICC_PROFILE"

      with_env("OXIMG_ICC", "0") do
        with_env("OXIMG_AUTO_ROTATE", "0") do
          Oximg.resize(source, out, icc: :keep, auto_rotate: true)
        end
      end
      assert_equal [150, 200], Oximg.probe(out).values_at(:width, :height)
      assert_includes File.binread(out), "ICC_PROFILE"
    end
  end

  # env: reaches the binary, and only for that call: four threads at
  # once, alternating OXIMG_ICC=0, each get their own answer.
  def test_env_applies_to_one_call_only
    in_tmpdir do |dir|
      source = File.join(dir, "rotated-icc.jpg")
      File.binwrite(source, tagged_photo)
      outs = 8.times.map { |i| File.join(dir, "out#{i}.jpg") }
      outs.each_slice(4).each do |batch|
        batch.each_with_index.map do |out, i|
          Thread.new { Oximg.resize(source, out, env: i.even? ? {"OXIMG_ICC" => "0"} : {}) }
        end.each(&:join)
      end
      outs.each_with_index do |out, i|
        assert_equal i.odd?, File.binread(out).include?("ICC_PROFILE"), "call #{i}"
      end
      assert_nil ENV["OXIMG_ICC"]
    end
  end

  # The binary validates knob values the same fail-closed way it does at
  # startup: a bad one is a usage error, exit 2.
  def test_env_values_are_validated_by_the_binary
    in_tmpdir do |dir|
      error = assert_raises(Oximg::ProcessingError) do
        Oximg.resize(fixture("photo.jpg"), File.join(dir, "out.jpg"), env: {"OXIMG_WEBP_QUALITY" => "lots"})
      end
      assert_equal 2, error.status
      assert_match(/OXIMG_WEBP_QUALITY/, error.message)
    end
  end

  def test_strip_takes_env
    in_tmpdir do |dir|
      source = File.join(dir, "rotated-icc.jpg")
      File.binwrite(source, tagged_photo)
      out = File.join(dir, "out.jpg")
      Oximg.strip(source, out, env: {"OXIMG_ICC" => "0"})
      refute_includes File.binread(out), "ICC_PROFILE"
    end
  end

  def test_probe_reads_headers_only
    probed = Oximg.probe(fixture("photo.jpg"))
    assert_equal "image/jpeg", probed[:content_type]
    assert_operator probed[:width], :>, 0
    assert_operator probed[:height], :>, 0
  end

  # The CLI appends ", 3 frames, 1500ms, looping forever" for an
  # animated source. This is the line as the binary really prints it;
  # the other loop spellings, which no fixture reaches, are in
  # ProbeTest.
  def test_probe_reads_an_animated_source
    probed = Oximg.probe(fixture("anim.gif"))
    assert_equal({content_type: "image/gif", format: :gif, width: 120, height: 90}, probed)

    probed = Oximg.probe(fixture("animated.webp"))
    assert_equal({content_type: "image/webp", format: :webp, width: 64, height: 48}, probed)
  end

  # GIF is decode-only, but it is still a content type the CLI emits,
  # so probe has to name it.
  def test_probe_names_gif_as_a_format
    probed = Oximg.probe(fixture("still.gif"))
    assert_equal({content_type: "image/gif", format: :gif, width: 240, height: 180}, probed)
  end

  # Nothing in the fixture to strip: the bytes come back unchanged.
  def test_strip_writes_the_source_without_its_metadata
    in_tmpdir do |dir|
      out = File.join(dir, "out.webp")
      assert_equal out, Oximg.strip(fixture("photo.webp"), out)
      assert_equal File.binread(fixture("photo.webp")), File.binread(out)
    end
  end

  def test_strip_refuses_to_drop_a_cmyk_profile
    in_tmpdir do |dir|
      error = assert_raises(Oximg::ProcessingError) do
        Oximg.strip(fixture("cmyk_icc.jpg"), File.join(dir, "out.jpg"), icc: :strip)
      end
      assert_match(/CMYK/, error.message)
      assert_equal 1, error.status
    end
  end

  # Without the probe library, an IO reaches the executable through a
  # tempfile, and the answer is the path's.
  def test_probe_reads_an_io_through_the_executable
    refute Oximg.in_process_probe?, "this suite drives the executable; NativeTest drives the library"
    probed = Oximg.probe(StringIO.new(File.binread(fixture("photo.jpg"))))
    assert_equal Oximg.probe(fixture("photo.jpg")), probed
  end

  # The same IO the library takes: one that answers #read(n) and no
  # more (IO.copy_stream would pass it a buffer too).
  def test_probe_reads_a_read_n_only_io_through_the_executable
    source = StringIO.new(File.binread(fixture("photo.jpg")))
    io = Object.new
    io.define_singleton_method(:read) { |n| source.read(n) }
    assert_equal Oximg.probe(fixture("photo.jpg")), Oximg.probe(io)
  end

  # Without the library an open File is still what it holds, not what
  # its name leads to by now: the executable reads the name only while
  # it is the same file, and a copy made with positioned reads
  # otherwise. The position is untouched either way.
  def test_probe_reads_an_open_file_through_the_executable
    want = Oximg.probe(fixture("rgb.png"))
    in_tmpdir do |dir|
      path = File.join(dir, "x.png")
      File.binwrite(path, File.binread(fixture("rgb.png")))
      File.open(path, "rb") do |file|
        file.read(100)
        assert_equal want, Oximg.probe(file), "same file"
        File.binwrite(File.join(dir, "new"), File.binread(fixture("photo.jpg")))
        File.rename(File.join(dir, "new"), path)
        assert_equal want, Oximg.probe(file), "name replaced"
        assert_equal 100, file.pos
      end
    end
  end

  # A File made from a descriptor has no name; without the library it
  # is copied with positioned reads.
  def test_probe_reads_a_nameless_file_through_the_executable
    file = File.for_fd(IO.sysopen(fixture("rgb.png"), "rb"))
    assert_nil file.path
    assert_equal Oximg.probe(fixture("rgb.png")), Oximg.probe(file)
  ensure
    file&.close
  end

  # An object that names its file only through #to_path.
  def test_probe_takes_a_to_path_object
    named = Object.new
    path = fixture("photo.jpg")
    named.define_singleton_method(:to_path) { path }
    assert_equal Oximg.probe(path), Oximg.probe(named)
  end

  # photo.jpg with an APP1 Exif (big-endian TIFF, one IFD entry:
  # Orientation = 6) and an APP2 ICC_PROFILE (one chunk of opaque
  # bytes; pass-through never parses it) right after SOI.
  def tagged_photo
    tiff = ["MM", 42, 8, 1, 0x0112, 3, 1, 6, 0, 0].pack("a2nNnnnNnnN")
    segment = ->(marker, body) { [0xFF, marker, body.bytesize + 2].pack("CCn") + body }
    jpeg = File.binread(fixture("photo.jpg"))
    jpeg[0, 2] + segment.call(0xE1, "Exif\0\0".b + tiff) +
      segment.call(0xE2, "ICC_PROFILE\0".b + [1, 1].pack("CC") + ("\x5A".b * 600)) + jpeg[2..]
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
