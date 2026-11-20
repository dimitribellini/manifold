cask "manifold" do
  version "0.7.8"
  sha256 "b4e57693a4ed8ac184220830e75b90be89bbc7b46eaf0800e9d2b7f92ab43050"

  url "https://github.com/dimitribellini/manifold/releases/download/v#{version}/manifold-v#{version}-macos-arm64.tar.gz"
  name "Manifold"
  desc "Cross-platform dual-pane file manager and remote transfer engine"
  homepage "https://github.com/dimitribellini/manifold"

  depends_on arch: :arm64
  depends_on macos: ">= :big_sur"

  app "Manifold.app"

  zap trash: [
    "~/.config/manifold",
    "~/.cache/manifold",
  ]
end
