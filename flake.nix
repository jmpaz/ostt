{
  description = "OSTT development environment and package";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      lib = nixpkgs.lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f:
        lib.genAttrs systems (system:
          let
            pkgs = import nixpkgs { inherit system; };
          in
          f pkgs
        );
    in
    {
      packages = forAllSystems (pkgs:
        let
          linuxBuildInputs = lib.optionals pkgs.stdenv.isLinux [
            pkgs.alsa-lib
          ];
          darwinBuildInputs = lib.optionals pkgs.stdenv.isDarwin (with pkgs.darwin.apple_sdk.frameworks; [
            AudioToolbox
            AudioUnit
            CoreAudio
            CoreFoundation
          ]);
          runtimePackages = [
            pkgs.ffmpeg
          ] ++ lib.optionals pkgs.stdenv.isLinux [
            pkgs.wl-clipboard
            pkgs.xclip
          ];
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "ostt";
            version = "0.0.5";
            src = ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            nativeBuildInputs = [
              pkgs.makeWrapper
              pkgs.pkg-config
            ];

            buildInputs = linuxBuildInputs ++ darwinBuildInputs;

            postInstall = ''
              wrapProgram "$out/bin/ostt" \
                --prefix PATH : ${lib.makeBinPath runtimePackages}
            '';

            meta = with lib; {
              description = "Open Speech-to-Text recording tool with real-time volume metering and contextualize-driven transcription";
              homepage = "https://github.com/kristoferlund/ostt";
              license = licenses.mit;
              mainProgram = "ostt";
              platforms = platforms.unix;
            };
          };
        }
      );

      devShells = forAllSystems (pkgs:
        let
          linuxBuildInputs = lib.optionals pkgs.stdenv.isLinux [
            pkgs.alsa-lib
          ];
          pkgConfigPath = lib.makeSearchPathOutput "dev" "lib/pkgconfig" linuxBuildInputs;
          shellPackages = [
            pkgs.cargo
            pkgs.clippy
            pkgs.ffmpeg
            pkgs.pkg-config
            pkgs.rust-analyzer
            pkgs.rustc
            pkgs.rustfmt
          ] ++ lib.optionals pkgs.stdenv.isLinux [
            pkgs.wl-clipboard
            pkgs.xclip
          ];
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${pkgs.system}.default ];
            packages = shellPackages;

            shellHook = lib.optionalString pkgs.stdenv.isLinux ''
              export PKG_CONFIG_PATH="${pkgConfigPath}''${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
            '';
          };
        }
      );

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.system}.default}/bin/ostt";
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
