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
      # Phase-1 spike: a development shell with the native toolchain needed to
      # build waycap-rs (PipeWire DMA-BUF capture + ffmpeg/NVENC encode). The
      # workspace packages are added in later phases; for now this shell is what
      # the spike binary (spike/) builds in.
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
