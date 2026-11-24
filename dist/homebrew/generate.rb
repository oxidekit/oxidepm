# frozen_string_literal: true

tag, repository, checksum_file = ARGV
abort "usage: generate.rb RELEASE_TAG OWNER/REPO CHECKSUM_FILE" unless checksum_file
abort "invalid release tag" unless tag.match?(/\Av\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?\z/)
abort "invalid repository" unless repository.match?(%r{\A[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\z})

checksums = File.readlines(checksum_file).to_h do |line|
  digest, name = line.split
  [name, digest]
end
homepage = "https://github.com/#{repository}"

asset = lambda do |target|
  name = "oxidepm-#{target}.tar.gz"
  digest = checksums[name]
  abort "missing SHA-256 for #{name}" unless digest&.match?(/\A[a-f0-9]{64}\z/)

  [%{url "#{homepage}/releases/download/#{tag}/#{name}"}, %{sha256 "#{digest}"}].join("\n      ")
end

puts <<~FORMULA
  class Oxidepm < Formula
    desc "Process manager written in Rust with adapter support"
    homepage "#{homepage}"
    version "#{tag.delete_prefix("v")}"
    license any_of: ["MIT", "Apache-2.0"]

    on_macos do
      on_arm do
        #{asset.call("aarch64-apple-darwin")}
      end
      on_intel do
        #{asset.call("x86_64-apple-darwin")}
      end
    end

    on_linux do
      on_arm do
        #{asset.call("aarch64-unknown-linux-gnu")}
      end
      on_intel do
        #{asset.call("x86_64-unknown-linux-gnu")}
      end
    end

    def install
      bin.install "oxidepm", "oxidepmd", "oxidepm-sandbox"
      share.install "share/oxidepm"
      prefix.install "LICENSE-MIT", "LICENSE-APACHE"
    end

    def caveats
      return "macOS supports the built-in cmd runtime. Adapter execution requires Linux." if OS.mac?

      "Import the runtimes with: oxidepm adapter import --bundled --tag runtime --enable --yes"
    end

    test do
      assert_match version.to_s, shell_output("\#{bin}/oxidepm --version")
      assert_path_exists bin/"oxidepm-sandbox"
      assert_path_exists share/"oxidepm/adapters/SHA256SUMS"
    end
  end
FORMULA
