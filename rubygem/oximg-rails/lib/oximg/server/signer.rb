# frozen_string_literal: true

require "openssl"

module Oximg
  module Server
    # The imgproxy-style signing scheme, exactly as the server verifies it
    # (+Signing::verify+ in src/main.rs): the signature is
    # +base64url(HMAC-SHA256(key, salt || path))+ over the percent-DECODED
    # path, so one signature covers every URL encoding of the same source.
    #
    # test/server_signer_test.rb pins this against the vectors the Rust
    # integration suite serves 200s for — a scheme with two
    # implementations needs shared vectors, not two readings of the prose.
    module Signer
      module_function

      # +path+ is the decoded path the server reconstructs before
      # verifying: "/resize/{w}/{h}/{file}" on the positional route,
      # "{prefix}/{options}/{file}" on the options route.
      def sign(key, salt, path)
        # Force binary before concatenating: key material is arbitrary
        # bytes, and a non-ASCII filename in a UTF-8 path would otherwise
        # raise Encoding::CompatibilityError on the join.
        digest = OpenSSL::HMAC.digest("SHA256", key, salt + path.to_s.b)
        # Unpadded base64url. The server strips "=" before decoding, so
        # padding would verify too — but every vector it ships is
        # unpadded, and an unpadded signature is one path segment with no
        # escaping question.
        [digest].pack("m0").tr("+/", "-_").delete("=")
      end
    end
  end
end
