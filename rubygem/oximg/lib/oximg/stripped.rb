# frozen_string_literal: true

module Oximg
  # What Oximg.strip did: where it wrote, and whether the output
  # displays upright. Built from <tt>oximg strip --json</tt>, so it is
  # the binary's own account of the run, not a second read of the file.
  #
  # It stands in for the destination path Oximg.strip returned before
  # (#176): #to_s, #to_path and #to_str are the path, and it compares
  # equal to that path as a String, so
  # <tt>File.size(Oximg.strip(...))</tt>, <tt>File.join</tt> and
  # <tt>assert_equal out, Oximg.strip(...)</tt> keep working.
  #
  #   result = Oximg.strip(upload, clean, auto_rotate: true)
  #   result.rotated?                #=> false
  #   result.orientation             #=> 6, the tag the output keeps
  #   result.not_rotated.code        #=> :unaligned_edges
  #   result.not_rotated.message     #=> "1601x1203 does not divide into ..."
  class Stripped
    # Why <tt>auto_rotate: true</tt> left a rotated source tagged.
    # +code+ is the contract, a Symbol from NOT_ROTATED_CODES (or
    # :unknown, below); +message+ is for humans and may change.
    NotRotated = Data.define(:code, :message)

    # The binary's codes (docs/features/paths.md). A code missing here
    # still comes through, as its Symbol: a newer binary may add one.
    NOT_ROTATED_CODES = %i[unaligned_edges corrupt over_limit not_jpeg].freeze

    # The destination, absolute.
    attr_reader :path
    # The output's content type ("image/jpeg", ...); nil from an oximg
    # older than 0.20, which has no --json.
    attr_reader :content_type
    # Output size in bytes.
    attr_reader :bytes
    # What was dropped: <tt>[{name: "APP1 Exif", count: 1, bytes: 1204}, ...]</tt>;
    # nil from an oximg older than 0.20.
    attr_reader :removed
    # The orientation tag the output keeps, 2..8, or nil when it
    # displays as stored (upright, untagged, or rotated upright).
    attr_reader :orientation
    # The orientation turned into the pixels, 2..8, or nil.
    attr_reader :rotated
    # A NotRotated, or nil unless auto_rotate left a rotated source
    # tagged. From an oximg older than 0.20 the code is :unknown: the
    # output was probed, so its orientation is known, but not why.
    attr_reader :not_rotated

    # From the hash <tt>oximg strip --json</tt> prints.
    def self.from_json(path, report)
      why = report["not_rotated"]
      new(
        path: path,
        content_type: report["content_type"],
        bytes: report["bytes"],
        removed: report["removed"]&.map { |r| {name: r["name"], count: r["count"], bytes: r["bytes"]} },
        orientation: report["orientation"],
        rotated: report["rotated"],
        not_rotated: why && NotRotated.new(code: why["code"].to_sym, message: why["message"])
      )
    end

    def initialize(path:, bytes:, orientation:, rotated:, not_rotated:, content_type: nil, removed: nil)
      @path = path
      @content_type = content_type
      @bytes = bytes
      @removed = removed
      @orientation = orientation
      @rotated = rotated
      @not_rotated = not_rotated
      freeze
    end

    # True when the pixels were turned upright (auto_rotate).
    def rotated?
      !@rotated.nil?
    end

    # True when the output displays as stored: no orientation tag left,
    # so its stored width and height are the displayed ones.
    def upright?
      @orientation.nil?
    end

    def to_s
      @path
    end
    alias_method :to_path, :to_s
    alias_method :to_str, :to_s

    # Equal to another result for the same run, or to the destination
    # path as a String (String#== defers here for anything with to_str).
    def ==(other)
      case other
      when Stripped then to_h == other.to_h
      when String then @path == other
      else false
      end
    end

    def to_h
      {path: @path, content_type: @content_type, bytes: @bytes, removed: @removed,
       orientation: @orientation, rotated: @rotated, not_rotated: @not_rotated&.to_h}
    end

    def inspect
      "#<#{self.class.name} #{@path} #{to_h.except(:path).compact.inspect}>"
    end
  end
end
