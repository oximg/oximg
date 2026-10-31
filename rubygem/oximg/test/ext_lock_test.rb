# frozen_string_literal: true

require "test_helper"

# The probe library (ext/) is its own Cargo project with its own lock.
# The point of shipping it is one implementation, so it must build the
# decoders the executable ships: every package its lock pins must be
# pinned at the same version in the root lock. After a root dependency
# change, refresh it with
#
#   cp Cargo.lock rubygem/oximg/ext/Cargo.lock && rake compile
#
# (cargo drops what the library does not use).
class Oximg::ExtLockTest < Oximg::Test
  ROOT = File.expand_path("../../..", __dir__)

  def packages(lock)
    File.read(lock).scan(/^\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"/)
  end

  def test_pins_the_root_locks_versions
    ext = packages(File.join(ROOT, "rubygem/oximg/ext/Cargo.lock"))
    root = packages(File.join(ROOT, "Cargo.lock"))
    assert_operator ext.size, :>, 50, "the lock parsed"
    drift = ext - root - [%w[oximg-probe 0.0.0]]
    assert_empty drift, "ext/Cargo.lock pins versions the root lock does not"
  end
end
