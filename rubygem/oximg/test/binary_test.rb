# frozen_string_literal: true

require "test_helper"

class Oximg::BinaryTest < Oximg::Test
  def test_prefers_oximg_bin_over_everything_else
    require_executable!
    Oximg::Binary.reset!
    with_env("OXIMG_BIN", executable) do
      assert_equal executable, Oximg.executable
    end
  end

  # A configured-but-wrong path must not quietly fall through to PATH:
  # that is how a process ends up running a different build than the one
  # it was told to run.
  def test_a_broken_oximg_bin_raises_rather_than_falling_back
    with_env("OXIMG_BIN", "/nonexistent/oximg") do
      error = assert_raises(Oximg::ExecutableNotFound) { Oximg.executable }
      assert_match(/OXIMG_BIN/, error.message)
    end
  end

  def test_an_explicit_assignment_overrides_the_environment
    require_executable!
    with_env("OXIMG_BIN", "/nonexistent/oximg") do
      Oximg.executable = executable
      assert_equal executable, Oximg.executable
    end
  end

  def test_reports_when_no_executable_can_be_found
    with_env("OXIMG_BIN", nil) do
      with_env("PATH", "/nonexistent") do
        Oximg::Binary.reset!
        refute_predicate Oximg, :available?
        error = assert_raises(Oximg::ExecutableNotFound) { Oximg.executable }
        assert_match(/not found/, error.message)
      end
    end
  end

  # The env reaches the child and only the child: a fake executable
  # prints what it sees, and the parent's ENV is unchanged afterwards.
  def test_run_passes_env_to_the_child_only
    in_tmpdir do |dir|
      fake = File.join(dir, "oximg")
      File.write(fake, "#!/bin/sh\nprintf '%s|%s' \"${OXIMG_ICC-unset}\" \"${OXIMG_AUTO_ROTATE-unset}\"\n")
      File.chmod(0o755, fake)
      Oximg.executable = fake
      with_env("OXIMG_AUTO_ROTATE", "1") do
        out, = Oximg::Binary.run("x", env: {"OXIMG_ICC" => "0", "OXIMG_AUTO_ROTATE" => nil})
        assert_equal "0|unset", out
        assert_nil ENV["OXIMG_ICC"]
        assert_equal "1", ENV["OXIMG_AUTO_ROTATE"]
        out, = Oximg::Binary.run("x")
        assert_equal "unset|1", out
      end
    end
  end

  def test_reports_the_executables_own_version
    require_executable!
    assert_match(/\A\d+\.\d+\.\d+/, Oximg.version)
  end
end
