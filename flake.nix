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
      # Packages — all prebuilt and pushed to the 0xfell cachix cache by CI, so
      # NixOS users never compile:
      #   ord-cli  — the `ord` control client (pure Rust).
      #   ordd     — the daemon, real NVENC recorder (`waycap` + `mux` features).
      #   ord-hud  — the wlr-layer-shell HUD (`layershell` feature).
      #   ord-ui   — the egui clip library window (`gui` feature).
      #   default  — a single output bundling all four binaries.
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            config.allowUnfree = true; # CUDA (for the waycap NVENC build)
          };
          lib = nixpkgs.lib;

          cargoLock = {
            lockFile = ./Cargo.lock;
            # The workspace lockfile carries waycap-rs and its forked pipewire-rs
            # as git deps (ord-core's `waycap` feature). Vendoring needs their
            # NAR hashes even for the pure CLI build.
            outputHashes = {
              "waycap-rs-3.0.0" = "sha256-jUfzvOkl7bcCiFs4wBjvpuzE9t5nHuP6O+JCKRadEQo=";
              "libspa-0.9.2" = "sha256-eqHVfGpjsfXouGOwBh306/E8g0jQIE5w6cZ5a8TbOIQ=";
            };
          };

          # Native libraries the GPU/Wayland builds link or dlopen.
          nativeLibs = with pkgs; [
            pipewire
            wayland
            wayland-protocols
            libdrm
            ffmpeg-full
            libGL
            libglvnd
            mesa
            dbus
            libxkbcommon
          ];

          # cust's build script (find_cuda_helper) wants a unified CUDA toolkit
          # with `lib64/`; the merged derivation provides include/ + lib/.
          cudatoolkit = pkgs.cudaPackages.cudatoolkit;

          # Common build environment for the native (GPU/Wayland) packages.
          nativeEnv = {
            nativeBuildInputs = with pkgs; [ pkg-config rustPlatform.bindgenHook makeWrapper ];
            buildInputs = nativeLibs ++ [ cudatoolkit ];
            # bindgen (ffmpeg-next/cust) needs libclang.
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            # cust/find_cuda_helper: present the toolkit with a lib64/ layout.
            CUDA_PATH = "${cudatoolkit}";
            CUDA_ROOT = "${cudatoolkit}";
            preBuild = ''
              export CUDA_LIBRARY_PATH="$TMPDIR/.cuda-shim"
              mkdir -p "$CUDA_LIBRARY_PATH"
              ln -sfn "${cudatoolkit}/lib" "$CUDA_LIBRARY_PATH/lib64"
            '';
            # NVENC/CUDA libs (libcuda.so.1, libnvidia-encode.so.1) are NOT in
            # nixpkgs — they ship with the running NVIDIA driver. Wrap each binary
            # so /run/opengl-driver/lib (the NixOS driver tree) plus the linked
            # native libs are on LD_LIBRARY_PATH at runtime.
            postFixup = ''
              for b in $out/bin/*; do
                if [ -f "$b" ] && [ -x "$b" ]; then
                  wrapProgram "$b" \
                    --prefix LD_LIBRARY_PATH : "/run/opengl-driver/lib:${lib.makeLibraryPath nativeLibs}"
                fi
              done
            '';
          };

          mkPkg = { pname, crate, features ? [], native ? false, mainProgram, description }:
            pkgs.rustPlatform.buildRustPackage (
              (lib.optionalAttrs native nativeEnv) // {
                inherit pname cargoLock;
                version = "0.1.0";
                src = ./.;
                cargoBuildFlags = [ "-p" crate ]
                  ++ lib.optionals (features != []) [ "--features" (lib.concatStringsSep "," features) ];
                # The GPU/Wayland features need a live session/GPU to test; skip
                # tests in the package build (CI runs the pure test suite).
                doCheck = !native;
                cargoTestFlags = lib.optionals (!native) [ "-p" crate ];
                meta = {
                  inherit description mainProgram;
                  license = lib.licenses.mit;
                  platforms = systems;
                };
              }
            );
        in
        rec {
          ord-cli = mkPkg {
            pname = "ord-cli";
            crate = "ord-cli";
            mainProgram = "ord";
            description = "open-recorder control CLI (ord)";
          };

          ordd = mkPkg {
            pname = "ordd";
            crate = "ord-daemon";
            features = [ "waycap" ]; # implies mux
            native = true;
            mainProgram = "ordd";
            description = "open-recorder capture daemon (NVENC)";
          };

          ord-hud = mkPkg {
            pname = "ord-hud";
            crate = "ord-overlay";
            features = [ "layershell" ];
            native = true;
            mainProgram = "ord-hud";
            description = "open-recorder wlr-layer-shell HUD";
          };

          ord-ui = mkPkg {
            pname = "ord-ui";
            crate = "ord-ui";
            features = [ "gui" ];
            native = true;
            mainProgram = "ord-ui";
            description = "open-recorder egui clip library";
          };

          # Bundle all binaries into one output for easy install.
          default = pkgs.symlinkJoin {
            name = "open-recorder-0.1.0";
            paths = [ ord-cli ordd ord-hud ord-ui ];
            meta = {
              description = "open-recorder — native NVENC game clipper (all binaries)";
              license = lib.licenses.mit;
              platforms = systems;
            };
          };
        });

      # Home Manager module: installs all open-recorder binaries (prebuilt from
      # the cache) and optionally runs ordd + the HUD as user services.
      homeManagerModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.programs.open-recorder;
          pkgsFor = self.packages.${pkgs.stdenv.hostPlatform.system};
        in
        {
          options.programs.open-recorder = {
            enable = lib.mkEnableOption "open-recorder clip recorder";
            package = lib.mkOption {
              type = lib.types.package;
              default = pkgsFor.default;
              defaultText = lib.literalExpression "open-recorder.packages.\${system}.default";
              description = "open-recorder package bundle (ord, ordd, ord-hud, ord-ui).";
            };
            daemon.enable =
              lib.mkEnableOption "the ordd capture daemon user service" // { default = true; };
            hud.enable =
              lib.mkEnableOption "the ord-hud overlay user service" // { default = true; };
          };
          config = lib.mkIf cfg.enable {
            home.packages = [ cfg.package ];

            systemd.user.services.ordd = lib.mkIf cfg.daemon.enable {
              Unit = {
                Description = "open-recorder capture daemon";
                After = [ "graphical-session.target" ];
                PartOf = [ "graphical-session.target" ];
              };
              Service = {
                ExecStart = "${cfg.package}/bin/ordd";
                Restart = "on-failure";
                RestartSec = 3;
              };
              Install.WantedBy = [ "graphical-session.target" ];
            };

            systemd.user.services.ord-hud = lib.mkIf cfg.hud.enable {
              Unit = {
                Description = "open-recorder HUD overlay";
                After = [ "graphical-session.target" "ordd.service" ];
                PartOf = [ "graphical-session.target" ];
              };
              Service = {
                ExecStart = "${cfg.package}/bin/ord-hud";
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
            dbus           # portal screencast (libdbus-sys)
            libxkbcommon   # smithay-client-toolkit (layer-shell HUD)
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
