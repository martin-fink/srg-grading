{
  description = "Instructor scripts with isolated compilation and execution";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs =
    { self, nixpkgs }:
    {
      packages = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          scripts = pkgs.runCommand "exercise-grading-scripts" { } ''
            mkdir -p $out/private
            cp ${./public.py} $out/public.py
            cp ${./private.py} $out/private.py
            cp ${./run.py} $out/run.py
            cp ${./private/cases.json} $out/private/cases.json
          '';
          public = pkgs.writeShellScriptBin "grade-public" ''
            exec ${pkgs.python3}/bin/python3 ${scripts}/public.py
          '';
          private = pkgs.writeShellScriptBin "grade-private" ''
            exec ${pkgs.python3}/bin/python3 ${scripts}/private.py
          '';
        in
        {
          studentImage = pkgs.dockerTools.buildLayeredImage {
            name = "scripted-student";
            tag = "build";
            contents = [
              pkgs.bash
              pkgs.coreutils
              pkgs.gcc
            ];
            config.User = "10003:10003";
            config.Env = [ "PATH=/bin" ];
          };
          graderImage = pkgs.dockerTools.buildLayeredImage {
            name = "scripted-grader";
            tag = "build";
            contents = [
              public
              private
              pkgs.python3
            ];
            config.User = "10004:10004";
            config.Env = [ "PYTHONDONTWRITEBYTECODE=1" ];
          };
        }
      );
    };
}
