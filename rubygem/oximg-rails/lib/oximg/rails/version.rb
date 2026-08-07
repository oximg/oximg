# frozen_string_literal: true

module Oximg
  # Note for anything added here later: inside this namespace, a bare
  # `Rails` resolves to *this* module, not the framework. Always write
  # `::Rails` for the framework.
  module Rails
    # Tracks the oximg release whose URL grammar the client speaks, the
    # same convention as the oximg gem and the npm package.
    VERSION = "0.11.0"
  end
end
