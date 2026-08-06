# frozen_string_literal: true

require_relative "lib/oximg/version"

Gem::Specification.new do |spec|
  spec.name = "oximg"
  spec.version = Oximg::VERSION
  spec.authors = ["oximg contributors"]
  spec.summary = "Image compression and resizing for Ruby, without libvips or ImageMagick"
  spec.description = <<~DESC.tr("\n", " ").strip
    Ruby bindings for oximg, the high-performance Rust image
    compression and resizing engine: JPEG, PNG, WebP and AVIF, resized
    in linear light. Platform gems bundle the executable, so there is
    no system library to install.
  DESC
  spec.homepage = "https://github.com/oximg/oximg"
  spec.license = "Apache-2.0"
  spec.required_ruby_version = ">= 3.1"

  # Platform gems are built by dropping the matching release binary into
  # exe/ and setting this (see the rubygems job in
  # .github/workflows/release.yml). Unset builds the plain-Ruby gem,
  # which resolves a binary from OXIMG_BIN or PATH instead. The Linux
  # platforms are spelled "-gnu" deliberately: an unqualified
  # "x86_64-linux" gem also installs on musl, where a glibc binary
  # cannot run.
  platform = ENV["OXIMG_GEM_PLATFORM"].to_s
  spec.platform = platform unless platform.empty?

  spec.metadata = {
    "homepage_uri" => spec.homepage,
    "source_code_uri" => "https://github.com/oximg/oximg/tree/main/rubygem/oximg",
    "changelog_uri" => "https://github.com/oximg/oximg/blob/main/CHANGELOG.md",
    "bug_tracker_uri" => "https://github.com/oximg/oximg/issues",
    "documentation_uri" => "https://github.com/oximg/oximg/blob/main/rubygem/oximg/README.md",
    "rubygems_mfa_required" => "true"
  }

  # An explicit list, not `git ls-files`: the gem lives in a
  # subdirectory of the oximg repo, where git would hand back the whole
  # Rust tree. exe/ is empty here and populated at package time for the
  # platform gems — and there is deliberately no spec.executables, since
  # RubyGems would wrap a native binary in a Ruby binstub. Ask
  # Oximg.executable for the path instead.
  spec.files = Dir["lib/**/*.rb"] + Dir["exe/oximg*"] + ["README.md", "LICENSE"]
  spec.require_paths = ["lib"]
end
