# frozen_string_literal: true

require_relative "lib/oximg/rails/version"

Gem::Specification.new do |spec|
  spec.name = "oximg-rails"
  spec.version = Oximg::Rails::VERSION
  spec.authors = ["oximg contributors"]
  spec.summary = "Rails integration for oximg, and URL building for a remote oximg server"
  spec.description = <<~DESC.tr("\n", " ").strip
    The Rails-facing half of oximg: builds and HMAC-signs URLs for a
    remote oximg server, for apps that let the server do the work
    instead of processing locally with the `oximg` gem. ActiveStorage
    glue is in progress.
  DESC
  spec.homepage = "https://github.com/oximg/oximg"
  spec.license = "Apache-2.0"
  spec.required_ruby_version = ">= 3.1"

  spec.metadata = {
    "homepage_uri" => spec.homepage,
    "source_code_uri" => "https://github.com/oximg/oximg/tree/main/rubygem/oximg-rails",
    "changelog_uri" => "https://github.com/oximg/oximg/blob/main/CHANGELOG.md",
    "bug_tracker_uri" => "https://github.com/oximg/oximg/issues",
    "documentation_uri" => "https://github.com/oximg/oximg/blob/main/rubygem/oximg-rails/README.md",
    "rubygems_mfa_required" => "true"
  }

  spec.files = Dir["lib/**/*.rb"] + ["README.md", "LICENSE"]
  spec.require_paths = ["lib"]

  # Versions move in lockstep with the oximg release they name; no
  # Rails dependency is declared until there is Rails code to support.
  spec.add_dependency "oximg", "~> #{Oximg::Rails::VERSION}"
end
