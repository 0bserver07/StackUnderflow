{
  description = "A local-first knowledge base for your AI coding sessions";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        pp = pkgs.python3Packages;

        # ── Frontend (npm build) ────────────────────────────────────
        #   vite.config.ts outDir = '../stackunderflow/static/react'
        #   We let nix build into dist/ and copy to $out.
        #   The npmDepsHash must be computed once; replace the placeholder
        #   with the real hash that nix reports on first build.
        frontend = pkgs.buildNpmPackage {
          pname = "stackunderflow-ui";
          version = "0.2.0";
          src = ./stackunderflow-ui;
          npmDepsHash = "sha256-0CvR9dNu+N2uGkCtnraVaY16mIyhJ5O7GMJaen9Tb0U=";

          installPhase = ''
            # vite.config.ts outDir = '../stackunderflow/static/react'
            # In the nix sandbox: source is at $PWD (e.g. /build/stackunderflow-ui/)
            # vite outputs to ./../stackunderflow/static/react/ = /build/stackunderflow/static/react/
            mkdir -p $out
            cp -r ../stackunderflow/static/react/* $out/
          '';
        };

        # ── Merged source tree (Python source + built frontend) ─────
        #   The server expects stackunderflow/static/react/index.html.
        #   This derivation combines the upstream source with the frontend
        #   build output into a single directory tree.
        srcWithFrontend = pkgs.runCommand "stackunderflow-src" { } ''
          cp -r ${pkgs.lib.cleanSource ./.} $out
          chmod -R u+w $out

          # Place built frontend where the Python server looks for it
          mkdir -p $out/stackunderflow/static/react
          cp -r ${frontend}/. $out/stackunderflow/static/react/
        '';

        # ── Python package ──────────────────────────────────────────
        stackunderflow-pkg = pp.buildPythonPackage {
          pname = "stackunderflow";
          version = "0.2.0";
          pyproject = true;
          src = srcWithFrontend;

          nativeBuildInputs = [ pp.hatchling pkgs.nodejs ];

          propagatedBuildInputs = with pp; [
            python-dotenv click fastapi uvicorn httpx
            python-multipart orjson uvloop
          ];

          doCheck = false; # tests use pytest separately

          meta = with pkgs.lib; {
            description = "A local-first knowledge base for your AI coding sessions";
            homepage = "https://github.com/0bserver07/StackUnderflow";
            license = licenses.mit;
            mainProgram = "stackunderflow";
          };
        };

      in
      rec {
        packages.default = stackunderflow-pkg;
        packages.stackunderflow = stackunderflow-pkg;
        packages.frontend = frontend;

        # Dev shell
        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.nodejs pkgs.python3
            pp.ruff pp.mypy
            pp.pytest pp.pytest-asyncio
            pp.pytest-cov pp.psutil
            pp.types-python-dateutil pp.types-psutil
            pkgs.rsync
          ];

          PYTHONPATH = "${toString ./.}";

          shellHook = ''
            echo ""
            echo "  StackUnderflow dev environment"
            echo "  Frontend → cd stackunderflow-ui && npm run dev"
            echo "  Backend  → python -m stackunderflow.cli init"
            echo "  Tests    → python -m pytest tests/stackunderflow/ -v"
            echo "  Lint     → bash lint.sh"
            echo ""
          '';
        };
      }
    );
}
