# frozen_string_literal: true

module Oximg
  # The gem version is the version of the oximg it bundles — the same
  # convention the npm package uses. A gem-only fix therefore waits for
  # the next oximg release rather than shipping under a version that
  # names a binary it does not carry. `Oximg.version` reports what the
  # resolved executable actually is, which can differ when it comes
  # from PATH.
  VERSION = "0.11.0"
end
