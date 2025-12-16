{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/release-25.11";
  };

  outputs =
    {
      nixpkgs,
      ...
    }:
    {
      # Dev.
      devShells =
        nixpkgs.lib.genAttrs
          [
            "aarch64-darwin"
            "aarch64-linux"
            "x86_64-darwin"
            "x86_64-linux"
          ]
          (
            system:
            let
              pkgs = import nixpkgs {
                inherit system;
              };
            in
            {
              default = pkgs.mkShell {
                packages = with pkgs; [
                  nixfmt-rfc-style
                ];
                buildInputs = with pkgs; [
                  udev
                  rust-jemalloc-sys-unprefixed
                ];
                nativeBuildInputs = with pkgs; [
                  pkg-config
                  rustPlatform.bindgenHook
                ];
              };
            }
          );
    };
}
