{
  description = "open-recorder — native zero-copy game clipper for Linux (NVIDIA-first)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  nixConfig = {
    extra-substituters = [
      "https://0xfell.cachix.org"
      "https://nix-community.cachix.org"
    ];
    extra-trusted-public-keys = [
      "0xfell.cachix.org-1:0VSPKbe/Eilt+WTT/0faSQeQnnhDOH7PxkUvoRtvPPo="
      "nix-community.cachix.org-1:mB9FSh9qf2dCimDSUo8Zy7bkq5CX+/rkCWyvRCYg3Fs="
    ];
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      # Packages:
      #   ord-cli  — the `ord` control client (pure Rust, builds anywhere).
      #   ordd     — the daemon. The default build uses the mock backend so it
      #              builds without a GPU; the real recorder is built with the
      #              `waycap` feature in the devshell (needs CUDA/PipeWire). We
      #              expose the buildable default here and document the GPU build.
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          common = {
            version = "0.1.0";
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
              # The workspace lockfile carries waycap-rs and its forked pipewire-rs
              # as git deps (used by ord-core's optional `waycap` feature). Vendoring
              # the lock requires their NAR hashes even for the pure CLI build.
              outputHashes = {
                "waycap-rs-3.0.0" = "sha256-jUfzvOkl7bcCiFs4wBjvpuzE9t5nHuP6O+JCKRadEQo=";
                "libspa-0.9.2" = "sha256-eqHVfGpjsfXouGOwBh306/E8g0jQIE5w6cZ5a8TbOIQ=";
              };
            };
          };
        in
        rec {
          ord-cli = pkgs.rustPlatform.buildRustPackage (common // {
            pname = "ord-cli";
            cargoBuildFlags = [ "-p" "ord-cli" ];
            cargoTestFlags = [ "-p" "ord-cli" "-p" "ord-common" "-p" "ord-core" ];
            # CLI + pure crates have no system deps.
            doCheck = true;
            meta = {
              description = "open-recorder control CLI (ord)";
              mainProgram = "ord";
              license = nixpkgs.lib.licenses.mit;
              platforms = systems;
            };
          });
          default = ord-cli;
        });

      # Home Manager module: installs the CLI and (optionally) runs ordd as a
      # user service. The daemon package itself is supplied by the user (the GPU
      # build from the devshell) via `services.ordd.package`.
      homeManagerModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.programs.open-recorder;
        in
        {
          options.programs.open-recorder = {
            enable = lib.mkEnableOption "open-recorder clip recorder";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.ord-cli;
              defaultText = lib.literalExpression "open-recorder.packages.\${system}.ord-cli";
              description = "The `ord` CLI package to install.";
            };
            daemon = {
              enable = lib.mkEnableOption "the ordd user service";
              package = lib.mkOption {
                type = lib.types.nullOr lib.types.package;
                default = null;
                description = ''
                  The `ordd` daemon package (built with the `waycap` feature from
                  the project devshell). Required if `daemon.enable` is true.
                '';
              };
            };
          };
          config = lib.mkIf cfg.enable {
            home.packages = [ cfg.package ]
              ++ lib.optional (cfg.daemon.enable && cfg.daemon.package != null) cfg.daemon.package;

            systemd.user.services.ordd = lib.mkIf cfg.daemon.enable {
              Unit = {
                Description = "open-recorder capture daemon";
                After = [ "graphical-session.target" ];
                PartOf = [ "graphical-session.target" ];
              };
              Service = {
                ExecStart = "${cfg.daemon.package}/bin/ordd";
                Restart = "on-failure";
                RestartSec = 3;
              };
              Install.WantedBy = [ "graphical-session.target" ];
            };
          };
        };

      # Development shell with the native toolchain to build the full workspace
      # incl. the `waycap` (NVENC) + `mux` (ffmpeg) features and the spike.
      devShells = forAllSystems (system:
        let
          # NOTE: we deliberately do NOT set config.cudaSupport. NVENC is
          # provided by the running NVIDIA driver (/run/opengl-driver), not by a
          # CUDA-rebuilt ffmpeg. Turning on cudaSupport would force a from-source
          # rebuild of ffmpeg-full + opencv + whisper + cublas/cufft. We only
          # need ffmpeg's *libraries* for ffmpeg-next to link against, plus the
          # nv-codec-headers, which the cached ffmpeg-full already carries.
          pkgs = import nixpkgs {
            inherit system;
            config.allowUnfree = true;
          };

          # Native libraries waycap-rs and its deps (ffmpeg-next, pipewire,
          # cust, khronos-egl, glutin, gl) link/dlopen at build and run time.
          nativeLibs = with pkgs; [
            pipewire
            wayland
            wayland-protocols
            libdrm
            ffmpeg-full
            libGL
            libglvnd
            mesa
            dbus       # portal screencast (libdbus-sys)
          ];

          # The waycap-rs `nvidia` feature pulls in `cust`, whose build script
          # (`find_cuda_helper`) wants a *unified* CUDA toolkit with `lib64/` +
          # `include/`, located via CUDA_PATH/CUDA_ROOT. The split cudaPackages
          # don't present that layout, so we use the merged `cudatoolkit`
          # derivation. Runtime libcuda still comes from the driver
          # (/run/opengl-driver). This does NOT rebuild ffmpeg (no global
          # config.cudaSupport).
          cudatoolkit = pkgs.cudaPackages.cudatoolkit;
        in
        {
          default = pkgs.mkShell {
            name = "open-recorder-dev";

            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              rust-analyzer

              # build glue for *-sys crates (bindgen, pkg-config)
              pkg-config
              clang
              llvmPackages.libclang

              # diagnostics used by the spike + golden tests
              ffmpeg-full
              libva-utils # vainfo
              cudatoolkit
            ] ++ nativeLibs;

            # bindgen (ffmpeg-next / cust) needs libclang at runtime.
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

            # NVENC/CUDA live in the driver tree, not nixpkgs. Point the linker
            # and dlopen path at the running NVIDIA driver + the toolchain libs.
            shellHook = ''
              export LD_LIBRARY_PATH="/run/opengl-driver/lib:${pkgs.lib.makeLibraryPath nativeLibs}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

              # cust's build script (find_cuda_helper 0.2) on Linux only accepts a
              # CUDA root whose `lib64/` exists; the nixpkgs merged toolkit uses
              # `lib/`. Build a tiny shim dir with `lib64` -> toolkit `lib` and a
              # `stubs` subdir, and point CUDA_LIBRARY_PATH at it. The real libcuda
              # used at runtime is the driver's, via LD_LIBRARY_PATH above.
              export CUDA_PATH="${cudatoolkit}"
              export CUDA_ROOT="${cudatoolkit}"
              _cuda_shim="$PWD/.cuda-shim"
              mkdir -p "$_cuda_shim"
              ln -sfn "${cudatoolkit}/lib" "$_cuda_shim/lib64"
              export CUDA_LIBRARY_PATH="$_cuda_shim"
              export PKG_CONFIG_PATH="${pkgs.lib.makeSearchPathOutput "dev" "lib/pkgconfig" nativeLibs}''${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
              echo "open-recorder devshell — NVENC via /run/opengl-driver, ffmpeg $(${pkgs.ffmpeg-full}/bin/ffmpeg -version | head -1 | cut -d' ' -f3)"
            '';
          };
        });
    };
}
