{
  description = "A fast terminal UI for anime: browse, stream and download through ani-cli, with AniList sync";

  # unstable, because ani-hop needs ani-cli 5.1 and NixOS 26.05 has 5.0
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      forAll = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
    in
    {
      packages = forAll (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib stdenv;
          # nixpkgs' wrapper puts its ffmpeg first on ani-cli's PATH, ahead of the ffmpeg
          # ani-hop puts there to download shows whose audio is a separate track
          ani-cli = pkgs.ani-cli.overrideAttrs { installPhase = "install -Dm755 ani-cli $out/bin/ani-cli"; };
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "ani-hop";
            inherit ((lib.importTOML ./Cargo.toml).package) version;
            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.makeWrapper ];
            # the tests stop downloads with pkill, which nixpkgs only has for Linux; on macOS the
            # one that needs it runs in CI's cargo test, with the system's
            nativeCheckInputs = lib.optional stdenv.hostPlatform.isLinux pkgs.procps;
            checkFlags = lib.optional stdenv.hostPlatform.isDarwin "--skip=ani::tests::fake_ffmpeg_renames_only_finished_downloads";
            # ani-cli's tools as nixpkgs picks them; the player and xdg-open last, so the
            # user's own (an mpv with scripts, say) wins
            postInstall = ''
              wrapProgram $out/bin/ani-hop \
                --prefix PATH : ${
                  lib.makeBinPath (
                    [
                      ani-cli
                      pkgs.ani-skip
                      pkgs.yt-dlp
                      pkgs.ffmpeg
                      pkgs.curl
                      pkgs.curl-impersonate
                      pkgs.gnugrep
                      pkgs.gnused
                    ]
                    ++ lib.optional stdenv.hostPlatform.isLinux pkgs.procps
                  )
                } \
                --suffix PATH : ${
                  lib.makeBinPath ([ pkgs.mpv ] ++ lib.optional stdenv.hostPlatform.isLinux pkgs.xdg-utils)
                }
            '';
            meta = {
              description = "Fast terminal UI for anime: browse, stream and download through ani-cli, with AniList sync";
              homepage = "https://github.com/ReynkeDeVos/ani-hop";
              license = lib.licenses.gpl3Plus;
              mainProgram = "ani-hop";
              platforms = lib.platforms.unix;
            };
          };
        }
      );
    };
}
