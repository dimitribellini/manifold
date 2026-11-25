cask "manifold" do
  version "0.8.0"
  sha256 "162ec7b66da9cdd7e509acb009e661d76779d64713231868858eebef3ad516ef"

  url "https://github.com/dimitribellini/manifold/releases/download/v#{version}/manifold-v#{version}-macos-arm64.tar.gz"
  name "Manifold"
  desc "Cross-platform dual-pane file manager and remote transfer engine"
  homepage "https://github.com/dimitribellini/manifold"

  depends_on arch: :arm64
  depends_on macos: :big_sur

  app "Manifold.app"

  zap trash: [
    "~/.config/manifold",
    "~/.cache/manifold",
  ]
end
