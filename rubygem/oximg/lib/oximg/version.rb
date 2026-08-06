# frozen_string_literal: true

module Oximg
  # The gem version tracks the oximg release whose URL grammar this
  # client speaks — the same convention the npm package uses. A gem-only
  # fix therefore waits for the next oximg release rather than shipping
  # under a version that names a grammar it does not match.
  VERSION = "0.10.1"
end
