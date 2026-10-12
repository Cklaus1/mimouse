class Mimouse < Formula
  desc "Software KVM switch: share one mouse and keyboard between computers on a LAN"
  homepage "https://github.com/Cklaus1/mimouse"
  url "https://github.com/Cklaus1/mimouse/archive/refs/tags/v0.11.0.tar.gz"
  sha256 "@SHA256_SOURCE_TARBALL@"
  license "GPL-3.0-or-later"
  head "https://github.com/Cklaus1/mimouse.git", branch: "main"

  depends_on "rust" => :build

  on_linux do
    depends_on "pkgconf" => :build
    depends_on "libx11"
    depends_on "libxtst"
  end

  def install
    system "cargo", "install", *std_cargo_args
    pkgshare.install "config.toml" => "config.toml.example"
  end

  def caveats
    <<~EOS
      Write ~/.config/mimouse/config.toml (example: #{opt_pkgshare}/config.toml.example).
      Open port 4242, UDP and TCP: UDP for input, TCP for pairing and clipboard.
      macOS: grant Accessibility and Input Monitoring to #{opt_bin}/mimouse when prompted;
      macOS asks again after an upgrade because the binary path changes.
    EOS
  end

  service do
    run [opt_bin/"mimouse", "daemon"]
    keep_alive true
    log_path var/"log/mimouse.log"
    error_log_path var/"log/mimouse.log"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/mimouse --version")
  end
end
