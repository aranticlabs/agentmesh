class Agentmesh < Formula
  desc "Synchronize project-level AI runtime context"
  homepage "https://agentmesh.sh"
  url "https://github.com/aranticlabs/agentmesh.git",
      tag:      "agentmesh-cli-v0.1.0",
      revision: "057c0bb0d39949a066b859d399db6d9bab866430"
  license "MIT"
  head "https://github.com/aranticlabs/agentmesh.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", "--locked", "--path", "crates/agentmesh-cli", "--root", prefix
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/agentmesh --version")
  end
end
