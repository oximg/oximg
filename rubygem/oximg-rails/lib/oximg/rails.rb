# frozen_string_literal: true

require "oximg"
require_relative "rails/version"
require_relative "server"

module Oximg
  # Rails integration for oximg, split from the `oximg` gem the way
  # `imgproxy-rails` is split from `imgproxy`: the base gem stays
  # framework-free, and everything that reaches into Rails lives here.
  #
  # What is here today is the server-URL builder (Oximg::Server), which
  # this gem owns. The ActiveStorage glue — an `ImageProcessing::Oximg`
  # variant processor for local processing, and attachment URL helpers
  # for the remote mode — is not implemented yet; see the README.
  #
  # Note for anything added here: inside this namespace a bare `Rails`
  # resolves to *this* module, not the framework. Always write
  # `::Rails`.
  module Rails
  end
end
